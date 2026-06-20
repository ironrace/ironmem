//! Bounded process execution for worker turns.
//!
//! `spawn_claude`/`spawn_codex` previously called `Command::output()`, which
//! blocks indefinitely: a single hung `claude -p` / `codex exec` worker (model
//! stuck in a loop, a wedged MCP call, a deadlocked cargo gate) stalled the whole
//! driver for hours. [`run_with_timeout`] bounds every turn to a wall-clock
//! deadline and, on expiry, kills the worker's entire **process group** — which
//! also reaps the per-turn `ironmem serve` MCP child the worker spawned (the
//! orphan-leak we observed) instead of stranding it.
//!
//! Implemented with std threads + a channel (no extra deps): reader threads drain
//! stdout/stderr concurrently (so a full pipe buffer can't deadlock the wait),
//! and a waiter thread observes child exit. On timeout we `kill(2)` the negative
//! pgid via `/bin/kill` to signal the whole group.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

/// Default per-turn wall-clock ceiling (30 min). A turn exceeding this is treated
/// as hung or too-large. Gate turns are incremental on a pre-warmed shared
/// `CARGO_TARGET_DIR`, but the **implement** turn is the model authoring code
/// (one validated run committed a change and was 100+ lines into a second edit
/// past the old 15-min cap, which killed live progress), so the ceiling must
/// clear a real implement turn while still reaping a genuine hang. Override with
/// `ABEVAL_TURN_TIMEOUT_SECS` (e.g. raise it if a cold first gate turn ever trips
/// the watchdog).
pub const DEFAULT_TURN_TIMEOUT_SECS: u64 = 1800;

/// Grace period between SIGTERM and SIGKILL when force-killing a timed-out group.
const KILL_GRACE: Duration = Duration::from_secs(3);

/// Resolve the per-turn timeout from `ABEVAL_TURN_TIMEOUT_SECS` (falling back to
/// [`DEFAULT_TURN_TIMEOUT_SECS`]). A `0` or unparseable value uses the default —
/// the watchdog can be tuned but not silently disabled.
pub fn turn_timeout() -> Duration {
    let secs = std::env::var("ABEVAL_TURN_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_TURN_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Captured result of a bounded process run.
#[derive(Debug)]
pub struct TimedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Run `cmd` to completion or `timeout`, whichever comes first. The child is
/// placed in its own process group so a timeout kill (`SIGTERM`, then `SIGKILL`
/// after [`KILL_GRACE`]) takes down the worker AND every MCP server it spawned.
///
/// stdin is `/dev/null`; stdout/stderr are captured. Returns `Err` on a timeout
/// (message names the elapsed ceiling) or a spawn failure.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<TimedOutput> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group (pgid == child pid) so we can signal the whole subtree.
    cmd.process_group(0);

    let mut child = cmd.spawn().context("spawning bounded worker process")?;
    let pid = child.id() as i32;

    // Drain pipes concurrently — a worker that fills the stdout buffer while we
    // wait would otherwise deadlock against our blocked `wait`. The reader threads
    // propagate the inner `io::Result` so a mid-stream pipe error or a panicked
    // reader becomes an `Err` on the success path rather than a silently truncated
    // (or empty) capture masquerading as a clean exit-0 turn.
    let mut out_pipe = child.stdout.take().expect("piped stdout");
    let mut err_pipe = child.stderr.take().expect("piped stderr");
    let out_reader = thread::spawn(move || -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        out_pipe.read_to_end(&mut buf)?;
        Ok(buf)
    });
    let err_reader = thread::spawn(move || -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        err_pipe.read_to_end(&mut buf)?;
        Ok(buf)
    });

    // Waiter thread owns the child; it reports exit (or wait error) over a channel
    // so the main thread can apply the timeout.
    let (tx, rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let status = child.wait();
        let _ = tx.send(status);
    });

    match rx.recv_timeout(timeout) {
        Ok(status) => package_success(status, waiter, out_reader, err_reader),
        Err(RecvTimeoutError::Timeout) => {
            // TOCTOU guard: the child may have exited naturally in the gap between
            // the timeout firing and our kill. If a status already landed, the pid
            // is (or is about to be) reaped and could be recycled — signaling the
            // group then risks hitting an unrelated process. Honor the natural exit.
            if let Ok(status) = rx.try_recv() {
                return package_success(status, waiter, out_reader, err_reader);
            }
            kill_group(pid);
            // The child is now dead; the waiter/readers unblock and we reap them.
            let _ = waiter.join();
            let _ = out_reader.join();
            let _ = err_reader.join();
            Err(anyhow!(
                "worker turn exceeded {}s wall-clock and was killed \
                 (hung worker, wedged MCP call, or task too large)",
                timeout.as_secs()
            ))
        }
        Err(RecvTimeoutError::Disconnected) => {
            // The waiter dropped `tx` without sending — it panicked inside
            // `child.wait()`. The child (and the MCP child it spawned) may still be
            // alive, so reap the whole group rather than stranding it, then join the
            // survivors. This is the one path that otherwise defeats the module's
            // stated purpose.
            kill_group(pid);
            let _ = waiter.join();
            let _ = out_reader.join();
            let _ = err_reader.join();
            Err(anyhow!(
                "bounded worker waiter thread disconnected unexpectedly"
            ))
        }
    }
}

/// Package a natural-exit `wait` status plus the drained pipes into a
/// [`TimedOutput`]. Propagates a pipe `io::Error` or a panicked reader thread as
/// `Err` (never a silently truncated/empty capture) and the `wait` error itself.
fn package_success(
    status: io::Result<ExitStatus>,
    waiter: thread::JoinHandle<()>,
    out_reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    err_reader: thread::JoinHandle<io::Result<Vec<u8>>>,
) -> Result<TimedOutput> {
    let status = status.context("waiting on bounded worker process")?;
    let _ = waiter.join();
    let stdout = join_reader(out_reader, "stdout")?;
    let stderr = join_reader(err_reader, "stderr")?;
    Ok(TimedOutput {
        status,
        stdout,
        stderr,
    })
}

/// Join a reader thread, surfacing both a panicked thread and an inner pipe
/// `io::Error` as `Err` so a partial/empty capture can never be packaged as a
/// clean success.
fn join_reader(handle: thread::JoinHandle<io::Result<Vec<u8>>>, which: &str) -> Result<Vec<u8>> {
    handle
        .join()
        .map_err(|_| anyhow!("bounded worker {which} reader thread panicked"))?
        .with_context(|| format!("reading bounded worker {which}"))
}

/// Signal an entire process group by pgid: `SIGTERM`, then `SIGKILL` after a
/// grace period. A negative target tells `kill(2)` to address the whole group
/// (the child was made group leader, so pgid == its pid).
fn kill_group(pgid: i32) {
    let target = format!("-{pgid}");
    // SIGTERM first (best-effort): a non-zero status here just means the group
    // already exited, the common case, so it stays silent.
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &target])
        .stderr(Stdio::null())
        .status();
    thread::sleep(KILL_GRACE);
    // SIGKILL is the last defense against a stranded `ironmem serve` MCP child or
    // a held DB lock. A spawn failure (missing `/bin/kill`) or a real kill error
    // (e.g. EPERM) must be surfaced so the leak is diagnosable instead of silent —
    // but a bare "No such process" just means SIGTERM already cleared the group,
    // which we still swallow.
    match Command::new("/bin/kill").args(["-KILL", &target]).output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let msg = String::from_utf8_lossy(&out.stderr);
            if !msg.to_ascii_lowercase().contains("no such process") {
                eprintln!(
                    "abeval: SIGKILL of worker group {target} failed: {}",
                    msg.trim()
                );
            }
        }
        Err(e) => {
            eprintln!("abeval: could not spawn /bin/kill to SIGKILL worker group {target}: {e}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RAII guard so an env-var test can't leak `ABEVAL_TURN_TIMEOUT_SECS` into a
    /// sibling test under parallel `cargo test`. Restores the original value (or
    /// removes it) on drop, regardless of intervening mutations.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, val);
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// `kill -0 <pid>` — true iff a process with that pid currently exists.
    fn pid_alive(pid: i32) -> bool {
        Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn completes_within_timeout_captures_stdout() {
        let mut cmd = Command::new("/bin/echo");
        cmd.arg("hello-abeval");
        let out = run_with_timeout(cmd, Duration::from_secs(10)).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello-abeval");
    }

    #[test]
    fn nonzero_exit_is_returned_not_errored() {
        // A worker that exits non-zero is a status to inspect, not a timeout.
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exit 7"]);
        let out = run_with_timeout(cmd, Duration::from_secs(10)).unwrap();
        assert_eq!(out.status.code(), Some(7));
    }

    #[test]
    fn kills_and_errors_when_turn_exceeds_timeout() {
        // `sleep 30` with a 300ms ceiling must be killed promptly (well under the
        // real 30-min default), not waited out.
        let start = std::time::Instant::now();
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30");
        let err = run_with_timeout(cmd, Duration::from_millis(300)).unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            err.to_string().contains("exceeded"),
            "timeout error must name the ceiling: {err}"
        );
        // KILL_GRACE (3s) + slack; must be far under the 30s sleep.
        assert!(
            elapsed < Duration::from_secs(8),
            "timed-out process should be killed promptly, took {elapsed:?}"
        );
    }

    #[test]
    fn turn_timeout_env_override_is_respected() {
        // RAII guard restores the original on drop, even across the mutations below.
        let _guard = EnvGuard::set("ABEVAL_TURN_TIMEOUT_SECS", "42");
        assert_eq!(turn_timeout(), Duration::from_secs(42));
        std::env::set_var("ABEVAL_TURN_TIMEOUT_SECS", "0");
        assert_eq!(
            turn_timeout(),
            Duration::from_secs(DEFAULT_TURN_TIMEOUT_SECS),
            "zero must not disable the watchdog"
        );
        std::env::set_var("ABEVAL_TURN_TIMEOUT_SECS", "not-a-number");
        assert_eq!(
            turn_timeout(),
            Duration::from_secs(DEFAULT_TURN_TIMEOUT_SECS),
            "non-numeric must not disable the watchdog"
        );
    }

    #[test]
    fn timeout_kills_the_whole_process_group_not_just_the_leader() {
        // The leader forks a `sleep 30` grandchild and records its pid, then waits.
        // If we only killed the leader (not its process group), the grandchild —
        // the stand-in for the orphaned `ironmem serve` MCP child this module
        // exists to reap — would survive. Confirm it's gone after the kill.
        let pidfile = std::env::temp_dir().join(format!(
            "abeval_pgtest_{}_{}.pid",
            std::process::id(),
            // distinguish from any sibling run; the leader pid is unique enough.
            "leader"
        ));
        let _ = std::fs::remove_file(&pidfile);
        let script = format!("sleep 30 & echo $! > '{}'; wait", pidfile.display());
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", &script]);
        let err = run_with_timeout(cmd, Duration::from_millis(500)).unwrap_err();
        assert!(
            err.to_string().contains("exceeded"),
            "must be a timeout: {err}"
        );

        let gpid: i32 = std::fs::read_to_string(&pidfile)
            .expect("leader should have written the grandchild pid")
            .trim()
            .parse()
            .expect("grandchild pid must parse");
        let _ = std::fs::remove_file(&pidfile);

        // The grandchild reparents to init on leader death; poll until init reaps it.
        let mut reaped = false;
        for _ in 0..40 {
            if !pid_alive(gpid) {
                reaped = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            reaped,
            "grandchild {gpid} survived — the process group was not reaped"
        );
    }

    #[test]
    fn timeout_escalates_to_sigkill_when_sigterm_is_trapped() {
        // The realistic wedged-worker case: a process that ignores SIGTERM. The
        // leader traps TERM and loops forever (its `sleep` children would die on
        // TERM, but the trapping shell keeps the group alive), so only the SIGKILL
        // escalation after KILL_GRACE can reap it.
        let start = std::time::Instant::now();
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "trap '' TERM; while :; do sleep 1; done"]);
        let err = run_with_timeout(cmd, Duration::from_millis(300)).unwrap_err();
        assert!(
            err.to_string().contains("exceeded"),
            "must be a timeout: {err}"
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed >= KILL_GRACE,
            "SIGTERM was trapped, so the grace period must elapse before SIGKILL: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "SIGKILL escalation should still be prompt: {elapsed:?}"
        );
    }

    #[test]
    fn large_stdout_capture_does_not_deadlock() {
        // The reader threads exist so a worker that fills the >64KB pipe buffer
        // can't deadlock our blocked `wait`. Emit ~512KB (well past one buffer) and
        // confirm the full capture survives intact.
        const BYTES: usize = 512 * 1024;
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", &format!("yes abeval | head -c {BYTES}")]);
        let out = run_with_timeout(cmd, Duration::from_secs(30)).unwrap();
        assert!(out.status.success(), "large emitter should exit 0");
        assert_eq!(
            out.stdout.len(),
            BYTES,
            "full large capture must survive without truncation or deadlock"
        );
    }
}

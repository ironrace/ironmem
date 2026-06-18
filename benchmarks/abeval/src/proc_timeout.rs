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

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

/// Default per-turn wall-clock ceiling (15 min). A turn exceeding this is treated
/// as hung or too-large: with a pre-warmed shared `CARGO_TARGET_DIR` every gate
/// pass is incremental, so no single legitimate collab turn should approach it.
/// Override with `ABEVAL_TURN_TIMEOUT_SECS` (e.g. raise it if a cold first gate
/// turn ever trips the watchdog).
pub const DEFAULT_TURN_TIMEOUT_SECS: u64 = 900;

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
    // wait would otherwise deadlock against our blocked `wait`.
    let mut out_pipe = child.stdout.take().expect("piped stdout");
    let mut err_pipe = child.stderr.take().expect("piped stderr");
    let out_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });

    // Waiter thread owns the child; it reports exit (or wait error) over a channel
    // so the main thread can apply the timeout.
    let (tx, rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let status = child.wait();
        let _ = tx.send(status);
    });

    match rx.recv_timeout(timeout) {
        Ok(status) => {
            let status = status.context("waiting on bounded worker process")?;
            let _ = waiter.join();
            let stdout = out_reader.join().unwrap_or_default();
            let stderr = err_reader.join().unwrap_or_default();
            Ok(TimedOutput {
                status,
                stdout,
                stderr,
            })
        }
        Err(RecvTimeoutError::Timeout) => {
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
            let _ = waiter.join();
            Err(anyhow!(
                "bounded worker waiter thread disconnected unexpectedly"
            ))
        }
    }
}

/// Signal an entire process group by pgid: `SIGTERM`, then `SIGKILL` after a
/// grace period. A negative target tells `kill(2)` to address the whole group
/// (the child was made group leader, so pgid == its pid).
fn kill_group(pgid: i32) {
    let target = format!("-{pgid}");
    // Best-effort; the follow-up SIGKILL commonly finds the group already gone
    // after SIGTERM, so silence `kill`'s "No such process" on stderr.
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &target])
        .stderr(Stdio::null())
        .status();
    thread::sleep(KILL_GRACE);
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &target])
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // real 15-min default), not waited out.
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
        // Use a guard so we don't leak the env var into other tests.
        std::env::set_var("ABEVAL_TURN_TIMEOUT_SECS", "42");
        assert_eq!(turn_timeout(), Duration::from_secs(42));
        std::env::set_var("ABEVAL_TURN_TIMEOUT_SECS", "0");
        assert_eq!(
            turn_timeout(),
            Duration::from_secs(DEFAULT_TURN_TIMEOUT_SECS),
            "zero must not disable the watchdog"
        );
        std::env::remove_var("ABEVAL_TURN_TIMEOUT_SECS");
    }
}

//! The IC dispatch primitive — build-ladder rung 2.
//!
//! Implements the spec's *IC lifecycle* CLI invocation: **one dispatch = one
//! process = N turns**. This module builds the argv for that invocation, runs
//! it, and parses the result JSON into a typed [`DispatchOutcome`] — nothing
//! more. Deciding what to *do* with an outcome (bank the cost, advance the
//! attempt counter, re-dispatch, escalate) is the Lead's job, built in later
//! rungs on top of rung 1's storage layer.
//!
//! # The 6a guard — never treat an unverified dispatch as "met"
//!
//! ⟨r6⟩ measured that the base result JSON carries no verdict field at all:
//! `terminal_reason`/`stop_reason`/`subtype`/`is_error` are identical between
//! a genuinely satisfied condition and one the evaluator judged impossible.
//! The mitigation is `--json-schema`, which forces a top-level
//! `structured_output.verdict`. [`parse_dispatch_output`] treats a missing,
//! malformed, or absent `structured_output` as `verdict: None` — see
//! [`DispatchOutcome::is_met`] — never as a silent success. This is the
//! mechanism the spec's Testing table calls out by name: "Dispatch invoked
//! without a valid `structured_output.verdict` ... is never recorded as met."
//!
//! # Caveat carried in from the design drawer
//!
//! Rung 0 validated `--json-schema` against two trivial single-turn probes,
//! and validated `--resume` against a real gate across two dispatches
//! *without* the schema flag. The combination this module actually builds —
//! schema-constrained, multi-turn, `--resume`-joined, against a real gate —
//! was not run end-to-end together before rung 2. See the rung-2 probe log
//! in the `autopilot-backlog-runner-spec` ironmem drawer for that
//! re-verification.

use std::path::Path;

use serde::Deserialize;

use crate::error::MemoryError;

use super::IssueRef;

/// The schema forced onto every IC dispatch via `--json-schema`, verbatim
/// from the spec's *turn-prompt template* section. **Must be passed as
/// inline JSON, not a file path** — rung 0 measured a file-path attempt
/// fail with "not valid JSON".
pub const IC_VERDICT_JSON_SCHEMA: &str = r#"{"type":"object","properties":{"verdict":{"type":"string","enum":["met","impossible","not_met"]},"reason":{"type":"string"}},"required":["verdict","reason"],"additionalProperties":false}"#;

/// Whether this invocation starts a new IC session or continues one across a
/// fresh process — the spec's "`--session-id` first dispatch only; `--resume`
/// thereafter".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMode {
    New { session_uuid: String },
    Resume { session_uuid: String },
}

impl SessionMode {
    fn session_uuid(&self) -> &str {
        match self {
            SessionMode::New { session_uuid } | SessionMode::Resume { session_uuid } => {
                session_uuid
            }
        }
    }
}

/// Everything [`build_argv`] needs to construct one dispatch invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchSpec {
    pub session: SessionMode,
    /// `--name`, giving the IC a deterministic address for `ListAgents` and
    /// the abort path. See [`ic_name`] for how this is derived from an issue.
    pub name: String,
    /// `--model`, routed by risk class — a later rung's concern; this module
    /// just carries whatever string the caller supplies.
    pub model: String,
    pub max_budget_usd: f64,
    pub max_turns: u32,
    /// The rendered `/goal` condition body from [`super::turn_prompt::render`]
    /// — everything after `/goal ` in the CLI invocation.
    pub condition: String,
    /// The repo's per-dispatch wall-clock bound (rung 7), or `None` for
    /// unbounded.
    ///
    /// The third bound alongside `max_budget_usd` (spend) and `max_turns`
    /// (iterations), and the only one of the three with **no CLI flag** —
    /// `claude` exposes no wall-clock ceiling, so this one is enforced by the
    /// supervisor around the process rather than by the process itself. It is
    /// deliberately part of the spec a caller builds, not a parameter of the
    /// runner, so the three bounds travel together and cannot be supplied
    /// from three different places.
    pub wall_clock_timeout: Option<std::time::Duration>,
}

/// `ic-<repo-slug>-<issue-number>`. The spec's own example (`ic-ironmem-283`)
/// uses a bare repo name; this uses [`IssueRef::slug`]'s full
/// `owner-repo-number` form instead, so two different repos with the same
/// short name never collide on the same IC address. A deliberate departure
/// from the spec's illustrative shorthand, not from its intent.
pub fn ic_name(issue: &IssueRef) -> String {
    format!("ic-{}", issue.slug())
}

/// Build the argv passed to the `claude` binary for one IC dispatch. Pure and
/// unit-testable without spawning a process — mirrors
/// `launcher::argv::build_args`'s pattern.
pub fn build_argv(spec: &DispatchSpec) -> Vec<String> {
    let mut args = vec!["-p".to_string(), format!("/goal {}", spec.condition)];

    let session_flag = match &spec.session {
        SessionMode::New { .. } => "--session-id",
        SessionMode::Resume { .. } => "--resume",
    };
    args.push(session_flag.to_string());
    args.push(spec.session.session_uuid().to_string());

    args.push("--output-format".to_string());
    args.push("json".to_string());
    args.push("--name".to_string());
    args.push(spec.name.clone());
    args.push("--model".to_string());
    args.push(spec.model.clone());
    args.push("--dangerously-skip-permissions".to_string());
    args.push("--max-budget-usd".to_string());
    args.push(spec.max_budget_usd.to_string());
    args.push("--max-turns".to_string());
    args.push(spec.max_turns.to_string());
    args.push("--json-schema".to_string());
    args.push(IC_VERDICT_JSON_SCHEMA.to_string());

    args
}

/// The evaluator's verdict, as forced through `--json-schema`. Serialized in
/// the schema's own lowercase spelling so a `--json` run report round-trips
/// the exact strings the enum was parsed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Met,
    Impossible,
    NotMet,
}

impl Verdict {
    fn from_schema_str(s: &str) -> Option<Self> {
        match s {
            "met" => Some(Verdict::Met),
            "impossible" => Some(Verdict::Impossible),
            "not_met" => Some(Verdict::NotMet),
            _ => None,
        }
    }
}

/// The base `--output-format json` result fields this module reads. Deriving
/// `Deserialize` on a struct (rather than hand-walking a `serde_json::Value`)
/// still tolerates unknown fields by default — Claude Code's result JSON
/// carries more than this (`usage`, `permission_denials`, ...), and adding a
/// field there must not break parsing here.
#[derive(Debug, Clone, Deserialize)]
struct RawDispatchResult {
    #[serde(default)]
    total_cost_usd: f64,
    #[serde(default)]
    num_turns: u32,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    session_id: String,
    /// Present only when `--json-schema` was honored. Kept as a raw
    /// [`serde_json::Value`] rather than a typed struct so a malformed or
    /// unexpected shape (e.g. `verdict` outside the enum) degrades to `None`
    /// in [`extract_verdict`] instead of failing the whole parse — the 6a
    /// guard's fail-closed behavior applies to shape errors too, not just
    /// absence.
    #[serde(default)]
    structured_output: Option<serde_json::Value>,
}

/// One dispatch's outcome: the exact meter and verdict the Lead reads to
/// decide what happens next.
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchOutcome {
    pub total_cost_usd: f64,
    pub num_turns: u32,
    pub duration_ms: u64,
    pub is_error: bool,
    pub session_id: String,
    /// `None` when `structured_output` was absent, malformed, or carried a
    /// `verdict` value outside the schema's enum — see [`Self::is_met`].
    pub verdict: Option<Verdict>,
    pub reason: Option<String>,
    /// Whether the *process* itself exited zero. `true` when this outcome
    /// came from [`parse_dispatch_output`] alone (no process context to
    /// distinguish); [`run_dispatch`] overwrites it with the real exit
    /// status. A process can flush a complete, schema-valid "met" result to
    /// stdout and then die for an unrelated reason (killed, crashed during
    /// cleanup) before exiting zero — [`Self::is_met`] refuses to trust that
    /// verdict, but the rest of the outcome (cost, turns, session id) stays
    /// available rather than being discarded, since the Lead still needs it
    /// for budget accounting even on a failed dispatch.
    pub process_success: bool,
    /// Whether this dispatch was killed for exceeding its repo's wall-clock
    /// bound (rung 7). A timed-out dispatch has **no result JSON at all** —
    /// the process never got to write one — so every other field here is a
    /// synthesized placeholder rather than a measurement, and
    /// `total_cost_usd` in particular is a `0.0` that means *unknown*, not
    /// *free*. [`run_dispatch`]'s caller is responsible for banking it to
    /// the ledger's unpriced counter rather than its total; see
    /// [`super::run::run_issue`].
    pub timed_out: bool,
}

impl DispatchOutcome {
    /// The 6a guard, in one place: a dispatch only counts as "met" when the
    /// schema-forced verdict says so, the invocation did not itself error,
    /// *and* the process exited zero. A dispatch with no verdict (schema not
    /// honored — e.g. an infrastructure failure mid-turn) or a non-zero exit
    /// (the JSON's own claims are untrustworthy once the process itself
    /// failed) is never treated as met, no matter what the JSON says on its
    /// own.
    pub fn is_met(&self) -> bool {
        self.process_success && !self.is_error && self.verdict == Some(Verdict::Met)
    }

    /// The evaluator judged the goal condition unsatisfiable. Per the spec's
    /// *Error handling* table this is "a normal-looking exit" that must be
    /// recorded as a failure, not a completion — callers should route this
    /// to a failed lineage attempt, never treat it as [`Self::is_met`].
    pub fn is_impossible(&self) -> bool {
        self.verdict == Some(Verdict::Impossible)
    }
}

fn extract_verdict(
    structured_output: &Option<serde_json::Value>,
) -> (Option<Verdict>, Option<String>) {
    let Some(value) = structured_output else {
        return (None, None);
    };
    let verdict = value
        .get("verdict")
        .and_then(|v| v.as_str())
        .and_then(Verdict::from_schema_str);
    let reason = value
        .get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    (verdict, reason)
}

/// Parse one dispatch's captured stdout into a [`DispatchOutcome`].
///
/// Only the outer JSON's validity is a hard error — a process that produced
/// no parseable result at all is an infrastructure failure the caller must
/// handle explicitly, not silently default. Everything *inside* that JSON
/// (missing `structured_output`, an out-of-enum `verdict`) degrades to `None`
/// rather than erroring, per the 6a guard.
pub fn parse_dispatch_output(stdout: &str) -> Result<DispatchOutcome, MemoryError> {
    let raw: RawDispatchResult = serde_json::from_str(stdout).map_err(|e| {
        MemoryError::Validation(format!(
            "IC dispatch produced no parseable result JSON: {e}"
        ))
    })?;
    let (verdict, reason) = extract_verdict(&raw.structured_output);
    Ok(DispatchOutcome {
        total_cost_usd: raw.total_cost_usd,
        num_turns: raw.num_turns,
        duration_ms: raw.duration_ms,
        is_error: raw.is_error,
        session_id: raw.session_id,
        verdict,
        reason,
        process_success: true,
        timed_out: false,
    })
}

/// Locate the `claude` binary on `PATH`, reusing `launcher`'s own binary
/// validation (the spec's *Reuse* section: "`launcher` validates the
/// assistant binary ... this is what makes a Codex reviewer nearly free").
pub fn resolve_claude_binary() -> Result<std::path::PathBuf, MemoryError> {
    crate::launcher::find_on_path(crate::launcher::Harness::Claude.binary())
}

/// Run one IC dispatch: spawn `bin` with `spec`'s argv, in `repo`, capturing
/// stdout (never inherited — this is a headless invocation, not an
/// interactive one; contrast `launcher::run_launcher`, which inherits stdio
/// for a human-attended session). Blocks until the dispatch's process exits,
/// per the spec's "one dispatch = one process" primitive.
///
/// A non-zero exit does **not** discard a successfully-parsed result — see
/// [`DispatchOutcome::process_success`] — because that data (cost, turns,
/// session id) is exactly what the Lead needs to bank spend and record a
/// lineage attempt even for a dispatch that failed, per the spec's error
/// table ("`--max-budget-usd` terminates it; the Lead treats it as a failed
/// attempt"). What it *does* discard is trust in the verdict:
/// [`DispatchOutcome::is_met`] refuses a "met" claim from a non-zero exit,
/// since a process can flush a complete, schema-valid result to stdout and
/// then die for an unrelated reason before exiting cleanly.
pub fn run_dispatch(
    bin: &Path,
    repo: &Path,
    spec: &DispatchSpec,
) -> Result<DispatchOutcome, MemoryError> {
    run_dispatch_bounded(bin, repo, spec, spec.wall_clock_timeout)
}

/// How often [`run_dispatch_bounded`] checks whether a bounded dispatch has
/// exited. A dispatch runs for minutes at least, so a one-second granularity
/// is far finer than any bound worth setting and costs one `waitpid` per
/// second of an otherwise-idle wait.
const TIMEOUT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// [`run_dispatch`], with the spec's open-question-7 wall-clock bound.
///
/// `timeout` is passed explicitly rather than read off the spec so this
/// function stays testable with a short bound against a stub binary;
/// [`run_dispatch`] supplies [`DispatchSpec::wall_clock_timeout`], which
/// comes from the repo's approved gate config
/// ([`super::gate_config::wall_clock_timeout`]) — per-repo, because gate
/// suites differ by orders of magnitude. `None` means unbounded, which is
/// what every rung before this one did unconditionally.
///
/// # What a timeout costs, stated rather than hidden
///
/// Killing the process forfeits its result JSON, and that JSON was the only
/// meter there is: the spec's own error table says a killed IC's tokens are
/// never recorded. So a timed-out dispatch really did spend money that
/// nothing can report. The outcome it synthesizes therefore carries
/// `timed_out: true` and a `total_cost_usd` of `0.0` that means **unknown**,
/// and [`super::run::run_issue`] banks it to the ledger's *unpriced* counter
/// — rung 5's `unpriced_dispatch_count`, which exists to mark a total as a
/// floor rather than a total. Recording `$0.00` as spend would make the
/// ledger quietly wrong in the one direction that matters.
///
/// The session is unaffected: `--resume` continues it from its last
/// checkpoint on the next dispatch, which is the spec's "IC process dies
/// mid-turn" row.
pub fn run_dispatch_bounded(
    bin: &Path,
    repo: &Path,
    spec: &DispatchSpec,
    timeout: Option<std::time::Duration>,
) -> Result<DispatchOutcome, MemoryError> {
    let args = build_argv(spec);
    let Some(timeout) = timeout else {
        let output = std::process::Command::new(bin)
            .args(&args)
            .current_dir(repo)
            .output()
            .map_err(|e| MemoryError::NotFound(format!("failed to launch IC dispatch: {e}")))?;
        return finish_dispatch(output);
    };

    let mut command = std::process::Command::new(bin);
    command
        .args(&args)
        .current_dir(repo)
        // Nulled explicitly because `spawn` inherits all three streams while
        // `output` — the unbounded path above — nulls stdin for you. Without
        // it a dispatch inherits the Lead's stdin: a TTY under an interactive
        // `autopilot lead`, whatever a cron hands it otherwise. A `claude`
        // that reads stdin would then either compete with the operator for the
        // terminal or block until the bound kills it, which is the wedge this
        // path exists to prevent, caused by the path that prevents it.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Put the dispatch in its own process group so the bound can reap what it
    // *started*, not just the `claude` process itself. An IC's whole job is
    // running the repo's gate — `cargo test --workspace`, `xcodebuild`, a
    // pytest suite — as child processes. `Child::kill` sends SIGKILL to the
    // direct child only, so those survive, keep running in the worktree, and
    // collide with the next dispatch that resumes there: the wedged-repo case
    // the bound exists to contain, made worse by the containment.
    //
    // Only on the bounded path. An unbounded dispatch is never killed, so it
    // has nothing to reap, and leaving its process-group behavior exactly as
    // rungs 2-6 shipped it keeps this change to the path that needs it.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|e| MemoryError::NotFound(format!("failed to launch IC dispatch: {e}")))?;

    // Drain both pipes for the whole life of the process, on their own
    // threads. Reading them only *after* it exits — what `wait_with_output`
    // does — deadlocks any dispatch whose output exceeds the OS pipe buffer
    // (~64 KiB): the child blocks writing, the poll loop below never sees it
    // exit, and the bound then kills a perfectly healthy dispatch and reports
    // it as timed out, forfeiting its work and its unrecorded spend. The
    // unbounded path's `Command::output` drains concurrently for exactly this
    // reason, and the two paths are supposed to differ only in how they wait.
    fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> DrainHandle {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    }
    let stdout_drain = drain(child.stdout.take());
    let stderr_drain = drain(child.stderr.take());

    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => {
                // We cannot tell whether it is running. Killing on that would
                // destroy a healthy dispatch, so leave it and report.
                return Err(MemoryError::Validation(format!(
                    "could not check on the IC dispatch process: {e}"
                )));
            }
        }
        if started.elapsed() >= timeout {
            // Best-effort kill, then reap. A `kill` that fails (the process
            // exited in the window between the check and the call) is not an
            // error — `wait` below settles what actually happened.
            // The drain threads are deliberately *not* joined here: a
            // descendant the killed process left behind can hold the write
            // end of a pipe open indefinitely, and joining would hang the
            // supervisor on the one path whose whole point is not to hang.
            // They exit on their own when the pipes finally close.
            kill_process_group(&mut child);
            let _ = child.wait();
            return Ok(DispatchOutcome {
                total_cost_usd: 0.0,
                num_turns: 0,
                duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                is_error: true,
                session_id: spec.session.session_uuid().to_string(),
                verdict: None,
                reason: Some(format!(
                    "dispatch exceeded this repo's wall-clock bound of {}s and was killed; \
                     its spend is unrecorded and the session resumes from its last checkpoint",
                    timeout.as_secs()
                )),
                process_success: false,
                timed_out: true,
            });
        }
        std::thread::sleep(TIMEOUT_POLL_INTERVAL);
    }

    let status = child
        .wait()
        .map_err(|e| MemoryError::Validation(format!("IC dispatch output was lost: {e}")))?;
    let output = std::process::Output {
        status,
        stdout: stdout_drain.join().unwrap_or_default(),
        stderr: stderr_drain.join().unwrap_or_default(),
    };
    finish_dispatch(output)
}

/// A background reader collecting one of a bounded dispatch's pipes.
type DrainHandle = std::thread::JoinHandle<Vec<u8>>;

/// SIGKILL the whole process group a bounded dispatch was spawned into, then
/// the child itself as a backstop.
///
/// The group call is what actually reaps the gate suite; `child.kill()` alone
/// leaves it running. Both are best-effort: a group that has already exited
/// returns `ESRCH`, which is the success case arriving as an error code.
///
/// On a non-unix target there are no process groups here, so this degrades to
/// exactly the previous behavior — the direct child only. Stated rather than
/// silently platform-dependent.
pub(super) fn kill_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // Negated pid addresses the group, which `process_group(0)` made
        // equal to the child's own pid at spawn.
        let pid = child.id() as i32;
        if pid > 0 {
            // SAFETY: `killpg` on a pid we spawned ourselves, with a signal
            // constant. It cannot address a group we did not create: the
            // child was spawned into its own new group, and its pid is still
            // reserved because we have not reaped it yet.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

/// Turn a completed process's output into an outcome. Shared by the bounded
/// and unbounded paths so they cannot drift apart on how a non-zero exit or
/// a non-UTF-8 stdout is treated.
fn finish_dispatch(output: std::process::Output) -> Result<DispatchOutcome, MemoryError> {
    let describe_failure = |detail: String| {
        MemoryError::Validation(format!(
            "{detail} (exit status: {:?}, stderr: {})",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    };
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|e| describe_failure(format!("IC dispatch produced non-UTF-8 stdout: {e}")))?;
    let mut outcome = parse_dispatch_output(stdout).map_err(|e| describe_failure(e.to_string()))?;
    outcome.process_success = output.status.success();
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_spec(session: SessionMode) -> DispatchSpec {
        DispatchSpec {
            session,
            name: "ic-ironmem-283".to_string(),
            model: "claude-sonnet-5".to_string(),
            max_budget_usd: 2.5,
            max_turns: 12,
            condition: "make cargo test pass or stop after 6 turns".to_string(),
            wall_clock_timeout: None,
        }
    }

    // ── argv construction ───────────────────────────────────────────────

    /// Write an executable stub script and return its path. Used to exercise
    /// the real spawn/kill path without spawning `claude` — which costs money
    /// and, at the wall-clock bound, would have to be killed mid-work.
    #[cfg(unix)]
    fn stub_binary(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    // ── rung 7: the wall-clock bound ────────────────────────────────────

    #[test]
    fn the_wall_clock_bound_is_not_a_cli_flag() {
        // `claude` exposes no wall-clock ceiling, which is why the supervisor
        // enforces it around the process. If a flag ever appeared in argv it
        // would be an invented one, and the CLI would reject the dispatch.
        let mut spec = sample_spec(SessionMode::New {
            session_uuid: "u".to_string(),
        });
        spec.wall_clock_timeout = Some(std::time::Duration::from_secs(900));
        let args = build_argv(&spec);
        assert!(!args.iter().any(|a| a.contains("wall")));
        assert!(!args.iter().any(|a| a.contains("timeout")));
        assert!(!args.contains(&"900".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn a_dispatch_that_overruns_its_bound_is_killed_and_reported_as_timed_out() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub_binary(dir.path(), "slow", "sleep 30");
        let spec = sample_spec(SessionMode::New {
            session_uuid: "abc".to_string(),
        });

        let started = std::time::Instant::now();
        let outcome = run_dispatch_bounded(
            &bin,
            dir.path(),
            &spec,
            Some(std::time::Duration::from_secs(1)),
        )
        .unwrap();

        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the bound must actually kill the process, not wait it out"
        );
        assert!(outcome.timed_out);
        assert!(!outcome.process_success);
        assert!(outcome.is_error);
        assert_eq!(outcome.verdict, None);
        assert_eq!(
            outcome.total_cost_usd, 0.0,
            "a killed dispatch has no meter; this 0.0 means unknown and the caller banks it \
             as unpriced"
        );
        assert_eq!(
            outcome.session_id, "abc",
            "the session id must survive so the next dispatch can --resume it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dispatch_that_finishes_inside_its_bound_is_parsed_normally() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"total_cost_usd":0.25,"num_turns":4,"duration_ms":900,"is_error":false,"session_id":"abc","structured_output":{"verdict":"met","reason":"gate green"}}"#;
        let bin = stub_binary(dir.path(), "fast", &format!("cat <<'EOF'\n{json}\nEOF"));
        let spec = sample_spec(SessionMode::New {
            session_uuid: "abc".to_string(),
        });

        let outcome = run_dispatch_bounded(
            &bin,
            dir.path(),
            &spec,
            Some(std::time::Duration::from_secs(30)),
        )
        .unwrap();

        assert!(!outcome.timed_out);
        assert!(outcome.is_met());
        assert_eq!(outcome.num_turns, 4);
    }

    #[cfg(unix)]
    #[test]
    fn a_dispatch_that_outruns_the_pipe_buffer_is_not_killed_as_a_timeout() {
        // 256 KiB of stderr, four times the usual 64 KiB pipe buffer. A
        // bounded dispatch that reads its pipes only after the process exits
        // deadlocks here: the child blocks writing, never exits, and the
        // bound kills a healthy dispatch and reports it as timed out.
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"total_cost_usd":0.3,"num_turns":1,"duration_ms":7,"is_error":false,"session_id":"s","structured_output":{"verdict":"met"}}"#;
        let bin = stub_binary(
            dir.path(),
            "chatty",
            &format!("yes x | head -c 262144 >&2\ncat <<'EOF'\n{json}\nEOF"),
        );
        let spec = sample_spec(SessionMode::New {
            session_uuid: "s".to_string(),
        });

        let outcome = run_dispatch_bounded(
            &bin,
            dir.path(),
            &spec,
            Some(std::time::Duration::from_secs(10)),
        )
        .unwrap();

        assert!(
            !outcome.timed_out,
            "a chatty dispatch must not be mistaken for a wedged one"
        );
        assert!(outcome.is_met());
    }

    #[cfg(unix)]
    #[test]
    fn the_timeout_reaps_the_gate_suite_the_dispatch_started() {
        // An IC's whole job is running the repo's gate as child processes. A
        // kill that reaps only `claude` leaves them running in the worktree,
        // where they collide with the next dispatch that resumes there — the
        // wedged-repo case the bound exists to contain.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("grandchild.pid");
        // The stub spawns a long-lived "gate suite", records its pid, and
        // then hangs — exactly the shape the bound has to clean up after.
        let bin = stub_binary(
            dir.path(),
            "spawns",
            &format!("sleep 120 &\necho $! > {}\nsleep 120", marker.display()),
        );
        let spec = sample_spec(SessionMode::New {
            session_uuid: "s".to_string(),
        });

        let outcome = run_dispatch_bounded(
            &bin,
            dir.path(),
            &spec,
            Some(std::time::Duration::from_secs(2)),
        )
        .unwrap();
        assert!(outcome.timed_out);

        let pid: i32 = std::fs::read_to_string(&marker)
            .expect("the stub must have recorded its gate-suite pid")
            .trim()
            .parse()
            .unwrap();

        // Signal 0 probes for existence without delivering anything. Give
        // the kernel a moment to finish tearing the group down first.
        std::thread::sleep(std::time::Duration::from_millis(300));
        // SAFETY: `kill` with signal 0 performs an existence/permission check
        // and delivers no signal.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if alive {
            // Do not leave a stray `sleep 120` behind if the assertion fails.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(
            !alive,
            "the gate suite (pid {pid}) survived the wall-clock kill"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_bounded_and_unbounded_paths_parse_identically() {
        // The two paths differ only in how they wait. A result that parses
        // one way and not the other would mean the bound silently changed
        // what a dispatch reports.
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"total_cost_usd":0.11,"num_turns":2,"duration_ms":5,"is_error":false,"session_id":"s","structured_output":{"verdict":"not_met"}}"#;
        let bin = stub_binary(dir.path(), "same", &format!("cat <<'EOF'\n{json}\nEOF"));
        let spec = sample_spec(SessionMode::New {
            session_uuid: "s".to_string(),
        });

        let unbounded = run_dispatch_bounded(&bin, dir.path(), &spec, None).unwrap();
        let bounded = run_dispatch_bounded(
            &bin,
            dir.path(),
            &spec,
            Some(std::time::Duration::from_secs(30)),
        )
        .unwrap();
        assert_eq!(unbounded, bounded);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_zero_exit_inside_the_bound_still_keeps_its_parsed_meter() {
        // Rung 2's rule, re-checked on the bounded path: a non-zero exit
        // discards trust in the verdict, never the accounting.
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"total_cost_usd":0.42,"num_turns":6,"duration_ms":80,"is_error":false,"session_id":"s","structured_output":{"verdict":"met"}}"#;
        let bin = stub_binary(
            dir.path(),
            "dies",
            &format!("cat <<'EOF'\n{json}\nEOF\nexit 3"),
        );
        let spec = sample_spec(SessionMode::New {
            session_uuid: "s".to_string(),
        });

        let outcome = run_dispatch_bounded(
            &bin,
            dir.path(),
            &spec,
            Some(std::time::Duration::from_secs(30)),
        )
        .unwrap();
        assert!(!outcome.timed_out);
        assert!(!outcome.process_success);
        assert!(!outcome.is_met(), "a non-zero exit's 'met' is not believed");
        assert!((outcome.total_cost_usd - 0.42).abs() < 1e-9);
    }

    #[test]
    fn first_dispatch_uses_session_id_not_resume() {
        let spec = sample_spec(SessionMode::New {
            session_uuid: "11111111-1111-1111-1111-111111111111".to_string(),
        });
        let args = build_argv(&spec);
        assert!(args.contains(&"--session-id".to_string()));
        assert!(!args.contains(&"--resume".to_string()));
        assert!(args.contains(&"11111111-1111-1111-1111-111111111111".to_string()));
    }

    #[test]
    fn later_dispatch_uses_resume_not_session_id() {
        let spec = sample_spec(SessionMode::Resume {
            session_uuid: "22222222-2222-2222-2222-222222222222".to_string(),
        });
        let args = build_argv(&spec);
        assert!(args.contains(&"--resume".to_string()));
        assert!(!args.contains(&"--session-id".to_string()));
    }

    #[test]
    fn prompt_is_the_condition_prefixed_with_goal() {
        let spec = sample_spec(SessionMode::New {
            session_uuid: "u".to_string(),
        });
        let args = build_argv(&spec);
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], "/goal make cargo test pass or stop after 6 turns");
    }

    #[test]
    fn every_required_flag_is_present_with_its_value() {
        let spec = sample_spec(SessionMode::New {
            session_uuid: "u".to_string(),
        });
        let args = build_argv(&spec);
        let value_after = |flag: &str| -> &str {
            let idx = args.iter().position(|a| a == flag).unwrap_or_else(|| {
                panic!("missing flag {flag} in argv: {args:?}");
            });
            &args[idx + 1]
        };
        assert_eq!(value_after("--output-format"), "json");
        assert_eq!(value_after("--name"), "ic-ironmem-283");
        assert_eq!(value_after("--model"), "claude-sonnet-5");
        assert_eq!(value_after("--max-budget-usd"), "2.5");
        assert_eq!(value_after("--max-turns"), "12");
        assert_eq!(value_after("--json-schema"), IC_VERDICT_JSON_SCHEMA);
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn json_schema_is_passed_inline_never_as_a_file_path() {
        // Regression guard for rung 0's measured failure: a file-path
        // `--json-schema` argument fails with "not valid JSON". The value
        // here must parse as JSON on its own.
        let spec = sample_spec(SessionMode::New {
            session_uuid: "u".to_string(),
        });
        let args = build_argv(&spec);
        let idx = args.iter().position(|a| a == "--json-schema").unwrap();
        let value = &args[idx + 1];
        assert!(
            !value.starts_with('/'),
            "schema value looks like a path: {value}"
        );
        assert!(serde_json::from_str::<serde_json::Value>(value).is_ok());
    }

    #[test]
    fn ic_name_is_derived_from_the_full_issue_slug() {
        let issue = IssueRef::new("ironrace/ironmem", 283);
        assert_eq!(ic_name(&issue), "ic-ironrace-ironmem-283");
    }

    #[test]
    fn session_mode_carries_the_same_uuid_either_way() {
        assert_eq!(
            SessionMode::New {
                session_uuid: "x".into()
            }
            .session_uuid(),
            "x"
        );
        assert_eq!(
            SessionMode::Resume {
                session_uuid: "x".into()
            }
            .session_uuid(),
            "x"
        );
    }

    // ── result parsing: the 6a guard ────────────────────────────────────

    #[test]
    fn a_verdict_of_met_with_the_schema_honored_is_recorded_as_met() {
        let stdout = r#"{"total_cost_usd":0.187,"num_turns":5,"duration_ms":12000,
            "is_error":false,"session_id":"s1",
            "structured_output":{"verdict":"met","reason":"gate passed"}}"#;
        let outcome = parse_dispatch_output(stdout).unwrap();
        assert!(outcome.is_met());
        assert!(!outcome.is_impossible());
        assert_eq!(outcome.reason.as_deref(), Some("gate passed"));
    }

    #[test]
    fn a_verdict_of_impossible_is_recorded_as_a_failure_not_a_completion() {
        // Spec's Testing table: "An evaluator verdict of impossible is
        // recorded as a failure, not a completion" — the normal-looking-exit
        // trap 6a exists to close.
        let stdout = r#"{"total_cost_usd":0.05,"num_turns":1,"duration_ms":3000,
            "is_error":false,"session_id":"s1",
            "structured_output":{"verdict":"impossible","reason":"cannot satisfy"}}"#;
        let outcome = parse_dispatch_output(stdout).unwrap();
        assert!(!outcome.is_met());
        assert!(outcome.is_impossible());
    }

    #[test]
    fn missing_structured_output_is_never_recorded_as_met() {
        // The exact scenario the base result JSON gave rung 0 no signal for:
        // terminal_reason/stop_reason/subtype/is_error alone cannot
        // distinguish met from impossible, so absence of structured_output
        // must never default to met.
        let stdout = r#"{"total_cost_usd":0.1,"num_turns":3,"duration_ms":5000,
            "is_error":false,"session_id":"s1"}"#;
        let outcome = parse_dispatch_output(stdout).unwrap();
        assert_eq!(outcome.verdict, None);
        assert!(!outcome.is_met());
        assert!(!outcome.is_impossible());
    }

    #[test]
    fn malformed_structured_output_shape_is_never_recorded_as_met() {
        let stdout = r#"{"total_cost_usd":0.1,"num_turns":3,"duration_ms":5000,
            "is_error":false,"session_id":"s1",
            "structured_output":{"unexpected":"shape"}}"#;
        let outcome = parse_dispatch_output(stdout).unwrap();
        assert_eq!(outcome.verdict, None);
        assert!(!outcome.is_met());
    }

    #[test]
    fn verdict_outside_the_enum_is_never_recorded_as_met() {
        let stdout = r#"{"total_cost_usd":0.1,"num_turns":3,"duration_ms":5000,
            "is_error":false,"session_id":"s1",
            "structured_output":{"verdict":"maybe","reason":"unsure"}}"#;
        let outcome = parse_dispatch_output(stdout).unwrap();
        assert_eq!(outcome.verdict, None);
        assert!(!outcome.is_met());
    }

    #[test]
    fn is_error_true_overrides_a_met_verdict() {
        // Defensive: even if a future CLI version somehow paired is_error
        // with a met verdict, is_met() must still fail closed.
        let stdout = r#"{"total_cost_usd":0.1,"num_turns":1,"duration_ms":1000,
            "is_error":true,"session_id":"s1",
            "structured_output":{"verdict":"met","reason":"gate passed"}}"#;
        let outcome = parse_dispatch_output(stdout).unwrap();
        assert!(!outcome.is_met());
    }

    #[test]
    fn unparseable_stdout_is_a_hard_error() {
        assert!(parse_dispatch_output("not json at all").is_err());
        assert!(parse_dispatch_output("").is_err());
    }

    #[test]
    fn unknown_extra_fields_in_the_result_json_do_not_break_parsing() {
        // Claude's real result JSON carries fields this module doesn't model
        // (usage, permission_denials, ...) — adding one there must not break
        // this parser.
        let stdout = r#"{"total_cost_usd":0.1,"num_turns":1,"duration_ms":1000,
            "is_error":false,"session_id":"s1","usage":{"input_tokens":100},
            "permission_denials":[],
            "structured_output":{"verdict":"not_met","reason":"still working"}}"#;
        let outcome = parse_dispatch_output(stdout).unwrap();
        assert_eq!(outcome.verdict, Some(Verdict::NotMet));
    }

    // ── run_dispatch against a fake binary (no real claude invocation,
    // no network, no spend) ─────────────────────────────────────────────

    #[cfg(unix)]
    fn write_fake_claude(dir: &std::path::Path, stdout_json: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-claude.sh");
        std::fs::write(
            &path,
            format!("#!/bin/sh\ncat <<'EOF'\n{stdout_json}\nEOF\n"),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn run_dispatch_parses_a_real_subprocess_invocation() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_fake_claude(
            dir.path(),
            r#"{"total_cost_usd":0.03,"num_turns":1,"duration_ms":900,
                "is_error":false,"session_id":"s-fake",
                "structured_output":{"verdict":"met","reason":"done"}}"#,
        );
        let spec = sample_spec(SessionMode::New {
            session_uuid: "u".to_string(),
        });
        let outcome = run_dispatch(&bin, dir.path(), &spec).unwrap();
        assert!(outcome.is_met());
        assert_eq!(outcome.session_id, "s-fake");
        assert!((outcome.total_cost_usd - 0.03).abs() < 1e-9);
    }

    #[cfg(unix)]
    #[test]
    fn run_dispatch_surfaces_stderr_and_exit_code_on_a_parse_failure() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake-claude-broken.sh");
        std::fs::write(&path, "#!/bin/sh\necho 'not json' >&2\nexit 3\n").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        let spec = sample_spec(SessionMode::New {
            session_uuid: "u".to_string(),
        });
        let err = run_dispatch(&path, dir.path(), &spec).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no parseable result JSON"), "got: {msg}");
        assert!(msg.contains("not json"), "got: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn run_dispatch_rejects_a_met_verdict_from_a_failed_process_but_keeps_the_cost() {
        // A crash or a killed-mid-cleanup process can still flush a complete,
        // schema-valid "met" result to stdout before dying — exit status is
        // the only signal that catches this, since the JSON itself parses
        // fine and looks trustworthy. Mirrors launcher::run_launcher's
        // status.success() check for trust, but — unlike a plain launcher
        // failure — the parsed cost/turns/session data must still reach the
        // caller: the Lead needs it to bank spend and record a failed
        // attempt, per the spec's `--max-budget-usd` error-handling entry.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake-claude-crashes-after-flushing.sh");
        std::fs::write(
            &path,
            r#"#!/bin/sh
cat <<'EOF'
{"total_cost_usd":0.03,"num_turns":1,"duration_ms":900,
 "is_error":false,"session_id":"s-fake",
 "structured_output":{"verdict":"met","reason":"done"}}
EOF
exit 1
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        let spec = sample_spec(SessionMode::New {
            session_uuid: "u".to_string(),
        });
        let outcome = run_dispatch(&path, dir.path(), &spec).unwrap();
        assert!(
            !outcome.is_met(),
            "a non-zero exit must never be trusted as met, even with a schema-valid verdict"
        );
        assert!(!outcome.process_success);
        assert_eq!(outcome.verdict, Some(Verdict::Met));
        assert!(
            (outcome.total_cost_usd - 0.03).abs() < 1e-9,
            "cost must still be recoverable from a failed-process dispatch, not discarded"
        );
        assert_eq!(outcome.session_id, "s-fake");
    }
}

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

/// The evaluator's verdict, as forced through `--json-schema`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    let args = build_argv(spec);
    let output = std::process::Command::new(bin)
        .args(&args)
        .current_dir(repo)
        .output()
        .map_err(|e| MemoryError::NotFound(format!("failed to launch IC dispatch: {e}")))?;
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
        }
    }

    // ── argv construction ───────────────────────────────────────────────

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

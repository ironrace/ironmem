//! The Codex `exec` harness for supervised dispatch: its verified argv and
//! its JSONL journal and final-verdict parsing. Supervision itself lives in
//! [`super::invocation`].
use std::path::Path;

use crate::autopilot::dispatch::{DispatchSpec, SessionMode};
use crate::autopilot::implementer::Implementer;
use crate::autopilot::invocation::{
    validate_spec, validation, FinalVerdict, InvocationArtifacts, ParsedRun, SupervisedCli,
};
use crate::error::MemoryError;

pub use crate::autopilot::invocation::VERDICT_JSON_SCHEMA as CODEX_VERDICT_JSON_SCHEMA;
pub const CODEX_DISPATCH_TIMEOUT_SECS: u64 = super::invocation::SUPERVISED_DISPATCH_TIMEOUT_SECS;

/// Codex CLI. It assigns its own thread ID, reported by `thread.started`, so
/// only a resume names the native ID the run must report.
pub struct CodexCli;

impl SupervisedCli for CodexCli {
    fn implementer(&self) -> Implementer {
        Implementer::Codex
    }

    /// argv verified against codex-cli 0.163.0. Resume has no `-C`; cwd is set
    /// by the supervisor. Controller UUIDs and Claude-specific limits stay out.
    fn argv(
        &self,
        repo: &Path,
        spec: &DispatchSpec,
        a: &InvocationArtifacts,
    ) -> Result<Vec<String>, MemoryError> {
        if let SessionMode::Resume { session_uuid } = &spec.session {
            uuid::Uuid::parse_str(session_uuid).map_err(|_| {
                validation("Codex resume requires the native UUID from thread.started.thread_id")
            })?;
        }
        let mut args = vec!["exec".into()];
        if matches!(spec.session, SessionMode::Resume { .. }) {
            args.push("resume".into());
        }
        args.extend([
            "--json".into(),
            "--output-schema".into(),
            a.schema_path.clone(),
            "-o".into(),
            a.final_path.clone(),
            "--dangerously-bypass-approvals-and-sandbox".into(),
            "--dangerously-bypass-hook-trust".into(),
        ]);
        if matches!(spec.session, SessionMode::New { .. }) {
            args.extend([
                "-C".into(),
                repo.to_str()
                    .ok_or_else(|| validation("Codex repository path must be UTF-8"))?
                    .into(),
            ]);
        }
        args.extend(["-m".into(), spec.model.clone()]);
        if let SessionMode::Resume { session_uuid } = &spec.session {
            args.push(session_uuid.clone());
        }
        args.push(spec.condition.clone());
        Ok(args)
    }

    fn expected_native_id(&self, spec: &DispatchSpec) -> Option<String> {
        match &spec.session {
            SessionMode::Resume { session_uuid } => Some(session_uuid.clone()),
            SessionMode::New { .. } => None,
        }
    }

    /// The journal must name one unique `thread.started` UUID. The resume
    /// mismatch is checked against the exit record by the shared reader.
    fn parse(
        &self,
        journal: &str,
        final_text: Result<String, MemoryError>,
        _expected_native_id: Option<&str>,
    ) -> ParsedRun {
        let mut errors = Vec::new();
        let mut native_id = None;
        let mut conflict = false;
        let mut turns = 0u32;
        let mut observed_output = !journal.is_empty();
        for line in journal.lines().filter(|line| !line.trim().is_empty()) {
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(value) if value.is_object() => {
                    match value.get("type").and_then(|v| v.as_str()) {
                        Some("thread.started") => {
                            if let Some(id) = value
                                .get("thread_id")
                                .and_then(|v| v.as_str())
                                .filter(|s| uuid::Uuid::parse_str(s).is_ok())
                            {
                                if native_id.as_deref().is_some_and(|prior| prior != id) {
                                    conflict = true;
                                    errors.push("conflicting native Codex thread IDs".into());
                                } else {
                                    native_id = Some(id.to_owned());
                                }
                            } else {
                                errors.push("invalid native Codex thread ID".into());
                            }
                        }
                        Some("turn.completed") => turns = turns.saturating_add(1),
                        _ => {}
                    }
                }
                _ => errors.push("malformed Codex JSONL journal".into()),
            }
        }
        if conflict {
            native_id = None;
        }
        if native_id.is_none() {
            errors.push("missing unique native Codex thread.started ID".into());
        }
        let verdict = match final_text.and_then(|text| {
            observed_output |= !text.is_empty();
            serde_json::from_str::<FinalVerdict>(&text)
                .map_err(|e| validation(format!("invalid Codex final verdict: {e}")))
        }) {
            Ok(verdict) => Some(verdict),
            Err(e) => {
                errors.push(e.to_string());
                None
            }
        };
        ParsedRun {
            native_id,
            verdict,
            turns,
            errors,
            observed_output,
        }
    }
}

/// The exact Codex argv for `spec`, refusing a spec for another harness.
pub fn build_argv(repo: &Path, spec: &DispatchSpec) -> Result<Vec<String>, MemoryError> {
    let a = validate_spec(&CodexCli, spec)?;
    CodexCli.argv(repo, spec, a)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use super::{build_argv, CODEX_VERDICT_JSON_SCHEMA};
    use crate::autopilot::dispatch::{DispatchSpec, InvocationArtifacts, SessionMode};
    use crate::autopilot::implementer::Implementer;

    fn artifacts() -> InvocationArtifacts {
        InvocationArtifacts {
            invocation_id: "invocation".into(),
            schema_path: "/tmp/schema.json".into(),
            journal_path: "/tmp/journal.jsonl".into(),
            final_path: "/tmp/final.json".into(),
            stderr_path: "/tmp/stderr.log".into(),
            exit_path: "/tmp/exit.json".into(),
        }
    }

    pub(crate) fn spec(session: SessionMode) -> DispatchSpec {
        DispatchSpec {
            implementer: Implementer::Codex,
            invocation_artifacts: Some(artifacts()),
            session,
            name: "unused-by-codex".into(),
            model: "gpt-test".into(),
            max_budget_usd: 1.0,
            max_turns: 2,
            condition: "make the gate pass".into(),
            wall_clock_timeout: None,
        }
    }

    #[test]
    fn codex_schema_is_the_exact_strict_verdict_contract() {
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(CODEX_VERDICT_JSON_SCHEMA).unwrap(),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "verdict": {"type": "string", "enum": ["met", "impossible", "not_met"]},
                    "reason": {"type": "string"}
                },
                "required": ["verdict", "reason"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn new_invocation_uses_verified_codex_exec_arguments() {
        let args = build_argv(
            Path::new("/repo"),
            &spec(SessionMode::New {
                session_uuid: "ignored-controller-id".into(),
            }),
        )
        .unwrap();
        assert_eq!(
            args,
            [
                "exec",
                "--json",
                "--output-schema",
                "/tmp/schema.json",
                "-o",
                "/tmp/final.json",
                "--dangerously-bypass-approvals-and-sandbox",
                "--dangerously-bypass-hook-trust",
                "-C",
                "/repo",
                "-m",
                "gpt-test",
                "make the gate pass"
            ]
        );
    }

    #[test]
    fn resume_uses_native_thread_id_and_omits_c_and_goal_prefix() {
        let args = build_argv(
            Path::new("/repo"),
            &spec(SessionMode::Resume {
                session_uuid: "11111111-2222-3333-4444-555555555555".into(),
            }),
        )
        .unwrap();
        assert_eq!(
            args,
            [
                "exec",
                "resume",
                "--json",
                "--output-schema",
                "/tmp/schema.json",
                "-o",
                "/tmp/final.json",
                "--dangerously-bypass-approvals-and-sandbox",
                "--dangerously-bypass-hook-trust",
                "-m",
                "gpt-test",
                "11111111-2222-3333-4444-555555555555",
                "make the gate pass"
            ]
        );
    }
}

#[cfg(test)]
mod contract_tests {
    use crate::autopilot::dispatch::Verdict;
    use crate::autopilot::invocation::contract_tests::{completed, journal, setup, THREAD};

    #[test]
    fn strict_final_and_native_identity_and_supervisor_exit_are_all_required() {
        for verdict in ["met", "impossible", "not_met"] {
            let (_dir, s) = setup();
            let o = completed(
                &s,
                &format!("{{\"verdict\":\"{verdict}\",\"reason\":\"gate\"}}"),
                &journal(),
                Some(0),
            );
            assert_eq!(o.is_met(), verdict == "met");
            assert_eq!(o.native_session_id.as_deref(), Some(THREAD));
            assert_eq!(o.num_turns, 1);
            assert_eq!(o.cost_usd, None);
            assert_eq!(
                o.verdict,
                Some(match verdict {
                    "met" => Verdict::Met,
                    "impossible" => Verdict::Impossible,
                    _ => Verdict::NotMet,
                })
            );
        }
        for final_json in [
            "",
            "{",
            "{}",
            r#"{"verdict":"met"}"#,
            r#"{"verdict":"unknown","reason":"x"}"#,
            r#"{"verdict":"met","reason":"x","extra":1}"#,
        ] {
            let (_dir, s) = setup();
            assert!(
                !completed(&s, final_json, &journal(), Some(0)).is_met(),
                "{final_json}"
            );
        }
        for log in ["".into(), "{bad\n".into(), "{\"type\":\"thread.started\",\"thread_id\":\"\"}\n".into(), "{\"type\":\"thread.started\",\"thread_id\":\"not-uuid\"}\n".into(), format!("{}{{\"type\":\"thread.started\",\"thread_id\":\"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\"}}\n", journal())] {
            let (_dir, s) = setup();
            assert!(!completed(&s, r#"{"verdict":"met","reason":"gate"}"#, &log, Some(0)).is_met(), "{log}");
        }
        for code in [None, Some(1)] {
            let (_dir, s) = setup();
            let o = completed(&s, r#"{"verdict":"met","reason":"gate"}"#, &journal(), code);
            assert!(!o.is_met());
            assert_eq!(o.native_session_id.as_deref(), Some(THREAD));
        }
    }
    #[test]
    fn truncated_journal_retains_unique_native_id_without_trusting_verdict() {
        let (_dir, s) = setup();
        let o = completed(
            &s,
            r#"{"verdict":"met","reason":"gate"}"#,
            &format!("{}{{broken", journal()),
            Some(0),
        );
        assert_eq!(o.native_session_id.as_deref(), Some(THREAD));
        assert!(!o.is_met());
        assert_eq!(o.verdict, None);
    }
}

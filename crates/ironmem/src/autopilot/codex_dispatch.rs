//! The Codex `exec` harness for supervised dispatch: its verified argv and
//! its JSONL journal and final-verdict parsing. Supervision itself lives in
//! [`super::invocation`].
use std::path::{Path, PathBuf};

use crate::autopilot::dispatch::{DispatchSpec, SessionMode};
use crate::autopilot::implementer::Implementer;
use crate::autopilot::invocation::{
    validation, FinalVerdict, InvocationArtifacts, ParsedRun, SpawnFile, SupervisedCli,
};
use crate::error::MemoryError;

/// Renamed to [`InvocationArtifacts`] and moved to [`super::invocation`];
/// kept here, deprecated, for one release.
#[deprecated(note = "renamed to `invocation::InvocationArtifacts`")]
pub type CodexArtifacts = InvocationArtifacts;
/// Renamed to [`super::invocation::InvocationRecovery`]; kept here,
/// deprecated, for one release.
#[deprecated(note = "renamed to `invocation::InvocationRecovery`")]
pub type CodexRecovery = crate::autopilot::invocation::InvocationRecovery;

/// Codex CLI. It assigns its own thread ID, reported by `thread.started`, so
/// only a resume names the native ID the run must report.
pub struct CodexCli;

/// The prompt Codex reads as its stdin, a sibling of the invocation's
/// schema marker.
fn prompt_path(a: &InvocationArtifacts) -> PathBuf {
    Path::new(&a.schema_path).with_file_name("prompt.md")
}

impl SupervisedCli for CodexCli {
    fn implementer(&self) -> Implementer {
        Implementer::Codex
    }

    /// argv verified against codex-cli 0.163.0. Resume has no `-C`; cwd is set
    /// by the supervisor. Controller UUIDs and Claude-specific limits stay out.
    /// The prompt is `-`: Codex reads it from stdin, the private prompt file
    /// ([`Self::stdin_file`]), so the goal condition (issue body, human
    /// answers, review findings) is never in argv, where any local user
    /// reads it with `ps` (measured on 0.163.0 for `exec` and `exec
    /// resume`).
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
        args.push("-".into());
        Ok(args)
    }

    /// The condition, byte for byte, as the private prompt file; the core
    /// creates it exactly once.
    fn spawn_files(&self, spec: &DispatchSpec, a: &InvocationArtifacts) -> Vec<SpawnFile> {
        vec![SpawnFile {
            path: prompt_path(a),
            contents: spec.condition.as_bytes().to_vec(),
        }]
    }

    fn stdin_file(&self, a: &InvocationArtifacts) -> Option<PathBuf> {
        Some(prompt_path(a))
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
        ParsedRun::from_evidence("Codex", errors, native_id, verdict, turns, observed_output)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use crate::autopilot::dispatch::{DispatchSpec, InvocationArtifacts, SessionMode};
    use crate::autopilot::implementer::Implementer;
    use crate::autopilot::invocation::{validate_argv, VERDICT_JSON_SCHEMA};

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
            serde_json::from_str::<serde_json::Value>(VERDICT_JSON_SCHEMA).unwrap(),
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
        let args = validate_argv(
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
                "-"
            ]
        );
    }

    /// The goal condition (issue body, human answers, review findings)
    /// never appears in argv, where any local user reads it with `ps`: it
    /// is a private prompt file the child reads as its stdin (`-`, measured
    /// on codex-cli 0.163.0 for `exec` and `exec resume`). The marker
    /// grammar still recovers the invocation from the argv, `-` included.
    #[test]
    fn the_condition_travels_in_a_private_prompt_file_not_argv() {
        use crate::autopilot::invocation::{SpawnFile, SupervisedCli};
        use crate::autopilot::process_tree::ProcessRow;
        for session in [
            SessionMode::New {
                session_uuid: "ignored-controller-id".into(),
            },
            SessionMode::Resume {
                session_uuid: "11111111-2222-3333-4444-555555555555".into(),
            },
        ] {
            let mut s = spec(session);
            s.condition = "SECRET-ISSUE-BODY with words".into();
            let args = validate_argv(Path::new("/repo"), &s).unwrap();
            let joined = args.join(" ");
            assert!(!joined.contains("SECRET-ISSUE-BODY"), "{joined}");
            assert_eq!(args.last().map(String::as_str), Some("-"));
            let prompt = Path::new("/tmp/prompt.md");
            assert_eq!(
                super::CodexCli.spawn_files(&s, &artifacts()),
                vec![SpawnFile {
                    path: prompt.to_path_buf(),
                    contents: b"SECRET-ISSUE-BODY with words".to_vec(),
                }]
            );
            assert_eq!(
                super::CodexCli.stdin_file(&artifacts()).as_deref(),
                Some(prompt)
            );
            let row = ProcessRow {
                pid: 1,
                ppid: 1,
                pgid: 1,
                stat: "S".into(),
                args: format!("/opt/homebrew/bin/codex {joined}"),
            };
            assert_eq!(row.marker(), Ok(Some("/tmp/schema.json")), "{joined}");
        }
    }

    #[test]
    fn resume_uses_native_thread_id_and_omits_c_and_goal_prefix() {
        let args = validate_argv(
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
                "-"
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
    /// The supervised child reads the exact condition on its stdin, from a
    /// prompt file only the operator can read.
    #[cfg(unix)]
    #[test]
    fn a_supervised_codex_reads_its_condition_from_a_private_stdin_file() {
        use std::os::unix::fs::PermissionsExt;

        use crate::autopilot::invocation::contract_tests::{binary, observable, script};
        let (dir, mut s) = setup();
        s.condition = "the exact condition".into();
        let bin = binary(
            dir.path(),
            &observable(&script(
                "[ \"$(cat)\" = 'the exact condition' ] || exit 9",
                0,
            )),
        );
        let o = crate::autopilot::invocation::run_supervised_bounded(
            &bin,
            dir.path(),
            &s,
            std::time::Duration::from_secs(20),
        )
        .unwrap();
        assert!(o.process_success && o.is_met(), "{o:?}");
        let a = s.invocation_artifacts.as_ref().unwrap();
        let prompt = std::path::Path::new(&a.schema_path).with_file_name("prompt.md");
        let mode = std::fs::metadata(prompt).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
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

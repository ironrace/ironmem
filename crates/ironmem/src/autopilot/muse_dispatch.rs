//! Muse Code (`muse exec`) as a supervised implementation harness: its
//! argv, prompt file, child environment and JSONL journal parsing.
//! Supervision itself lives in [`super::invocation`].
//!
//! Every flag and record shape here was measured against Muse Code
//! 1.4.13-R6996.1 on 2026-10-26 (issue #366 has the measurements):
//! `--session-id` both creates a session and headlessly continues an existing
//! one, so new and resumed invocations share one argv; the final verdict is
//! the `text` of the last `run_terminal` record, shaped by `--output-schema`;
//! a SIGTERM/SIGINT ends the process with no terminal record at all; and no
//! record carries a price, so every invocation is unpriced.
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::autopilot::dispatch::DispatchSpec;
use crate::autopilot::implementer::Implementer;
use crate::autopilot::invocation::{
    create_private, validation, FinalVerdict, InvocationArtifacts, ParsedRun, SupervisedCli,
};
use crate::error::MemoryError;

/// Muse CLI. It continues the controller's `--session-id`, so every
/// invocation names the native ID its run must report.
pub struct MuseCli;

/// The prompt Muse reads, a sibling of the invocation's schema marker.
pub fn prompt_path(a: &InvocationArtifacts) -> PathBuf {
    Path::new(&a.schema_path).with_file_name("prompt.md")
}

/// ps joins argv with spaces, so a word with whitespace, or a value that
/// reads as an option, would make the marker grammar misparse this row.
fn marker_safe<'a>(what: &str, word: &'a str) -> Result<&'a str, MemoryError> {
    if word.is_empty() || word.starts_with('-') || word.chars().any(char::is_whitespace) {
        return Err(validation(format!(
            "Muse {what} must be non-empty, contain no whitespace and not start with '-'; got {word:?}"
        )));
    }
    Ok(word)
}

impl SupervisedCli for MuseCli {
    fn implementer(&self) -> Implementer {
        Implementer::Muse
    }

    /// `--disable-sandbox`: Muse's sandbox broke SSH commit signing and did
    /// not confine writes to the workspace (measured), so it bought nothing
    /// and cost the commit contract. `--trust-workspace` loads the repo's own
    /// rules and hooks, as Codex's `--dangerously-bypass-hook-trust` does.
    /// `--no-foreign-personal-context` keeps the operator's Claude Code and
    /// Codex personal rules out of an unattended run. The prompt goes through
    /// `--prompt-file`, so `ps` shows no positional word and the marker
    /// grammar parses the whole argv.
    fn argv(
        &self,
        repo: &Path,
        spec: &DispatchSpec,
        a: &InvocationArtifacts,
    ) -> Result<Vec<String>, MemoryError> {
        // Only the canonical spelling: Muse names its session stream by the
        // UUID, and a `urn:uuid:` or uppercase alias would never match it.
        let session = spec.session.session_uuid();
        if !uuid::Uuid::parse_str(session).is_ok_and(|id| id.hyphenated().to_string() == session) {
            return Err(validation(
                "Muse requires the controller's canonical session UUID for --session-id",
            ));
        }
        if spec.max_turns == 0 {
            return Err(validation("Muse --max-model-steps must be at least 1"));
        }
        let model = marker_safe("model", &spec.model)?;
        let workspace = marker_safe(
            "workspace path",
            repo.to_str()
                .ok_or_else(|| validation("Muse workspace path must be UTF-8"))?,
        )?;
        let prompt = prompt_path(a);
        let prompt = prompt
            .to_str()
            .ok_or_else(|| validation("Muse prompt path must be UTF-8"))?;
        Ok(vec![
            "exec".into(),
            "--json".into(),
            "--session-id".into(),
            session.into(),
            "--output-schema".into(),
            a.schema_path.clone(),
            "--prompt-file".into(),
            prompt.into(),
            "--model".into(),
            model.into(),
            "--max-model-steps".into(),
            spec.max_turns.to_string(),
            "--workspace".into(),
            workspace.into(),
            "--approval-mode".into(),
            "never".into(),
            "--disable-sandbox".into(),
            "--trust-workspace".into(),
            "--no-foreign-personal-context".into(),
            "--disable-request-user-input".into(),
        ])
    }

    fn spawn_files(&self, a: &InvocationArtifacts) -> Vec<PathBuf> {
        vec![prompt_path(a)]
    }

    /// The condition, byte for byte, in a private file created exactly once:
    /// an existing prompt is never overwritten.
    fn prepare_spawn(
        &self,
        spec: &DispatchSpec,
        a: &InvocationArtifacts,
    ) -> Result<(), MemoryError> {
        let mut file = create_private(&prompt_path(a))?;
        file.write_all(spec.condition.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    /// The launcher otherwise replaces the binary between invocations.
    fn env(&self) -> &'static [(&'static str, &'static str)] {
        &[("MUSE_NO_AUTO_UPDATE", "1")]
    }

    fn expected_native_id(&self, spec: &DispatchSpec) -> Option<String> {
        Some(spec.session.session_uuid().to_owned())
    }

    /// Only the controller's session counts. Records on other session streams
    /// (delegated agents, if `--trust-workspace` enables delegation) are
    /// ignored rather than treated as a conflict, and a terminal record on
    /// any other stream is never this run's verdict. Muse writes no final
    /// file, so `final_text` is unused.
    fn parse(
        &self,
        journal: &str,
        _final_text: Result<String, MemoryError>,
        expected_native_id: Option<&str>,
    ) -> ParsedRun {
        let mut parsed = ParsedRun {
            observed_output: !journal.is_empty(),
            ..ParsedRun::default()
        };
        let Some(expected) = expected_native_id else {
            parsed
                .errors
                .push("Muse supervisor record names no session; nothing can be trusted".into());
            return parsed;
        };
        let mut saw_expected = false;
        let mut last_terminal = None;
        for line in journal.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                parsed.errors.push("malformed Muse JSONL journal".into());
                continue;
            };
            let ours = value["stream"]["kind"].as_str() == Some("session")
                && value["stream"]["id"].as_str() == Some(expected);
            saw_expected |= ours;
            match value["payload"]["kind"].as_str() {
                Some("run_terminal") if ours => last_terminal = Some(value["payload"].clone()),
                Some("run_usage") if value["stream"]["id"].as_str() == Some(expected) => {
                    if let Some(n) = value["payload"]["completed_calls"].as_u64() {
                        parsed.turns = u32::try_from(n).unwrap_or(u32::MAX);
                    }
                }
                _ => {}
            }
        }
        if saw_expected {
            parsed.native_id = Some(expected.to_owned());
        } else {
            parsed
                .errors
                .push("Muse journal never names the requested session".into());
        }
        match last_terminal {
            None => parsed
                .errors
                .push("Muse run emitted no terminal record (killed or crashed)".into()),
            Some(t) if t["terminal"].as_str() == Some("completed") => {
                match serde_json::from_str::<FinalVerdict>(t["text"].as_str().unwrap_or_default()) {
                    Ok(verdict) => parsed.verdict = Some(verdict),
                    Err(e) => parsed
                        .errors
                        .push(format!("invalid Muse final verdict: {e}")),
                }
            }
            Some(t) => parsed.errors.push(format!(
                "Muse run ended {}: {}",
                t["terminal"].as_str().unwrap_or("without a terminal kind"),
                t["reason"].as_str().unwrap_or("no reason given")
            )),
        }
        if !parsed.errors.is_empty() {
            parsed.verdict = None;
        }
        parsed
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::autopilot::dispatch::{DispatchSpec, SessionMode, Verdict};
    use crate::autopilot::implementer::Implementer;
    use crate::autopilot::invocation::{InvocationArtifacts, SupervisedCli};

    const SID: &str = "934331e6-2c8a-4e0f-bcd6-0b6300000000";

    fn artifacts() -> InvocationArtifacts {
        let d = "/r/.git/ironmem/autopilot/o-r-7/0f0e0d0c-0b0a-4908-8706-050403020100";
        InvocationArtifacts {
            invocation_id: "0f0e0d0c-0b0a-4908-8706-050403020100".into(),
            schema_path: format!("{d}/schema.json"),
            journal_path: format!("{d}/journal.jsonl"),
            final_path: format!("{d}/final.json"),
            stderr_path: format!("{d}/stderr.log"),
            exit_path: format!("{d}/exit.json"),
        }
    }

    fn spec(session: SessionMode) -> DispatchSpec {
        DispatchSpec {
            implementer: Implementer::Muse,
            invocation_artifacts: Some(artifacts()),
            session,
            name: "ic-o-r-7".into(),
            model: "muse-wonder-1.0".into(),
            max_budget_usd: 5.0,
            max_turns: 300,
            condition: "make the gate pass".into(),
            wall_clock_timeout: None,
        }
    }

    fn record(stream_kind: &str, stream_id: &str, payload: serde_json::Value) -> String {
        serde_json::json!({
            "schema_version": 1, "id": "x", "stream": {"kind": stream_kind, "id": stream_id},
            "sequence": 1, "recorded_at": 1, "record_type": "event", "durability": "durable",
            "causation_id": "c", "payload_type": "t", "payload_schema_version": 1,
            "payload": payload
        })
        .to_string()
    }

    fn terminal(terminal: &str, text: &str) -> String {
        record(
            "session",
            SID,
            serde_json::json!({
                "kind": "run_terminal", "terminal": terminal, "text": text, "reason": null
            }),
        )
    }

    fn usage(completed: u32) -> String {
        record(
            "exec.usage",
            SID,
            serde_json::json!({
                "kind": "run_usage", "models": [], "calls": completed,
                "completed_calls": completed, "failed_calls": 0, "cancelled_calls": 0,
                "children_included": false, "allowance": null
            }),
        )
    }

    /// The options in [`MuseCli::argv`] that take a value. Every other word
    /// after `exec` must be a bare flag.
    const VALUED: &[&str] = &[
        "--session-id",
        "--output-schema",
        "--prompt-file",
        "--model",
        "--max-model-steps",
        "--workspace",
        "--approval-mode",
    ];

    #[test]
    fn new_and_resumed_invocations_use_the_same_measured_argv() {
        for session in [
            SessionMode::New {
                session_uuid: SID.into(),
            },
            SessionMode::Resume {
                session_uuid: SID.into(),
            },
        ] {
            let s = spec(session);
            let args = MuseCli
                .argv(Path::new("/w/o-r-7"), &s, &artifacts())
                .unwrap();
            assert_eq!(
                args,
                [
                    "exec",
                    "--json",
                    "--session-id",
                    SID,
                    "--output-schema",
                    &artifacts().schema_path,
                    "--prompt-file",
                    prompt_path(&artifacts()).to_str().unwrap(),
                    "--model",
                    "muse-wonder-1.0",
                    "--max-model-steps",
                    "300",
                    "--workspace",
                    "/w/o-r-7",
                    "--approval-mode",
                    "never",
                    "--disable-sandbox",
                    "--trust-workspace",
                    "--no-foreign-personal-context",
                    "--disable-request-user-input",
                ]
            );
            // ps joins argv with spaces, so the marker grammar can parse it
            // only when no positional word appears: every non-option word
            // is the value of the option before it, and the condition itself
            // travels in the prompt file.
            let joined = args.join(" ");
            let words: Vec<&str> = joined.split_whitespace().collect();
            assert_eq!(words.len(), args.len(), "an argv word carries whitespace");
            for (i, word) in words.iter().enumerate().skip(1) {
                assert!(
                    word.starts_with("--") || VALUED.contains(&words[i - 1]),
                    "positional word {word:?} in {joined}"
                );
            }
            assert!(!joined.contains(&s.condition), "{joined}");
            // The marker grammar recovers this exact invocation from its own
            // argv, so the builder and the grammar cannot drift apart.
            let row = crate::autopilot::process_tree::ProcessRow {
                pid: 1,
                ppid: 1,
                pgid: 1,
                stat: "S".into(),
                args: format!("/u/muse-bin-1.4.13-R6996.1 {joined}"),
            };
            assert_eq!(
                row.marker().unwrap(),
                Some(artifacts().schema_path.as_str())
            );
            assert_eq!(MuseCli.expected_native_id(&s).as_deref(), Some(SID));
        }
    }

    #[test]
    fn a_session_that_is_not_a_uuid_or_a_zero_step_cap_is_refused_before_spawn() {
        for session in [
            "not-a-uuid".to_owned(),
            format!("urn:uuid:{SID}"),
            SID.to_ascii_uppercase(),
            SID.replace('-', ""),
        ] {
            let bad = spec(SessionMode::Resume {
                session_uuid: session.clone(),
            });
            assert!(
                MuseCli.argv(Path::new("/w"), &bad, &artifacts()).is_err(),
                "accepted session {session:?}"
            );
        }
        let mut zero = spec(SessionMode::New {
            session_uuid: SID.into(),
        });
        zero.max_turns = 0;
        assert!(MuseCli.argv(Path::new("/w"), &zero, &artifacts()).is_err());
    }

    #[test]
    fn a_model_or_workspace_that_would_break_the_marker_grammar_is_refused() {
        let ok = spec(SessionMode::New {
            session_uuid: SID.into(),
        });
        for model in ["", "muse wonder", "--yolo"] {
            let mut s = ok.clone();
            s.model = model.into();
            assert!(
                MuseCli.argv(Path::new("/w"), &s, &artifacts()).is_err(),
                "accepted model {model:?}"
            );
        }
        assert!(MuseCli
            .argv(Path::new("/w/my repo"), &ok, &artifacts())
            .is_err());
    }

    #[test]
    fn the_child_never_self_updates_mid_dispatch() {
        assert_eq!(MuseCli.env(), &[("MUSE_NO_AUTO_UPDATE", "1")]);
    }

    #[test]
    fn a_completed_schema_verdict_with_one_session_is_parsed() {
        let journal = [
            usage(9),
            terminal("completed", r#"{"verdict":"met","reason":"gate green"}"#),
        ]
        .join("\n");
        let parsed = MuseCli.parse(&journal, Ok(String::new()), Some(SID));
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        assert_eq!(parsed.native_id.as_deref(), Some(SID));
        assert_eq!(parsed.turns, 9);
        assert_eq!(parsed.verdict.unwrap().verdict, Verdict::Met);
        // With no expected ID (a corrupt exit record), nothing is trusted.
        let untrusted = MuseCli.parse(&journal, Ok(String::new()), None);
        assert!(untrusted.verdict.is_none() && !untrusted.errors.is_empty());
    }

    #[test]
    fn records_after_the_terminal_record_do_not_hide_it() {
        // Measured: `session.workspace_branch.observed` follows the
        // `run_terminal` record on the same session stream.
        let journal = [
            terminal("completed", r#"{"verdict":"not_met","reason":"red"}"#),
            record(
                "session",
                SID,
                serde_json::json!({"kind": "workspace_branch_observed"}),
            ),
        ]
        .join("\n");
        let parsed = MuseCli.parse(&journal, Ok(String::new()), Some(SID));
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        assert_eq!(parsed.verdict.unwrap().verdict, Verdict::NotMet);
    }

    #[test]
    fn every_untrustworthy_journal_yields_no_verdict() {
        let other = "11111111-2222-3333-4444-555555555555";
        let cases = [
            // Killed by a signal: no terminal record at all.
            usage(3),
            String::new(),
            terminal("failed", ""),
            terminal("cancelled", ""),
            terminal("completed", "not json"),
            terminal(
                "completed",
                r#"{"verdict":"success","reason":"wrong enum"}"#,
            ),
            terminal("completed", r#"{"verdict":"met","reason":"x","extra":1}"#),
            // A terminal record from a different session only.
            record(
                "session",
                other,
                serde_json::json!({
                    "kind": "run_terminal", "terminal": "completed",
                    "text": r#"{"verdict":"met","reason":"x"}"#, "reason": null
                }),
            ),
            format!(
                "not json\n{}",
                terminal("completed", r#"{"verdict":"met","reason":"x"}"#)
            ),
            // A later failure supersedes an earlier completion.
            [
                terminal("completed", r#"{"verdict":"met","reason":"x"}"#),
                terminal("failed", ""),
            ]
            .join("\n"),
        ];
        for journal in cases {
            let parsed = MuseCli.parse(&journal, Ok(String::new()), Some(SID));
            assert!(
                parsed.verdict.is_none(),
                "trusted a verdict from: {journal}"
            );
            assert!(!parsed.errors.is_empty(), "no error for: {journal}");
        }
    }

    #[test]
    fn records_from_other_sessions_such_as_delegated_agents_are_ignored() {
        let journal = [
            record(
                "session",
                "11111111-2222-3333-4444-555555555555",
                serde_json::json!({"kind": "x"}),
            ),
            terminal("completed", r#"{"verdict":"met","reason":"ok"}"#),
        ]
        .join("\n");
        let parsed = MuseCli.parse(&journal, Ok(String::new()), Some(SID));
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        assert_eq!(parsed.native_id.as_deref(), Some(SID));
    }

    #[test]
    fn prepare_spawn_writes_the_exact_condition_privately_once() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = crate::autopilot::process_tree::testing::invocation(dir.path());
        let mut s = spec(SessionMode::New {
            session_uuid: SID.into(),
        });
        s.invocation_artifacts = Some(a.clone());
        assert_eq!(MuseCli.spawn_files(&a), vec![prompt_path(&a)]);
        MuseCli.prepare_spawn(&s, &a).unwrap();
        assert_eq!(
            std::fs::read_to_string(prompt_path(&a)).unwrap(),
            "make the gate pass"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(prompt_path(&a))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        // A second spawn into the same invocation is refused, never overwritten.
        assert!(MuseCli.prepare_spawn(&s, &a).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_stubbed_muse_run_is_met_only_with_its_own_session_and_exit_zero() {
        use crate::autopilot::invocation::contract_tests::{binary, setup};
        for (code, met) in [(0, true), (1, false)] {
            let (repo, mut s) = setup();
            s.implementer = Implementer::Muse;
            s.session = SessionMode::New {
                session_uuid: SID.into(),
            };
            s.condition = "the exact condition".into();
            let bin = binary(
                repo.path(),
                &format!(
                    "[ \"$MUSE_NO_AUTO_UPDATE\" = 1 ] || exit 8\n\
                     prompt=\"$(echo \"$@\" | sed -n 's/.*--prompt-file \\([^ ]*\\).*/\\1/p')\"\n\
                     [ \"$(cat \"$prompt\")\" = 'the exact condition' ] || exit 9\n\
                     printf '%s\\n' '{}'\nprintf '%s\\n' '{}'\n/bin/sleep 0.5\nexit {code}\n",
                    usage(2),
                    terminal("completed", r#"{"verdict":"met","reason":"ok"}"#)
                ),
            );
            let outcome = crate::autopilot::invocation::run_supervised_bounded(
                &bin,
                repo.path(),
                &s,
                std::time::Duration::from_secs(20),
            )
            .unwrap();
            assert_eq!(outcome.is_met(), met, "{outcome:?}");
            assert!(outcome.invocation_started);
            assert_eq!(outcome.process_success, met, "{outcome:?}");
            assert_eq!(outcome.native_session_id.as_deref(), Some(SID));
            assert_eq!(outcome.num_turns, 2);
            assert_eq!(outcome.cost_usd, None);
            // A replacement controller recovers the same answer from disk.
            let recovered = crate::autopilot::invocation::read_artifacts(
                Implementer::Muse,
                s.invocation_artifacts.as_ref().unwrap(),
            )
            .unwrap();
            assert_eq!(recovered.is_met(), met, "{recovered:?}");
        }
    }
}

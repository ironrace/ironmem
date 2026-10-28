//! Muse Code (`muse exec`) as a supervised implementation harness: its
//! argv, prompt file, child environment and JSONL journal parsing.
//! Supervision itself lives in [`super::invocation`].
//!
//! Every flag and record shape here was measured against Muse Code
//! 1.4.13-R6996.1 on 2026-10-26 (issue #366 has the measurements):
//! `--session-id` both creates a session and headlessly continues an existing
//! one, so new and resumed invocations share one argv; the final verdict is
//! the `text` of the one `run_terminal` record, shaped by `--output-schema`;
//! a SIGTERM/SIGINT ends the process with no terminal record at all; and no
//! record carries a price, so every invocation is unpriced.
use std::path::{Path, PathBuf};

use crate::autopilot::dispatch::{DispatchSpec, SessionRefusal};
use crate::autopilot::implementer::Implementer;
use crate::autopilot::invocation::{
    validation, FinalVerdict, InvocationArtifacts, ParsedRun, SpawnFile, SupervisedCli,
};
use crate::autopilot::scrub::scrub_and_bound;
use crate::error::MemoryError;

/// How much of a rejected value or verdict text an error quotes.
const EXCERPT_CHARS: usize = 120;

/// `text` scrubbed and bounded to [`EXCERPT_CHARS`], ending in `…` when it
/// was cut, so a quote never reads as the whole value.
fn excerpt(text: &str) -> String {
    let bounded = scrub_and_bound(text, EXCERPT_CHARS);
    let mut text = bounded.text;
    if bounded.truncated {
        text.push('…');
    }
    text
}

/// One journal line as [`MuseCli::parse`] reads it: the `stream` and the
/// head of the `payload`, with every other field, and the rest of the
/// payload, skipped without being built. Only a line that is not a JSON
/// object fails to parse; a field of an unexpected type reads as absent,
/// as indexing a `serde_json::Value` would.
#[derive(Default)]
struct Envelope {
    stream: Head,
    payload: Head,
}

/// The fields of a `stream` or `payload` object the parser reads; any of
/// them absent, or of another type, is `None`, and so is every field of a
/// value that is not an object.
#[derive(Default)]
struct Head {
    kind: Option<String>,
    id: Option<String>,
    completed_calls: Option<u64>,
}

impl<'de> serde::Deserialize<'de> for Envelope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Envelope;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a Muse journal record object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Envelope, A::Error> {
                let mut envelope = Envelope::default();
                while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                    match key.as_ref() {
                        "stream" => envelope.stream = map.next_value()?,
                        "payload" => envelope.payload = map.next_value()?,
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(envelope)
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

impl<'de> serde::Deserialize<'de> for Head {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Head;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Head, A::Error> {
                let mut head = Head::default();
                while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                    match key.as_ref() {
                        "kind" => head.kind = map.next_value::<Leaf>()?.text,
                        "id" => head.id = map.next_value::<Leaf>()?.text,
                        "completed_calls" => {
                            head.completed_calls = map.next_value::<Leaf>()?.count;
                        }
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(head)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Head, A::Error> {
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(Head::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<Head, E> {
                Ok(Head::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Head, E> {
                Ok(Head::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Head, E> {
                Ok(Head::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<Head, E> {
                Ok(Head::default())
            }
            fn visit_str<E>(self, _: &str) -> Result<Head, E> {
                Ok(Head::default())
            }
            fn visit_unit<E>(self) -> Result<Head, E> {
                Ok(Head::default())
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// A scalar field of a [`Head`]: its text if it is a string, its count if
/// it is a non-negative integer, and neither for anything else.
#[derive(Default)]
struct Leaf {
    text: Option<String>,
    count: Option<u64>,
}

impl<'de> serde::Deserialize<'de> for Leaf {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Leaf;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_str<E>(self, text: &str) -> Result<Leaf, E> {
                Ok(Leaf {
                    text: Some(text.to_owned()),
                    count: None,
                })
            }
            fn visit_u64<E>(self, count: u64) -> Result<Leaf, E> {
                Ok(Leaf {
                    text: None,
                    count: Some(count),
                })
            }
            fn visit_i64<E>(self, count: i64) -> Result<Leaf, E> {
                Ok(Leaf {
                    text: None,
                    count: u64::try_from(count).ok(),
                })
            }
            fn visit_f64<E>(self, _: f64) -> Result<Leaf, E> {
                Ok(Leaf::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<Leaf, E> {
                Ok(Leaf::default())
            }
            fn visit_unit<E>(self) -> Result<Leaf, E> {
                Ok(Leaf::default())
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Leaf, A::Error> {
                while map
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(Leaf::default())
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Leaf, A::Error> {
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(Leaf::default())
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// Muse CLI. It continues the controller's `--session-id`, so every
/// invocation names the native ID its run must report.
pub struct MuseCli;

/// The prompt Muse reads, a sibling of the invocation's schema marker.
pub fn prompt_path(a: &InvocationArtifacts) -> PathBuf {
    Path::new(&a.schema_path).with_file_name("prompt.md")
}

/// Whether `word` survives as one marker word. ps joins argv with spaces, so
/// a word with whitespace, or a value that reads as an option, would make the
/// marker grammar misparse this row. Run validation refuses a model on the
/// same rule before any side effect.
pub(crate) fn is_marker_safe(word: &str) -> bool {
    !word.is_empty() && !word.starts_with('-') && !word.chars().any(char::is_whitespace)
}

fn marker_safe<'a>(what: &str, word: &'a str) -> Result<&'a str, MemoryError> {
    if !is_marker_safe(word) {
        return Err(validation(format!(
            "Muse {what} must be non-empty, contain no whitespace and not start with '-'; got {:?}",
            excerpt(word)
        )));
    }
    Ok(word)
}

/// `line` without a leading `error:` in any case, and the whitespace after it.
fn strip_error_lead_in(line: &str) -> &str {
    match line.get(..6) {
        Some(lead) if lead.eq_ignore_ascii_case("error:") => line[6..].trim_start(),
        _ => line,
    }
}

impl SupervisedCli for MuseCli {
    fn implementer(&self) -> Implementer {
        Implementer::Muse
    }

    /// # Security posture
    ///
    /// Three flags combine into an unattended agent with the operator's full
    /// authority, steered partly by content the repository controls:
    ///
    /// - `--approval-mode never`: every tool call runs without a prompt.
    /// - `--disable-sandbox`: the shell is unconfined and runs as the
    ///   operator's uid, with network access and the operator's SSH signing
    ///   keys. Muse's sandbox broke SSH commit signing and did not confine
    ///   writes to the workspace (measured), so it cost the commit contract
    ///   and bought no containment.
    /// - `--trust-workspace`: loads the workspace's rules, hooks *and*
    ///   skills, and enables agent delegation (`muse exec --help`; the #366
    ///   measurements). Repository content therefore steers the agent and
    ///   runs code unattended. Parity with Codex's
    ///   `--dangerously-bypass-hook-trust` is why it is passed, not a bound
    ///   on that risk.
    ///
    /// Trust boundary: with the sandbox off, the agent can write this
    /// invocation's journal and exit files itself (their paths are visible
    /// in `ps`). [`MuseCli::parse`] defends against Muse's framing, not
    /// tampering by the agent; that is acceptable because a forged record
    /// buys nothing beyond the agent answering `met` itself.
    ///
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
            return Err(validation(format!(
                "Muse requires the controller's canonical session UUID for --session-id; got {:?}",
                excerpt(session)
            )));
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

    /// The condition, byte for byte, as the prompt file. The core creates
    /// it privately exactly once, so an existing prompt is never overwritten.
    fn spawn_files(&self, spec: &DispatchSpec, a: &InvocationArtifacts) -> Vec<SpawnFile> {
        vec![SpawnFile {
            path: prompt_path(a),
            contents: spec.condition.as_bytes().to_vec(),
        }]
    }

    /// Four times the default. Muse's `--json` stream wraps every record,
    /// tool output included, in envelope metadata (`schema_version`, `id`,
    /// `stream`, `sequence`, `recorded_at`, `causation_id`, …), and its
    /// volume over a long gate-running session has not been measured, so a
    /// cap sized for Codex's leaner JSONL could kill a healthy run mid-way.
    /// Recovery reads at most this much back into memory.
    fn journal_limit(&self) -> u64 {
        4 * crate::autopilot::invocation::JOURNAL_LIMIT
    }

    /// The launcher otherwise replaces the binary between invocations.
    fn env(&self) -> &'static [(&'static str, &'static str)] {
        &[("MUSE_NO_AUTO_UPDATE", "1")]
    }

    fn expected_native_id(&self, spec: &DispatchSpec) -> Option<String> {
        Some(spec.session.session_uuid().to_owned())
    }

    /// Muse's own refusal lines about the requested session, measured on
    /// 1.4.13 and again on 1.4.14 (exit 1, nothing on the journal):
    ///
    /// - `session <uuid> is already in use`: another `exec` holds it;
    /// - `failed to read retained session <uuid>: session <uuid> was created
    ///   with provider <a>; refusing to resume with provider <b>; start a new
    ///   session to switch providers`, and the same prefix for a retained
    ///   session it cannot read back.
    ///
    /// Only a whole line about `expected_native_id` counts (Muse's own
    /// `muse: ` notice prefix and an `error:` lead-in in any case allowed),
    /// so another session's refusal, or the words quoted in some tool's
    /// output, never do. "In use" wins over any
    /// other refusal: it is the one that must never be rotated over.
    fn session_refusal(
        &self,
        stderr: &str,
        expected_native_id: Option<&str>,
    ) -> Option<SessionRefusal> {
        let session = expected_native_id?;
        let in_use = format!("session {session} is already in use");
        let unreadable = format!("failed to read retained session {session}:");
        let other_provider = format!("session {session} was created with provider ");
        let mut refusal = None;
        for line in stderr.lines() {
            let line = line.trim();
            let line = line.strip_prefix("muse: ").unwrap_or(line);
            let line = strip_error_lead_in(line);
            if line == in_use {
                return Some(SessionRefusal::InUse);
            }
            if line.starts_with(&unreadable)
                || (line.starts_with(&other_provider)
                    && line.contains("; refusing to resume with provider "))
            {
                refusal = Some(SessionRefusal::Unresumable);
            }
        }
        refusal
    }

    /// Only the controller's session counts. Records on other session streams
    /// (delegated agents, which `--trust-workspace` enables) are ignored
    /// rather than treated as a conflict, and a terminal record on any other
    /// stream is never this run's verdict. Measured runs emit exactly one
    /// terminal record per invocation, so more than one trusts nothing.
    /// Whether a resume after a wall-clock kill replays the interrupted turn
    /// with a terminal record of its own is unmeasured; if it does, that
    /// resume is an infrastructure failure (bounded by the run's
    /// consecutive-failure limit) rather than a guess at which terminal
    /// answers this invocation's prompt. Muse writes no final file, so
    /// `final_text` is unused.
    fn parse(
        &self,
        journal: &str,
        _final_text: Result<String, MemoryError>,
        expected_native_id: Option<&str>,
    ) -> ParsedRun {
        let observed_output = !journal.is_empty();
        let Some(expected) = expected_native_id else {
            return ParsedRun::failed(
                vec!["Muse supervisor record names no session; nothing can be trusted".into()],
                None,
                0,
                observed_output,
            );
        };
        let mut errors: Vec<String> = Vec::new();
        let mut native_id = None;
        let mut verdict = None;
        let mut turns = 0;
        let on_stream = |envelope: &Envelope, kind: &str| {
            envelope.stream.kind.as_deref() == Some(kind)
                && envelope.stream.id.as_deref() == Some(expected)
        };
        let mut saw_expected = false;
        let mut terminals = Vec::new();
        let (mut malformed, mut first_malformed) = (0usize, 0usize);
        for (index, line) in journal.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            // A bare number, string or array parses, but is no record. Only
            // the envelope is read; a payload (tool output, mostly) is
            // skipped unbuilt, and parsed whole only for this session's
            // terminal record.
            let Ok(envelope) = serde_json::from_str::<Envelope>(line) else {
                malformed += 1;
                if first_malformed == 0 {
                    first_malformed = index + 1;
                }
                continue;
            };
            let ours = on_stream(&envelope, "session");
            saw_expected |= ours;
            match envelope.payload.kind.as_deref() {
                Some("run_terminal") if ours => {
                    // The envelope parsed, so the line is a JSON object.
                    let value: serde_json::Value =
                        serde_json::from_str(line).unwrap_or(serde_json::Value::Null);
                    terminals.push(value["payload"].clone());
                }
                Some("run_usage") if on_stream(&envelope, "exec.usage") => {
                    if let Some(n) = envelope.payload.completed_calls {
                        turns = u32::try_from(n).unwrap_or(u32::MAX);
                    }
                }
                _ => {}
            }
        }
        if malformed > 0 {
            errors.push(format!(
                "{malformed} malformed Muse JSONL line(s) (first at line {first_malformed})"
            ));
        }
        if saw_expected {
            native_id = Some(expected.to_owned());
        } else {
            errors.push(format!(
                "Muse journal never names the requested session {expected}"
            ));
        }
        match terminals.as_slice() {
            [] => errors.push("Muse run emitted no terminal record".into()),
            [t] if t["terminal"].as_str() == Some("completed") => match t["text"].as_str() {
                None => errors.push("Muse completed terminal record carries no text verdict".into()),
                Some(text) => match serde_json::from_str::<FinalVerdict>(text) {
                    Ok(parsed) => verdict = Some(parsed),
                    Err(e) => errors.push(format!(
                        "invalid Muse final verdict: {e}; text: {:?}",
                        excerpt(text)
                    )),
                },
            },
            [t] => errors.push(format!(
                "Muse run ended {}: {}",
                excerpt(t["terminal"].as_str().unwrap_or("without a terminal kind")),
                t["reason"]
                    .as_str()
                    .map_or_else(|| "no reason given".to_owned(), excerpt)
            )),
            many => errors.push(format!(
                "Muse journal has {} terminal records for the requested session; expected exactly one",
                many.len()
            )),
        }
        let parsed =
            ParsedRun::from_evidence("Muse", errors, native_id, verdict, turns, observed_output);
        // The error above already names the session it never saw.
        if saw_expected {
            parsed
        } else {
            parsed.reported_missing_session()
        }
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
            for bad in [
                spec(SessionMode::New {
                    session_uuid: session.clone(),
                }),
                spec(SessionMode::Resume {
                    session_uuid: session.clone(),
                }),
            ] {
                let err = MuseCli
                    .argv(Path::new("/w"), &bad, &artifacts())
                    .unwrap_err()
                    .to_string();
                assert!(err.contains(&session), "{err}");
            }
        }
        // The rejected value is quoted, but bounded.
        let long = spec(SessionMode::New {
            session_uuid: "x".repeat(500),
        });
        let err = MuseCli
            .argv(Path::new("/w"), &long, &artifacts())
            .unwrap_err()
            .to_string();
        assert!(!err.contains(&"x".repeat(121)), "{err}");
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
        for workspace in ["/w/my repo", "", "-w"] {
            assert!(
                MuseCli
                    .argv(Path::new(workspace), &ok, &artifacts())
                    .is_err(),
                "accepted workspace {workspace:?}"
            );
        }
    }

    /// A cut quote says so, so a reader never takes it for the whole value.
    #[test]
    fn an_excerpt_marks_a_cut_and_leaves_a_whole_value_alone() {
        let cut = excerpt(&"y".repeat(500));
        assert_eq!(cut, format!("{}…", "y".repeat(EXCERPT_CHARS)));
        assert_eq!(excerpt("short"), "short");
        assert_eq!(
            excerpt(&"y".repeat(EXCERPT_CHARS)),
            "y".repeat(EXCERPT_CHARS)
        );
    }

    /// Muse's own reason is quoted, but bounded and marked when cut, like
    /// any other value the parser quotes.
    #[test]
    fn a_failed_terminal_reason_is_quoted_bounded() {
        let long = record(
            "session",
            SID,
            serde_json::json!({
                "kind": "run_terminal", "terminal": "failed", "text": "",
                "reason": "z".repeat(500)
            }),
        );
        let errors = MuseCli
            .parse(&long, Ok(String::new()), Some(SID))
            .errors()
            .to_vec();
        let ended = errors
            .iter()
            .find(|e| e.starts_with("Muse run ended failed: "))
            .unwrap_or_else(|| panic!("{errors:?}"));
        assert!(!ended.contains(&"z".repeat(EXCERPT_CHARS + 1)), "{ended}");
        assert!(ended.ends_with('…'), "{ended}");
        let short = record(
            "session",
            SID,
            serde_json::json!({
                "kind": "run_terminal", "terminal": "cancelled", "text": "",
                "reason": "operator interrupt"
            }),
        );
        let errors = MuseCli.parse(&short, Ok(String::new()), Some(SID));
        assert!(
            errors
                .errors()
                .contains(&"Muse run ended cancelled: operator interrupt".to_owned()),
            "{:?}",
            errors.errors()
        );
    }

    /// A line that is valid JSON but no record (a bare number, string or
    /// array, as a torn write leaves) is as malformed as one that does not
    /// parse, as the Codex parser already holds.
    #[test]
    fn a_json_line_that_is_not_a_record_is_malformed() {
        let ok = terminal("completed", r#"{"verdict":"met","reason":"x"}"#);
        for stray in ["12345", "\"text\"", "[1,2]", "null", "true"] {
            let parsed = MuseCli.parse(&format!("{ok}\n{stray}"), Ok(String::new()), Some(SID));
            assert!(parsed.verdict().is_none(), "trusted with {stray}");
            assert!(
                parsed
                    .errors()
                    .contains(&"1 malformed Muse JSONL line(s) (first at line 2)".to_owned()),
                "{stray}: {:?}",
                parsed.errors()
            );
        }
        // Any object is a record, whatever its fields hold: one of an
        // unexpected type is simply not ours.
        for odd in [
            r#"{"stream":"session","payload":5}"#,
            r#"{"stream":{"kind":7,"id":[1]},"payload":{"kind":null,"completed_calls":-1}}"#,
            r#"{"stream":null,"payload":[{"kind":"run_terminal"}],"extra":{"deep":[1,{"a":"b"}]}}"#,
        ] {
            let parsed = MuseCli.parse(&format!("{ok}\n{odd}"), Ok(String::new()), Some(SID));
            assert!(parsed.errors().is_empty(), "{odd}: {:?}", parsed.errors());
            assert_eq!(parsed.verdict().unwrap().verdict, Verdict::Met, "{odd}");
        }
    }

    /// `run_usage` totals the run so far, so the latest record on the
    /// requested session's usage stream is the count: never a sum, never
    /// another session's, and a record with no count changes nothing.
    #[test]
    fn usage_takes_the_latest_count_on_the_requested_sessions_usage_stream() {
        let met = terminal("completed", r#"{"verdict":"met","reason":"ok"}"#);
        let turns = |records: &[String]| {
            let mut journal = records.to_vec();
            journal.push(met.clone());
            let parsed = MuseCli.parse(&journal.join("\n"), Ok(String::new()), Some(SID));
            assert!(parsed.errors().is_empty(), "{:?}", parsed.errors());
            parsed.turns()
        };
        // The latest wins, whatever order the counts arrive in.
        assert_eq!(turns(&[usage(3), usage(5)]), 5);
        assert_eq!(turns(&[usage(5), usage(3)]), 3);
        // Another session's usage, even on the usage stream, is not ours.
        let other = record(
            "exec.usage",
            "11111111-2222-4333-8444-555555555555",
            serde_json::json!({"kind": "run_usage", "completed_calls": 40}),
        );
        assert_eq!(turns(&[usage(3), other]), 3);
        // A usage record with no count leaves the last one standing.
        let uncounted = record(
            "exec.usage",
            SID,
            serde_json::json!({"kind": "run_usage", "calls": 9}),
        );
        assert_eq!(turns(&[usage(4), uncounted.clone()]), 4);
        assert_eq!(turns(&[uncounted]), 0);
        // A count past u32 is clamped, never wrapped.
        let huge = record(
            "exec.usage",
            SID,
            serde_json::json!({"kind": "run_usage", "completed_calls": u64::from(u32::MAX) + 7}),
        );
        assert_eq!(turns(&[huge]), u32::MAX);
    }

    /// Muse's own words, measured on 1.4.13 and again on 1.4.14 (exit 1, no
    /// journal), anchored to the requested session.
    #[test]
    fn session_refusals_are_read_from_muses_words_about_the_requested_session() {
        use crate::autopilot::dispatch::SessionRefusal;
        let other = "11111111-2222-4333-8444-555555555555";
        let in_use = format!("session {SID} is already in use");
        let cross = format!(
            "failed to read retained session {SID}: session {SID} was created with provider \
             echo; refusing to resume with provider meta; start a new session to switch providers"
        );
        let unreadable = format!("failed to read retained session {SID}: unexpected end of file");
        let notices = "muse: workspace root: /w (explicit)\nmuse: Agent delegation: auto \
                       unavailable: workspace is untrusted.\n";
        for (stderr, expected) in [
            (in_use.clone(), Some(SessionRefusal::InUse)),
            (format!("{notices}{in_use}\n"), Some(SessionRefusal::InUse)),
            (cross.clone(), Some(SessionRefusal::Unresumable)),
            (
                format!("{notices}{cross}\n"),
                Some(SessionRefusal::Unresumable),
            ),
            (unreadable.clone(), Some(SessionRefusal::Unresumable)),
            // An `error:` lead-in, any case and spacing, after an optional
            // `muse: ` notice prefix.
            (format!("error: {in_use}"), Some(SessionRefusal::InUse)),
            (format!("ERROR:{in_use}"), Some(SessionRefusal::InUse)),
            (
                format!("muse: Error:   {in_use}"),
                Some(SessionRefusal::InUse),
            ),
            (format!("Error: {cross}"), Some(SessionRefusal::Unresumable)),
            (format!("error: {}", in_use.replace(SID, other)), None),
            (format!("warning: {in_use}"), None),
            // Another session's refusal is not this run's.
            (in_use.replace(SID, other), None),
            (cross.replace(SID, other), None),
            // Quoted mid-line (say, in a tool's output echoed to stderr).
            (format!("tool said \"{in_use}\" earlier"), None),
            ("error: provider request timed out".into(), None),
            (String::new(), None),
            // "In use" wins over any other refusal, wherever it comes.
            (format!("{cross}\n{in_use}\n"), Some(SessionRefusal::InUse)),
            (
                format!("{in_use}\n{unreadable}\n"),
                Some(SessionRefusal::InUse),
            ),
        ] {
            assert_eq!(
                MuseCli.session_refusal(&stderr, Some(SID)),
                expected,
                "{stderr}"
            );
        }
        assert_eq!(MuseCli.session_refusal(&in_use, None), None);
    }

    #[test]
    fn a_completed_schema_verdict_with_one_session_is_parsed() {
        let journal = [
            usage(9),
            terminal("completed", r#"{"verdict":"met","reason":"gate green"}"#),
        ]
        .join("\n");
        let parsed = MuseCli.parse(&journal, Ok(String::new()), Some(SID));
        assert!(parsed.errors().is_empty(), "{:?}", parsed.errors());
        assert_eq!(parsed.native_id(), Some(SID));
        assert_eq!(parsed.turns(), 9);
        assert_eq!(parsed.verdict().unwrap().verdict, Verdict::Met);
        // With no expected ID (a corrupt exit record), nothing is trusted.
        let untrusted = MuseCli.parse(&journal, Ok(String::new()), None);
        assert!(untrusted.verdict().is_none() && !untrusted.errors().is_empty());
    }

    #[test]
    fn usage_counts_only_from_the_usage_stream_of_the_requested_session() {
        let journal = [
            usage(3),
            terminal("completed", r#"{"verdict":"met","reason":"ok"}"#),
            // A run_usage payload on any other stream kind is not ours.
            record(
                "session",
                SID,
                serde_json::json!({"kind": "run_usage", "completed_calls": 7}),
            ),
        ]
        .join("\n");
        let parsed = MuseCli.parse(&journal, Ok(String::new()), Some(SID));
        assert!(parsed.errors().is_empty(), "{:?}", parsed.errors());
        assert_eq!(parsed.turns(), 3);
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
        assert!(parsed.errors().is_empty(), "{:?}", parsed.errors());
        assert_eq!(parsed.verdict().unwrap().verdict, Verdict::NotMet);
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
            // A completed terminal record carrying no text at all.
            record(
                "session",
                SID,
                serde_json::json!({
                    "kind": "run_terminal", "terminal": "completed", "text": null,
                    "reason": null
                }),
            ),
            // A terminal record on our UUID but not on the session stream.
            record(
                "exec.usage",
                SID,
                serde_json::json!({
                    "kind": "run_terminal", "terminal": "completed",
                    "text": r#"{"verdict":"met","reason":"x"}"#, "reason": null
                }),
            ),
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
        ];
        for journal in cases {
            let parsed = MuseCli.parse(&journal, Ok(String::new()), Some(SID));
            assert!(
                parsed.verdict().is_none(),
                "trusted a verdict from: {journal}"
            );
            assert!(!parsed.errors().is_empty(), "no error for: {journal}");
        }
    }

    #[test]
    fn more_than_one_terminal_record_for_the_requested_session_trusts_nothing() {
        let met = terminal("completed", r#"{"verdict":"met","reason":"x"}"#);
        for second in [terminal("failed", ""), met.clone()] {
            let journal = [met.clone(), second].join("\n");
            let parsed = MuseCli.parse(&journal, Ok(String::new()), Some(SID));
            assert!(parsed.verdict().is_none(), "trusted: {journal}");
            assert!(
                parsed.errors().contains(
                    &"Muse journal has 2 terminal records for the requested session; expected exactly one"
                        .to_owned()
                ),
                "{:?}",
                parsed.errors()
            );
        }
    }

    #[test]
    fn failure_reasons_are_specific_and_bounded() {
        let errors = |journal: &str| {
            MuseCli
                .parse(journal, Ok(String::new()), Some(SID))
                .errors()
                .to_vec()
        };
        // No terminal record: say so without guessing a cause.
        let none = errors(&usage(1));
        assert!(
            none.contains(&"Muse run emitted no terminal record".to_owned()),
            "{none:?}"
        );
        // Repeated malformed lines collapse into one error.
        let ok = terminal("completed", r#"{"verdict":"met","reason":"x"}"#);
        let malformed = errors(&format!("{ok}\nnot json\n\nalso bad"));
        assert_eq!(
            malformed
                .iter()
                .filter(|e| e.contains("malformed"))
                .collect::<Vec<_>>(),
            ["2 malformed Muse JSONL line(s) (first at line 2)"]
        );
        // The session the journal never names is the one it should have.
        let other = record(
            "session",
            "11111111-2222-3333-4444-555555555555",
            serde_json::json!({"kind": "x"}),
        );
        let unnamed = errors(&other);
        assert!(
            unnamed
                .iter()
                .any(|e| e.contains("never names") && e.contains(SID)),
            "{unnamed:?}"
        );
        // An invalid verdict quotes a bounded excerpt of the text.
        let invalid = errors(&terminal("completed", "not json at all"));
        assert!(
            invalid
                .iter()
                .any(|e| e.contains("invalid Muse final verdict") && e.contains("not json at all")),
            "{invalid:?}"
        );
        let long = errors(&terminal("completed", &"y".repeat(500)));
        assert!(
            long.iter().all(|e| !e.contains(&"y".repeat(121))),
            "{long:?}"
        );
    }

    #[test]
    fn another_sessions_terminal_after_ours_leaves_ours_trusted() {
        let journal = [
            terminal("completed", r#"{"verdict":"met","reason":"ok"}"#),
            record(
                "session",
                "11111111-2222-3333-4444-555555555555",
                serde_json::json!({
                    "kind": "run_terminal", "terminal": "failed", "text": "", "reason": "x"
                }),
            ),
        ]
        .join("\n");
        let parsed = MuseCli.parse(&journal, Ok(String::new()), Some(SID));
        assert!(parsed.errors().is_empty(), "{:?}", parsed.errors());
        assert_eq!(parsed.verdict().unwrap().verdict, Verdict::Met);
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
        assert!(parsed.errors().is_empty(), "{:?}", parsed.errors());
        assert_eq!(parsed.native_id(), Some(SID));
    }

    /// Muse reads its prompt from one file: the condition, byte for byte,
    /// beside the invocation's schema marker. The core creates it.
    #[test]
    fn the_only_spawn_file_is_the_exact_condition_as_the_prompt() {
        let s = spec(SessionMode::New {
            session_uuid: SID.into(),
        });
        assert_eq!(
            MuseCli.spawn_files(&s, &artifacts()),
            vec![SpawnFile {
                path: prompt_path(&artifacts()),
                contents: b"make the gate pass".to_vec(),
            }]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_stubbed_muse_run_is_met_only_with_its_own_session_and_exit_zero() {
        use crate::autopilot::invocation::contract_tests::{await_first_sample, binary, setup};
        let wait = await_first_sample();
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
                     printf '%s\\n' '{}'\nprintf '%s\\n' '{}'\n{wait}\nexit {code}\n",
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

    /// The refusal is read from the raw stderr file, so a long preamble that
    /// would push it out of the bounded reason does not hide it, and a
    /// recovering controller reads the same answer from disk.
    #[cfg(unix)]
    #[test]
    fn a_supervised_muse_refusal_is_classified_from_its_stderr_file() {
        use crate::autopilot::dispatch::SessionRefusal;
        use crate::autopilot::invocation::contract_tests::{await_first_sample, binary, setup};
        let wait = await_first_sample();
        let preamble = "muse: notice ".repeat(80);
        for (line, code, expected) in [
            (
                format!("session {SID} is already in use"),
                1,
                Some(SessionRefusal::InUse),
            ),
            (
                format!(
                    "failed to read retained session {SID}: session {SID} was created with \
                     provider echo; refusing to resume with provider meta; start a new session \
                     to switch providers"
                ),
                1,
                Some(SessionRefusal::Unresumable),
            ),
            // A run that exits zero refused nothing, whatever it printed.
            (format!("session {SID} is already in use"), 0, None),
        ] {
            let (repo, mut s) = setup();
            s.implementer = Implementer::Muse;
            s.session = SessionMode::New {
                session_uuid: SID.into(),
            };
            let bin = binary(
                repo.path(),
                &format!(
                    "printf '%s\\n' '{preamble}' >&2\nprintf '%s\\n' '{line}' >&2\n\
                     {wait}\nexit {code}\n"
                ),
            );
            let outcome = crate::autopilot::invocation::run_supervised_bounded(
                &bin,
                repo.path(),
                &s,
                std::time::Duration::from_secs(20),
            )
            .unwrap();
            assert_eq!(outcome.session_refusal, expected, "{line}: {outcome:?}");
            let recovered = crate::autopilot::invocation::read_artifacts(
                Implementer::Muse,
                s.invocation_artifacts.as_ref().unwrap(),
            )
            .unwrap();
            assert_eq!(recovered.session_refusal, expected, "{line}: {recovered:?}");
        }
    }

    /// A run killed at its wall-clock timeout, or one that never launched,
    /// refused nothing, whatever its stderr says: only a launched run that
    /// exited non-zero on its own is asked. Otherwise a quoted "in use"
    /// line would park the issue instead of retrying it.
    #[cfg(unix)]
    #[test]
    fn a_timed_out_or_never_launched_run_is_no_session_refusal() {
        use crate::autopilot::invocation::contract_tests::{binary, setup};
        let in_use = format!("session {SID} is already in use");
        let (repo, mut s) = setup();
        s.implementer = Implementer::Muse;
        s.session = SessionMode::New {
            session_uuid: SID.into(),
        };
        let bin = binary(
            repo.path(),
            &format!("printf '%s\\n' '{in_use}' >&2\nsleep 30\nexit 1\n"),
        );
        let outcome = crate::autopilot::invocation::run_supervised_bounded(
            &bin,
            repo.path(),
            &s,
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        assert!(outcome.timed_out, "{outcome:?}");
        assert_eq!(outcome.session_refusal, None, "{outcome:?}");
        // A supervisor record that the spawn failed, beside the same line.
        let (_repo, mut s) = setup();
        s.implementer = Implementer::Muse;
        let a = s.invocation_artifacts.as_ref().unwrap();
        std::fs::write(&a.stderr_path, format!("{in_use}\n")).unwrap();
        std::fs::write(
            &a.exit_path,
            serde_json::json!({
                "completion": {
                    "version": 1, "invocation_id": a.invocation_id, "schema_path": a.schema_path
                },
                "exit_code": null, "timed_out": false, "limit_exceeded": null,
                "expected_native_id": SID, "launch_failure": "failed to launch Muse dispatch"
            })
            .to_string(),
        )
        .unwrap();
        let recovered = crate::autopilot::invocation::read_artifacts(Implementer::Muse, a).unwrap();
        assert!(!recovered.invocation_started, "{recovered:?}");
        assert_eq!(recovered.session_refusal, None, "{recovered:?}");
    }

    /// The parser already names the missing session; the core's generic
    /// mismatch would only spend the bounded reason on a repeat.
    #[cfg(unix)]
    #[test]
    fn a_journal_on_another_session_is_reported_once() {
        use crate::autopilot::invocation::contract_tests::{await_first_sample, binary, setup};
        let wait = await_first_sample();
        let (repo, mut s) = setup();
        s.implementer = Implementer::Muse;
        s.session = SessionMode::New {
            session_uuid: SID.into(),
        };
        let other = "11111111-2222-3333-4444-555555555555";
        let bin = binary(
            repo.path(),
            &format!(
                "printf '%s\\n' '{}'\n{wait}\n",
                record(
                    "session",
                    other,
                    serde_json::json!({
                        "kind": "run_terminal", "terminal": "completed",
                        "text": r#"{"verdict":"met","reason":"x"}"#, "reason": null
                    })
                )
            ),
        );
        let outcome = crate::autopilot::invocation::run_supervised_bounded(
            &bin,
            repo.path(),
            &s,
            std::time::Duration::from_secs(20),
        )
        .unwrap();
        assert!(!outcome.is_met());
        assert_eq!(outcome.native_session_id, None);
        let reason = outcome.reason.unwrap_or_default();
        assert!(
            reason.contains(&format!("never names the requested session {SID}")),
            "{reason}"
        );
        assert!(
            !reason.contains("native session ID did not match"),
            "{reason}"
        );
    }
}

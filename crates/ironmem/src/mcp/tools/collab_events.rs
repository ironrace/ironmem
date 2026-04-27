use serde_json::Value;

use crate::collab::{CollabEvent, Phase};
use crate::error::MemoryError;

use super::shared::sha256_hex;

/// Maximum length (chars) for `coding_failure` on a failure_report. Matches
/// the CHECK constraint in migration 005 so the DB and MCP layer agree. The
/// outer `content` cap (MAX_COLLAB_CONTENT_CHARS) is larger — this per-field
/// cap prevents a caller from filling the whole content budget with one
/// unbounded string.
const MAX_CODING_FAILURE_CHARS: usize = 2048;

/// Maximum length (chars) for `pr_url` on a final_review event. Matches the
/// CHECK constraint in migration 005.
const MAX_PR_URL_CHARS: usize = 2048;

/// Translate a `(topic, content)` send into a `CollabEvent`. Dispatch is
/// split into v1 planning and v3 coding groups so each sub-function stays
/// under the file's 50-line function guideline. Phase disambiguation is
/// minimal post-batch-refactor — only `final` carries any phase coupling,
/// and that's just an early-out friendlier-error guard rather than a real
/// dispatch split.
pub(super) fn build_collab_event(
    topic: &str,
    content: &str,
    phase: Phase,
) -> Result<CollabEvent, MemoryError> {
    match topic {
        "draft" | "canonical" => build_v1_plan_event(topic, content),
        "review" => build_v1_review_event(content),
        "final" => build_v1_final_event(content, phase),
        "task_list"
        | "implementation_done"
        | "review_local"
        | "review_fix_global"
        | "final_review"
        | "failure_report" => build_v3_coding_event(topic, content),
        other => Err(MemoryError::Validation(format!(
            "unknown collab topic: {other}"
        ))),
    }
}

/// v1 planning topics with no phase overloading. `draft` and `canonical` hash
/// the raw content and carry no structured payload.
pub(super) fn build_v1_plan_event(topic: &str, content: &str) -> Result<CollabEvent, MemoryError> {
    match topic {
        "draft" => Ok(CollabEvent::SubmitDraft {
            content_hash: sha256_hex(content),
        }),
        "canonical" => Ok(CollabEvent::PublishCanonical {
            content_hash: sha256_hex(content),
        }),
        _ => unreachable!("build_v1_plan_event called with non-v1 topic: {topic}"),
    }
}

/// v1 `review` topic — plan-only. v3 batch mode has no per-task review topic
/// (Codex only participates at the global review stage).
pub(super) fn build_v1_review_event(content: &str) -> Result<CollabEvent, MemoryError> {
    Ok(CollabEvent::SubmitReview {
        verdict: parse_review_verdict(content)?,
    })
}

/// v1 plan finalization. `final` was previously phase-overloaded (also used
/// by v3 per-task `CodeFinal`), but v3 batch mode removed that path entirely.
/// Topic dispatch now emits `PublishFinal` unconditionally; we keep an
/// explicit early-out guard here so a caller sending `final` outside
/// `PlanClaudeFinalizePending` gets a clear "expected phase" message
/// rather than a generic `WrongPhase` from the state machine.
pub(super) fn build_v1_final_event(
    content: &str,
    phase: Phase,
) -> Result<CollabEvent, MemoryError> {
    if !matches!(phase, Phase::PlanClaudeFinalizePending) {
        return Err(MemoryError::Validation(format!(
            "topic 'final' is only accepted in PlanClaudeFinalizePending; current phase is {phase}"
        )));
    }
    let plan = parse_final_payload(content)?;
    Ok(CollabEvent::PublishFinal {
        content_hash: sha256_hex(&plan),
    })
}

/// v3 coding topics. Batch mode: Claude orchestrates per-task subagents
/// inline and signals completion via `implementation_done`; Codex only
/// participates at the global review stage.
pub(super) fn build_v3_coding_event(
    topic: &str,
    content: &str,
) -> Result<CollabEvent, MemoryError> {
    match topic {
        "task_list" => parse_task_list_event(content),
        "implementation_done" => Ok(CollabEvent::ImplementationDone {
            head_sha: parse_required_head_sha(content, "implementation_done")?,
        }),
        "review_local" => Ok(CollabEvent::ReviewLocal {
            head_sha: parse_required_head_sha(content, "review_local")?,
        }),
        "review_fix_global" => Ok(CollabEvent::CodeReviewFixGlobal {
            head_sha: parse_required_head_sha(content, "review_fix_global")?,
        }),
        "final_review" => parse_final_review_event(content),
        "failure_report" => parse_failure_report_event(content),
        _ => unreachable!("build_v3_coding_event called with non-v3 topic: {topic}"),
    }
}

pub(super) fn parse_final_review_event(content: &str) -> Result<CollabEvent, MemoryError> {
    let payload: Value = serde_json::from_str(content)
        .map_err(|e| MemoryError::Validation(format!("final_review content must be JSON: {e}")))?;
    let head_sha = extract_required_str(&payload, "head_sha", "final_review")?;
    let pr_url = extract_required_str(&payload, "pr_url", "final_review")?;
    if pr_url.chars().count() > MAX_PR_URL_CHARS {
        return Err(MemoryError::Validation(format!(
            "final_review pr_url exceeds {MAX_PR_URL_CHARS} chars",
        )));
    }
    // Only https URLs are accepted — a javascript:/file:// URL here could
    // become an open-redirect or SSRF if any downstream consumer renders it.
    if !pr_url.starts_with("https://") {
        return Err(MemoryError::Validation(
            "final_review pr_url must start with https://".to_string(),
        ));
    }
    Ok(CollabEvent::FinalReview { head_sha, pr_url })
}

pub(super) fn parse_failure_report_event(content: &str) -> Result<CollabEvent, MemoryError> {
    let payload: Value = serde_json::from_str(content).map_err(|e| {
        MemoryError::Validation(format!("failure_report content must be JSON: {e}"))
    })?;
    let coding_failure = payload
        .get("coding_failure")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            MemoryError::Validation(
                "failure_report content must include a non-empty \"coding_failure\" field"
                    .to_string(),
            )
        })?
        .to_string();
    if coding_failure.chars().count() > MAX_CODING_FAILURE_CHARS {
        return Err(MemoryError::Validation(format!(
            "failure_report coding_failure exceeds {MAX_CODING_FAILURE_CHARS} chars",
        )));
    }
    Ok(CollabEvent::FailureReport { coding_failure })
}

/// Best-effort check for any prefix that the state machine admits from a
/// non-owner agent (branch drift, Codex dispatch failure). Returns false
/// on any JSON parse failure so malformed payloads still fall through to
/// the main `parse_failure_report_event` validation.
pub(super) fn failure_report_is_off_turn_admissible(content: &str) -> bool {
    serde_json::from_str::<Value>(content)
        .ok()
        .and_then(|v| {
            v.get("coding_failure").and_then(Value::as_str).map(|s| {
                crate::collab::OFF_TURN_FAILURE_PREFIXES
                    .iter()
                    .any(|prefix| s.starts_with(prefix))
            })
        })
        .unwrap_or(false)
}

/// Allowed values for `execution_mode` on a `task_list` payload.
/// Absence means default (subagent-driven). The string `"subagent_driven"` is
/// intentionally NOT in this set — callers omit the field for the default path.
const ALLOWED_EXECUTION_MODES: &[&str] = &["mechanical_direct"];

/// Parse and validate the task_list payload shape. Fails fast on missing
/// fields, empty task array, missing acceptance criteria, or non-array tasks.
/// The state machine re-checks plan_hash, base_sha presence, and task count.
///
/// Optional `plan_file_path`: if present, must be non-empty, repo-relative
/// (no leading `/`), and contain no `..` path segments. Persisted on the
/// session (via the canonicalized `task_list` JSON) so reviewers can locate
/// the writing-plans markdown that drove subagent execution.
///
/// Optional `execution_mode`: if present, must be one of the allowed values in
/// `ALLOWED_EXECUTION_MODES`. Unknown values are rejected immediately so a
/// typo in the dispatcher fails at submit time rather than silently defaulting
/// to subagent-driven behaviour. Absence means the default (subagent-driven).
pub(super) fn parse_task_list_event(content: &str) -> Result<CollabEvent, MemoryError> {
    let payload: Value = serde_json::from_str(content).map_err(|e| {
        MemoryError::Validation(format!(
            "task_list content must be JSON shaped like {{\"plan_hash\":\"…\",\"base_sha\":\"…\",\"head_sha\":\"…\",\"plan_file_path\":\"docs/…\",\"tasks\":[{{\"id\":1,\"title\":\"…\",\"acceptance\":[\"…\"]}}]}} (parse error: {e})"
        ))
    })?;
    let plan_hash = payload
        .get("plan_hash")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            MemoryError::Validation("task_list missing non-empty plan_hash".to_string())
        })?
        .to_string();
    let base_sha = payload
        .get("base_sha")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| MemoryError::Validation("task_list missing non-empty base_sha".to_string()))?
        .to_string();
    let head_sha = payload
        .get("head_sha")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| MemoryError::Validation("task_list missing non-empty head_sha".to_string()))?
        .to_string();
    if let Some(raw) = payload.get("plan_file_path") {
        let path = raw.as_str().ok_or_else(|| {
            MemoryError::Validation("task_list plan_file_path must be a string".to_string())
        })?;
        validate_plan_file_path(path)?;
    }
    if let Some(raw) = payload.get("execution_mode") {
        let mode = raw.as_str().ok_or_else(|| {
            MemoryError::Validation("task_list execution_mode must be a string".to_string())
        })?;
        if !ALLOWED_EXECUTION_MODES.contains(&mode) {
            return Err(MemoryError::Validation(format!(
                "task_list execution_mode \"{}\" is not allowed; allowed values: [{}]",
                mode,
                ALLOWED_EXECUTION_MODES
                    .iter()
                    .map(|v| format!("\"{}\"", v))
                    .collect::<Vec<_>>()
                    .join(", "),
            )));
        }
    }
    let tasks = payload
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| MemoryError::Validation("task_list missing \"tasks\" array".to_string()))?;
    if tasks.is_empty() {
        return Err(MemoryError::Validation(
            "task_list must contain at least one task".to_string(),
        ));
    }
    let mut last_id: Option<i64> = None;
    for (idx, task) in tasks.iter().enumerate() {
        let task_id = task.get("id").and_then(Value::as_i64).ok_or_else(|| {
            MemoryError::Validation(format!("task_list task[{idx}] missing integer \"id\""))
        })?;
        if let Some(prev) = last_id {
            if task_id <= prev {
                return Err(MemoryError::Validation(format!(
                    "task_list tasks must be strictly ordered by id (task[{idx}].id={task_id} follows {prev})"
                )));
            }
        }
        last_id = Some(task_id);
        let acceptance = task
            .get("acceptance")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                MemoryError::Validation(format!(
                    "task_list task[{idx}] missing \"acceptance\" array"
                ))
            })?;
        if acceptance.is_empty() {
            return Err(MemoryError::Validation(format!(
                "task_list task[{idx}] must include at least one acceptance criterion"
            )));
        }
    }
    let tasks_count = u32::try_from(tasks.len())
        .map_err(|_| MemoryError::Validation("task_list contains too many tasks".to_string()))?;
    // Canonicalize the task_list JSON we store on the session so downstream
    // readers see a normalized form regardless of incoming whitespace.
    let task_list_json = serde_json::to_string(&payload)
        .map_err(|e| MemoryError::Validation(format!("task_list serialize error: {e}")))?;
    Ok(CollabEvent::SubmitTaskList {
        plan_hash,
        base_sha,
        task_list_json,
        tasks_count,
        head_sha,
    })
}

/// Validate the optional `plan_file_path` field on a `task_list` payload.
///
/// The field is consumed by the Codex prompt as a literal repo-relative
/// path opened from `repo_path`. Any input that escapes the repo or
/// smuggles control bytes is potentially exploitable as an arbitrary-file-
/// read for the agent that follows the prompt. This validator therefore
/// applies a strict allowlist:
///
/// - Non-empty, ≤512 chars (matches caps elsewhere in this module).
/// - Must parse as a sequence of `Component::Normal` segments only —
///   `ParentDir` (`..`), `RootDir` (`/...`), `Prefix` (Windows-style
///   `C:\\...` or `\\?\` UNC), and `CurDir` (`.`) are all rejected.
///   `Path::components` normalizes the path before iteration, so a value
///   like `docs\..\..\etc\passwd` parses as a single `Normal` segment on
///   POSIX (where `\` is a literal char) — that's still safe because it
///   resolves to a literal filename, but a future cross-platform port
///   would catch it via the `Prefix` arm.
/// - No control bytes: rejects `\0` (path-truncation in C interop) and
///   any character below `0x20` other than tab.
/// - No percent-encoded escape sequences: a `%` byte is permitted only as
///   a literal filename character, not as the start of a `%2e`/`%2f`
///   pair, since downstream consumers may URL-decode unsafely. Easier to
///   refuse the whole class than enumerate decoders.
fn validate_plan_file_path(path: &str) -> Result<(), MemoryError> {
    use std::path::{Component, Path};

    const MAX_LEN: usize = 512;

    if path.is_empty() {
        return Err(MemoryError::Validation(
            "task_list plan_file_path must be non-empty when present".to_string(),
        ));
    }
    if path.chars().count() > MAX_LEN {
        return Err(MemoryError::Validation(format!(
            "task_list plan_file_path exceeds {MAX_LEN} chars"
        )));
    }
    if path.bytes().any(|b| b == 0 || (b < 0x20 && b != b'\t')) {
        return Err(MemoryError::Validation(
            "task_list plan_file_path must not contain control bytes (incl. NUL)".to_string(),
        ));
    }
    if path.contains('%') {
        return Err(MemoryError::Validation(
            "task_list plan_file_path must not contain percent-encoded sequences (no '%' allowed)"
                .to_string(),
        ));
    }
    for component in Path::new(path).components() {
        match component {
            Component::Normal(_) => {}
            Component::ParentDir => {
                return Err(MemoryError::Validation(
                    "task_list plan_file_path must not contain '..' segments".to_string(),
                ));
            }
            Component::RootDir => {
                return Err(MemoryError::Validation(
                    "task_list plan_file_path must be repo-relative (no leading '/')".to_string(),
                ));
            }
            Component::Prefix(_) => {
                return Err(MemoryError::Validation(
                    "task_list plan_file_path must not contain a path prefix (e.g. drive letter or UNC root)"
                        .to_string(),
                ));
            }
            Component::CurDir => {
                return Err(MemoryError::Validation(
                    "task_list plan_file_path must not contain '.' segments".to_string(),
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn parse_required_head_sha(content: &str, topic: &str) -> Result<String, MemoryError> {
    let payload: Value = serde_json::from_str(content)
        .map_err(|e| MemoryError::Validation(format!("{topic} content must be JSON: {e}")))?;
    extract_required_str(&payload, "head_sha", topic)
}

/// Pull a non-empty string field out of a parsed JSON payload with a uniform
/// validation error.
pub(super) fn extract_required_str(
    payload: &Value,
    field: &str,
    topic: &str,
) -> Result<String, MemoryError> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            MemoryError::Validation(format!(
                "{topic} content must include a non-empty \"{field}\" field"
            ))
        })
}

pub(super) fn parse_review_verdict(content: &str) -> Result<String, MemoryError> {
    let payload: Value = serde_json::from_str(content).map_err(|e| {
        MemoryError::Validation(format!(
            "review content must be JSON shaped like {{\"verdict\":\"approve|approve_with_minor_edits|request_changes\",\"notes\":[\"...\"]}} (parse error: {e})"
        ))
    })?;
    let verdict = payload
        .get("verdict")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            MemoryError::Validation(
                "review content must include a \"verdict\" string field".to_string(),
            )
        })?;
    Ok(verdict.to_string())
}

pub(super) fn parse_final_payload(content: &str) -> Result<String, MemoryError> {
    let payload: Value = serde_json::from_str(content).map_err(|e| {
        MemoryError::Validation(format!(
            "final content must be JSON shaped like {{\"plan\":\"<full plan text>\"}} (parse error: {e})"
        ))
    })?;
    let plan = payload.get("plan").and_then(Value::as_str).ok_or_else(|| {
        MemoryError::Validation("final content must include a \"plan\" string field".to_string())
    })?;
    Ok(plan.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_required_str_pins_error_format() {
        let payload = json!({ "head_sha": "abc123", "empty": "", "n": 3 });
        assert_eq!(
            extract_required_str(&payload, "head_sha", "implement")
                .expect("head_sha should extract successfully"),
            "abc123"
        );
        let missing = extract_required_str(&payload, "pr_url", "final_review").unwrap_err();
        assert_eq!(
            missing.to_string(),
            "Validation error: final_review content must include a non-empty \"pr_url\" field"
        );
        let empty = extract_required_str(&payload, "empty", "review_fix").unwrap_err();
        assert!(empty.to_string().contains("non-empty \"empty\" field"));
        let wrong_type = extract_required_str(&payload, "n", "review_fix").unwrap_err();
        assert!(wrong_type.to_string().contains("non-empty \"n\" field"));
    }

    fn task_list_with_plan_file_path(path: serde_json::Value) -> String {
        let mut payload = json!({
            "plan_hash": "h",
            "base_sha": "b",
            "head_sha": "head",
            "tasks": [{ "id": 1, "title": "t", "acceptance": ["ok"] }],
        });
        payload
            .as_object_mut()
            .unwrap()
            .insert("plan_file_path".to_string(), path);
        payload.to_string()
    }

    #[test]
    fn task_list_accepts_optional_plan_file_path() {
        let raw = task_list_with_plan_file_path(json!("docs/superpowers/plans/today-feature.md"));
        let event = parse_task_list_event(&raw).expect("valid plan_file_path should parse");
        let CollabEvent::SubmitTaskList { task_list_json, .. } = event else {
            panic!("expected SubmitTaskList event");
        };
        // Canonicalized JSON must round-trip the field so reviewers can find
        // the markdown plan that drove subagent execution.
        assert!(
            task_list_json.contains("docs/superpowers/plans/today-feature.md"),
            "plan_file_path should be preserved in canonicalized task_list, got: {task_list_json}",
        );
    }

    #[test]
    fn task_list_rejects_non_string_plan_file_path() {
        let raw = task_list_with_plan_file_path(json!(42));
        let err = parse_task_list_event(&raw).unwrap_err();
        assert!(err.to_string().contains("plan_file_path must be a string"));
    }

    #[test]
    fn task_list_rejects_empty_plan_file_path() {
        let raw = task_list_with_plan_file_path(json!(""));
        let err = parse_task_list_event(&raw).unwrap_err();
        assert!(err.to_string().contains("plan_file_path must be non-empty"));
    }

    #[test]
    fn task_list_rejects_absolute_plan_file_path() {
        let raw = task_list_with_plan_file_path(json!("/etc/passwd"));
        let err = parse_task_list_event(&raw).unwrap_err();
        assert!(err.to_string().contains("repo-relative"));
    }

    #[test]
    fn task_list_rejects_dotdot_segment() {
        let raw = task_list_with_plan_file_path(json!("docs/../../etc/passwd"));
        let err = parse_task_list_event(&raw).unwrap_err();
        assert!(err.to_string().contains("'..' segments"));
    }

    #[test]
    fn task_list_rejects_curdir_segment() {
        let raw = task_list_with_plan_file_path(json!("./docs/plan.md"));
        let err = parse_task_list_event(&raw).unwrap_err();
        assert!(err.to_string().contains("'.' segments"));
    }

    #[test]
    fn task_list_rejects_null_byte_in_plan_file_path() {
        let raw = task_list_with_plan_file_path(json!("docs/plan\0.md"));
        let err = parse_task_list_event(&raw).unwrap_err();
        assert!(err.to_string().contains("control bytes"));
    }

    #[test]
    fn task_list_rejects_percent_encoded_plan_file_path() {
        let raw = task_list_with_plan_file_path(json!("docs/%2e%2e/etc/passwd"));
        let err = parse_task_list_event(&raw).unwrap_err();
        assert!(err.to_string().contains("percent-encoded"));
    }

    #[test]
    fn task_list_rejects_oversized_plan_file_path() {
        let mut huge = String::from("docs/");
        huge.push_str(&"a".repeat(600));
        let raw = task_list_with_plan_file_path(json!(huge));
        let err = parse_task_list_event(&raw).unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn task_list_rejects_windows_drive_prefix_when_run_on_windows() {
        // Path::components only parses `C:\…` as a `Prefix` on Windows.
        // On POSIX, it's a single `Normal` segment ("C:\foo"). Run a
        // POSIX-safe check: the ParentDir / Prefix arms are exercised by
        // the other tests; here just verify a literal backslash filename
        // is allowed (no false positive from a future regex-based fix).
        let raw = task_list_with_plan_file_path(json!("docs\\plan.md"));
        let event = parse_task_list_event(&raw).expect("backslash literal must round-trip");
        let CollabEvent::SubmitTaskList { task_list_json, .. } = event else {
            panic!("expected SubmitTaskList");
        };
        assert!(
            task_list_json.contains("docs\\\\plan.md") || task_list_json.contains("docs\\plan")
        );
    }

    // ── execution_mode field ──────────────────────────────────────────────────

    fn base_task_list() -> serde_json::Value {
        json!({
            "plan_hash": "h",
            "base_sha": "b",
            "head_sha": "head",
            "tasks": [{ "id": 1, "title": "t", "acceptance": ["ok"] }],
        })
    }

    #[test]
    fn task_list_accepts_mechanical_direct() {
        let mut payload = base_task_list();
        payload
            .as_object_mut()
            .unwrap()
            .insert("execution_mode".to_string(), json!("mechanical_direct"));
        let event =
            parse_task_list_event(&payload.to_string()).expect("mechanical_direct should be valid");
        let CollabEvent::SubmitTaskList { task_list_json, .. } = event else {
            panic!("expected SubmitTaskList");
        };
        // Field must survive canonicalization so collab_status can return it.
        assert!(
            task_list_json.contains("mechanical_direct"),
            "execution_mode must be preserved in canonical task_list JSON, got: {task_list_json}",
        );
    }

    #[test]
    fn task_list_accepts_omitted_execution_mode_as_default() {
        // No execution_mode key → should parse successfully (default path).
        let payload = base_task_list();
        assert!(
            !payload.as_object().unwrap().contains_key("execution_mode"),
            "base_task_list fixture must not include execution_mode"
        );
        parse_task_list_event(&payload.to_string())
            .expect("omitted execution_mode should be accepted as default");
    }

    #[test]
    fn task_list_rejects_unknown_execution_mode() {
        let mut payload = base_task_list();
        payload
            .as_object_mut()
            .unwrap()
            .insert("execution_mode".to_string(), json!("subagent_driven"));
        let err = parse_task_list_event(&payload.to_string()).unwrap_err();
        assert!(
            err.to_string().contains("execution_mode"),
            "error must mention execution_mode, got: {err}"
        );
        assert!(
            err.to_string().contains("not allowed"),
            "error must say 'not allowed', got: {err}"
        );
    }

    #[test]
    fn task_list_rejects_non_string_execution_mode() {
        let mut payload = base_task_list();
        payload
            .as_object_mut()
            .unwrap()
            .insert("execution_mode".to_string(), json!(42));
        let err = parse_task_list_event(&payload.to_string()).unwrap_err();
        assert!(
            err.to_string().contains("execution_mode must be a string"),
            "error must say execution_mode must be a string, got: {err}"
        );
    }
}

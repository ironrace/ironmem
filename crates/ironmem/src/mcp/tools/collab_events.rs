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

/// Maximum length (chars) for `pr_url` on a pr_opened event. Matches the
/// CHECK constraint in migration 005.
const MAX_PR_URL_CHARS: usize = 2048;

/// Translate a `(topic, content)` send into a `CollabEvent` using the session's
/// current phase to disambiguate v1/v2-overloaded topics (`review`, `final`).
/// Dispatch is split into v1 planning, phase-overloaded, and v2 coding groups
/// so each sub-function stays under the file's 50-line function guideline.
pub(super) fn build_collab_event(
    topic: &str,
    content: &str,
    session: &crate::collab::CollabSession,
) -> Result<CollabEvent, MemoryError> {
    match topic {
        "draft" | "canonical" => build_v1_plan_event(topic, content),
        "review" | "final" => build_overloaded_event(topic, content, &session.phase),
        "task_list" | "implement" | "verdict" | "comment" | "review_local" | "review_global"
        | "verdict_global" | "comment_global" | "final_review" | "pr_opened" | "failure_report" => {
            build_v2_coding_event(topic, content)
        }
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

/// Phase-overloaded topics (`review`, `final`) are shared across v1 planning
/// and v2 per-task coding. An explicit phase whitelist picks the right event
/// variant — anything outside the whitelist is rejected here so the caller
/// gets a clean `WrongPhase` instead of a cryptic JSON parse error downstream.
pub(super) fn build_overloaded_event(
    topic: &str,
    content: &str,
    phase: &Phase,
) -> Result<CollabEvent, MemoryError> {
    match (topic, phase) {
        ("review", Phase::PlanCodexReviewPending) => Ok(CollabEvent::SubmitReview {
            verdict: parse_review_verdict(content)?,
        }),
        ("review", Phase::CodeReviewPending) => {
            let head_sha = parse_required_head_sha(content, "review")?;
            Ok(CollabEvent::CodeReview { head_sha })
        }
        ("final", Phase::PlanClaudeFinalizePending) => {
            let plan = parse_final_payload(content)?;
            Ok(CollabEvent::PublishFinal {
                content_hash: sha256_hex(&plan),
            })
        }
        ("final", Phase::CodeFinalPending) => {
            let head_sha = parse_required_head_sha(content, "final")?;
            Ok(CollabEvent::CodeFinal { head_sha })
        }
        (topic, phase) => Err(MemoryError::Validation(format!(
            "topic '{topic}' is not accepted in phase {phase}; v1 expects it in PlanCodexReviewPending/PlanClaudeFinalizePending and v2 expects it in CodeReviewPending/CodeFinalPending",
        ))),
    }
}

/// v2 coding topics. Each payload is parsed once and required fields are
/// extracted in a single pass so `verdict_global` / `review_global` don't
/// double-parse their JSON for head_sha and verdict.
pub(super) fn build_v2_coding_event(
    topic: &str,
    content: &str,
) -> Result<CollabEvent, MemoryError> {
    match topic {
        "task_list" => parse_task_list_event(content),
        "implement" => Ok(CollabEvent::CodeImplement {
            head_sha: parse_required_head_sha(content, "implement")?,
        }),
        "verdict" => {
            let (head_sha, verdict) = parse_head_sha_and_verdict(content, "verdict")?;
            Ok(CollabEvent::CodeVerdict { verdict, head_sha })
        }
        "comment" => Ok(CollabEvent::CodeComment {
            head_sha: parse_required_head_sha(content, "comment")?,
        }),
        "review_local" => Ok(CollabEvent::ReviewLocal {
            head_sha: parse_required_head_sha(content, "review_local")?,
        }),
        "review_global" => {
            let (head_sha, verdict) = parse_head_sha_and_verdict(content, "review_global")?;
            Ok(CollabEvent::ReviewGlobal { verdict, head_sha })
        }
        "verdict_global" => {
            let (head_sha, verdict) = parse_head_sha_and_verdict(content, "verdict_global")?;
            Ok(CollabEvent::VerdictGlobal { verdict, head_sha })
        }
        "comment_global" => Ok(CollabEvent::CommentGlobal {
            head_sha: parse_required_head_sha(content, "comment_global")?,
        }),
        "final_review" => Ok(CollabEvent::FinalReview {
            head_sha: parse_required_head_sha(content, "final_review")?,
        }),
        "pr_opened" => parse_pr_opened_event(content),
        "failure_report" => parse_failure_report_event(content),
        _ => unreachable!("build_v2_coding_event called with non-v2 topic: {topic}"),
    }
}

pub(super) fn parse_pr_opened_event(content: &str) -> Result<CollabEvent, MemoryError> {
    let payload: Value = serde_json::from_str(content)
        .map_err(|e| MemoryError::Validation(format!("pr_opened content must be JSON: {e}")))?;
    let head_sha = extract_required_str(&payload, "head_sha", "pr_opened")?;
    let pr_url = extract_required_str(&payload, "pr_url", "pr_opened")?;
    if pr_url.chars().count() > MAX_PR_URL_CHARS {
        return Err(MemoryError::Validation(format!(
            "pr_opened pr_url exceeds {MAX_PR_URL_CHARS} chars",
        )));
    }
    // Only https URLs are accepted — a javascript:/file:// URL here could
    // become an open-redirect or SSRF if any downstream consumer renders it.
    if !pr_url.starts_with("https://") {
        return Err(MemoryError::Validation(
            "pr_opened pr_url must start with https://".to_string(),
        ));
    }
    Ok(CollabEvent::PrOpened { pr_url, head_sha })
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

/// Best-effort check for the `branch_drift:` prefix used by the upstream
/// turn gate. Returns false on any JSON parse failure so malformed payloads
/// still fall through to the main `parse_failure_report_event` validation.
pub(super) fn failure_report_is_branch_drift(content: &str) -> bool {
    serde_json::from_str::<Value>(content)
        .ok()
        .and_then(|v| {
            v.get("coding_failure")
                .and_then(Value::as_str)
                .map(|s| s.starts_with(crate::collab::BRANCH_DRIFT_PREFIX))
        })
        .unwrap_or(false)
}

/// Parse the JSON payload once and extract both `head_sha` and `verdict` in a
/// single pass. Used by `verdict` / `review_global` / `verdict_global` which
/// all need both fields.
pub(super) fn parse_head_sha_and_verdict(
    content: &str,
    topic: &str,
) -> Result<(String, String), MemoryError> {
    let payload: Value = serde_json::from_str(content)
        .map_err(|e| MemoryError::Validation(format!("{topic} content must be JSON: {e}")))?;
    let head_sha = extract_required_str(&payload, "head_sha", topic)?;
    let verdict = extract_required_str(&payload, "verdict", topic)?;
    Ok((head_sha, verdict))
}

/// Parse and validate the task_list payload shape. Fails fast on missing
/// fields, empty task array, missing acceptance criteria, or non-array tasks.
/// The state machine re-checks plan_hash, base_sha presence, and task count.
pub(super) fn parse_task_list_event(content: &str) -> Result<CollabEvent, MemoryError> {
    let payload: Value = serde_json::from_str(content).map_err(|e| {
        MemoryError::Validation(format!(
            "task_list content must be JSON shaped like {{\"plan_hash\":\"…\",\"base_sha\":\"…\",\"head_sha\":\"…\",\"tasks\":[{{\"id\":1,\"title\":\"…\",\"acceptance\":[\"…\"]}}]}} (parse error: {e})"
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
            extract_required_str(&payload, "head_sha", "implement").unwrap(),
            "abc123"
        );
        let missing = extract_required_str(&payload, "pr_url", "pr_opened").unwrap_err();
        assert_eq!(
            missing.to_string(),
            "Validation error: pr_opened content must include a non-empty \"pr_url\" field"
        );
        let empty = extract_required_str(&payload, "empty", "verdict").unwrap_err();
        assert!(empty.to_string().contains("non-empty \"empty\" field"));
        let wrong_type = extract_required_str(&payload, "n", "verdict").unwrap_err();
        assert!(wrong_type.to_string().contains("non-empty \"n\" field"));
    }
}

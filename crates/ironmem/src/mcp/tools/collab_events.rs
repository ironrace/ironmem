use serde_json::Value;

use crate::collab::{validate_task_list_body, CollabEvent, Phase};
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

/// Maximum characters of a rejected `head_sha` echoed back in the refusal
/// `parse_task_list_event` raises. Everywhere else a `head_sha` reaches a
/// message it has already passed `is_hex_sha` and is therefore at most 64
/// characters; that refusal is the one path where it did *not*, so it is the
/// one place an unbounded caller string would land verbatim in an MCP
/// response. Wide enough to show a full 64-char object name and still signal
/// that something followed it.
const MAX_ECHOED_HEAD_SHA_CHARS: usize = 80;

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

/// v1 `review` topic — plan-only. v3 batch mode has no per-task review topic;
/// Codex's branch-scope review uses `review_fix_global`.
pub(super) fn build_v1_review_event(content: &str) -> Result<CollabEvent, MemoryError> {
    Ok(CollabEvent::SubmitReview {
        verdict: parse_review_verdict(content)?,
    })
}

/// v1 plan finalization. `final` was previously phase-overloaded (also used
/// by v3 per-task `CodeFinal`), but v3 batch mode removed that path entirely.
/// Topic dispatch now emits `PublishFinal` unconditionally; we keep an
/// explicit early-out guard here so a caller sending `final` outside
/// `PlanFinalizePending` gets a clear "expected phase" message
/// rather than a generic `WrongPhase` from the state machine.
pub(super) fn build_v1_final_event(
    content: &str,
    phase: Phase,
) -> Result<CollabEvent, MemoryError> {
    if !matches!(phase, Phase::PlanFinalizePending) {
        return Err(MemoryError::Validation(format!(
            "topic 'final' is only accepted in {}; current phase is {phase}",
            Phase::PlanFinalizePending
        )));
    }
    let plan = parse_final_payload(content)?;
    Ok(CollabEvent::PublishFinal {
        content_hash: sha256_hex(&plan),
    })
}

/// v3 coding topics. Batch mode: the selected implementer orchestrates
/// per-task subagents and signals completion via `implementation_done`;
/// Codex owns the first branch-scope review pass afterward.
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
    let coding_failure = if crate::mcp::compact::should_compact_failure_reports() {
        crate::mcp::compact::compact_failure_log(&coding_failure, MAX_CODING_FAILURE_CHARS)
    } else {
        coding_failure
    };
    if coding_failure.chars().count() > MAX_CODING_FAILURE_CHARS {
        return Err(MemoryError::Validation(format!(
            "failure_report coding_failure exceeds {MAX_CODING_FAILURE_CHARS} chars",
        )));
    }
    Ok(CollabEvent::FailureReport { coding_failure })
}

/// Best-effort check for a contextually authorized off-turn report. Branch
/// drift is admissible from either agent in any coding-active phase (it is
/// Terminal, so admitting it ends the session rather than handing over a
/// turn); checkpoint drift is admissible from either agent but only from
/// `CodeImplementPending`; a Codex-dispatch failure requires Claude reporting
/// against a Codex-owned turn in a phase whose Codex turn Claude dispatches.
/// Returns false on any JSON parse failure so malformed payloads still fall
/// through to the main `parse_failure_report_event` validation.
pub(super) fn failure_report_is_off_turn_admissible(
    content: &str,
    reporter: crate::collab::Agent,
    current_owner: crate::collab::Agent,
    phase: Phase,
    implementer: crate::collab::Agent,
) -> bool {
    serde_json::from_str::<Value>(content)
        .ok()
        .and_then(|v| {
            v.get("coding_failure").and_then(Value::as_str).map(|s| {
                crate::collab::off_turn_failure_is_admissible(
                    s,
                    reporter,
                    current_owner,
                    phase,
                    implementer,
                )
            })
        })
        .unwrap_or(false)
}

/// Parse and validate the task_list payload shape. Fails fast on missing
/// fields, empty or oversized task arrays, missing acceptance criteria, or
/// non-array tasks. The state machine re-checks plan_hash, base_sha presence,
/// and the 15-task issue budget.
///
/// Optional `plan_file_path`: if present, must be non-empty, repo-relative
/// (no leading `/`), and contain no `..` path segments. Persisted on the
/// session (via the canonicalized `task_list` JSON) so reviewers can locate
/// the approved task markdown that drove subagent execution.
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
    // All three trimmed before the emptiness check, for the reason
    // `extract_required_str` records: they are transcribed out of a turn
    // template, so a leading tab or trailing space is an ordinary
    // transcription slip rather than a different value. For `head_sha` the
    // trim is load-bearing at this very call — the `is_hex_sha` guard below
    // refuses anything that is not 7-64 hex characters, and padding is
    // exactly that, so an untrimmed sha would be refused here despite naming
    // the commit the branch is actually at.
    let plan_hash = payload
        .get("plan_hash")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            MemoryError::Validation("task_list missing non-empty plan_hash".to_string())
        })?
        .to_string();
    let base_sha = payload
        .get("base_sha")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| MemoryError::Validation("task_list missing non-empty base_sha".to_string()))?
        .to_string();
    let head_sha = payload
        .get("head_sha")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| MemoryError::Validation("task_list missing non-empty head_sha".to_string()))?
        .to_string();
    // Shape-checked here, at the seed site, rather than only where the value
    // is later read back. `head_sha` becomes the session's `last_head_sha`,
    // and `validate_global_review_head_advance` cannot refuse a malformed
    // *stored* value without wedging the session — nothing rewrites that
    // field but a successful head-advancing send, which is exactly what the
    // refusal would block. Here the refusal is recoverable: it names a value
    // the caller is holding, in a phase where nothing has been written yet.
    //
    // Same `is_hex_sha` (7-64 hex chars) the advance guard applies, rather
    // than a second spelling of the rule. Shape only — whether the commit
    // exists is the git shell-out's question, and there is no repo path in
    // scope at this layer to ask it with.
    if !crate::code_maps::is_hex_sha(&head_sha) {
        // Bounded echo: see [`MAX_ECHOED_HEAD_SHA_CHARS`]. `echoed` is a
        // prefix of `head_sha`, so comparing byte lengths detects the cut.
        let echoed: String = head_sha.chars().take(MAX_ECHOED_HEAD_SHA_CHARS).collect();
        let ellipsis = if echoed.len() < head_sha.len() {
            "…"
        } else {
            ""
        };
        // The remedy leads, and it is a command rather than a description:
        // the reader here is an agent with a shell, and `git rev-parse HEAD`
        // is the whole fix. The template is referred to generically on
        // purpose — quoting its placeholder literally would couple this
        // string to a file that is being rewritten for the same reason this
        // check exists. The causes are listed rather than assumed, because a
        // short abbreviation reaches this refusal just as a revision
        // expression does and is not the same mistake.
        return Err(MemoryError::Validation(format!(
            "task_list head_sha {echoed}{ellipsis} is not a git object name. Run \
             `git rev-parse HEAD` in the session's repo and send the full \
             40-character sha it prints. This field must be 7-64 hex \
             characters: a revision expression such as HEAD or a branch name, \
             a placeholder copied out of the turn template, and an \
             abbreviation shorter than 7 characters are all refused here, \
             because none of them pins one commit — and this value becomes \
             the fixed point every later drift check in the session measures \
             against."
        )));
    }
    let tasks_count = validate_task_list_body(&payload)
        .map_err(|error| MemoryError::Validation(error.to_string()))?;
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
///
/// Trimmed before the emptiness check, which makes a whitespace-only value a
/// rejection rather than a stored blank. The trim also has to match
/// `checkpoint::optional_str`, the other route by which a `head_sha` reaches
/// the server: these values arrive transcribed out of a turn template, so a
/// trailing space is a normal transcription slip, and `require_checkpoint_proof`
/// compares the checkpoint's `head_sha` against this one with raw string
/// equality. Trimming on only one of the two paths would make `"abc123 "` and
/// `"abc123"` — the same commit, filed twice by the same agent — refuse each
/// other, and the refusal's paste-ready remedy would carry the padded value
/// straight back into the next attempt.
pub(super) fn extract_required_str(
    payload: &Value,
    field: &str,
    topic: &str,
) -> Result<String, MemoryError> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
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

    /// A representative 40-hex head sha, shared by every fixture in this
    /// module that needs a well-formed `head_sha` placeholder — real enough
    /// to satisfy `is_hex_sha`, not tied to any actual commit.
    const SHA: &str = "b9c2ce0f4d3a2b1c8e7f6a5b4c3d2e1f0a9b8c7d";

    #[test]
    fn final_rejection_uses_the_stable_wire_phase_name() {
        let error = build_v1_final_event(
            &json!({ "plan": "final plan" }).to_string(),
            Phase::PlanSynthesisPending,
        )
        .expect_err("final must be refused before the finalization phase");

        assert_eq!(
            error.to_string(),
            "Validation error: topic 'final' is only accepted in PlanClaudeFinalizePending; current phase is PlanSynthesisPending"
        );
    }

    // ── Task 9: off-turn admission regression guard ─────────────────────────

    /// Branch drift remains observable by either agent. A Codex dispatch
    /// failure is admissible only when Claude reports it while Codex owns the
    /// interrupted turn; the reverse direction would let Codex steal a Claude
    /// turn.
    #[test]
    fn off_turn_failure_admission_respects_reporter_and_owner() {
        assert!(failure_report_is_off_turn_admissible(
            &json!({"coding_failure": "branch_drift: head_sha abc not found"}).to_string(),
            crate::collab::Agent::Codex,
            crate::collab::Agent::Claude,
            Phase::CodeReviewFixGlobalPending,
            crate::collab::Agent::Claude,
        ));
        assert!(failure_report_is_off_turn_admissible(
            &json!({"coding_failure": "codex_dispatch_failed: mcp call timed out"}).to_string(),
            crate::collab::Agent::Claude,
            crate::collab::Agent::Codex,
            Phase::CodeReviewFixGlobalPending,
            crate::collab::Agent::Claude,
        ));
        assert!(!failure_report_is_off_turn_admissible(
            &json!({"coding_failure": "codex_dispatch_failed: mcp call timed out"}).to_string(),
            crate::collab::Agent::Codex,
            crate::collab::Agent::Claude,
            Phase::CodeReviewFixGlobalPending,
            crate::collab::Agent::Claude,
        ));
        for recoverable_only in [
            "git_commit_failed: index.lock EPERM",
            "git_push_failed: rejected non-fast-forward",
            "sandbox_denied: workspace-write refused",
            "disk_full: no space left on device",
            "network_failed: connection reset",
        ] {
            assert!(
                !failure_report_is_off_turn_admissible(
                    &json!({"coding_failure": recoverable_only}).to_string(),
                    crate::collab::Agent::Codex,
                    crate::collab::Agent::Claude,
                    Phase::CodeReviewFixGlobalPending,
                    crate::collab::Agent::Claude,
                ),
                "{recoverable_only} must NOT be off-turn admissible"
            );
        }
    }

    /// The dispatch-failure carve-out is scoped to phases whose Codex turn
    /// Claude dispatches. The pilot's own audit and PR turns are excluded, so
    /// a `pilot=codex` session's Codex-owned review turns cannot be seized by
    /// the dispatcher; branch drift stays admissible from those same phases.
    #[test]
    fn off_turn_dispatch_failure_admission_is_phase_scoped() {
        let dispatch =
            json!({"coding_failure": "codex_dispatch_failed: mcp call timed out"}).to_string();
        for pilot_owned in [Phase::CodeReviewLocalPending, Phase::CodeReviewFinalPending] {
            assert!(
                !failure_report_is_off_turn_admissible(
                    &dispatch,
                    crate::collab::Agent::Claude,
                    crate::collab::Agent::Codex,
                    pilot_owned,
                    crate::collab::Agent::Codex,
                ),
                "{pilot_owned} is a pilot turn and must not be off-turn admissible"
            );
            assert!(
                failure_report_is_off_turn_admissible(
                    &json!({"coding_failure": "branch_drift: head_sha abc not found"}).to_string(),
                    crate::collab::Agent::Claude,
                    crate::collab::Agent::Codex,
                    pilot_owned,
                    crate::collab::Agent::Codex,
                ),
                "branch drift stays observable from {pilot_owned}"
            );
        }

        // The implementation turn only counts when Codex is the implementer —
        // that is the turn Claude dispatched.
        assert!(failure_report_is_off_turn_admissible(
            &dispatch,
            crate::collab::Agent::Claude,
            crate::collab::Agent::Codex,
            Phase::CodeImplementPending,
            crate::collab::Agent::Codex,
        ));
        assert!(!failure_report_is_off_turn_admissible(
            &dispatch,
            crate::collab::Agent::Claude,
            crate::collab::Agent::Codex,
            Phase::CodeImplementPending,
            crate::collab::Agent::Claude,
        ));
    }

    /// The MCP send gate is a second, independently reachable admission
    /// surface (`collab_session.rs` computes `turn_exempt` from this same
    /// predicate), so the checkpoint-drift scope is pinned here too. Unlike
    /// `branch_drift:` — Terminal, and therefore safe to admit anywhere —
    /// `checkpoint_drift:` is recoverable: admitting it parks the session and
    /// hands the reporter the turn, so it is confined to the one phase where
    /// a checkpoint is under construction.
    #[test]
    fn off_turn_checkpoint_drift_admission_is_phase_scoped() {
        let drift = json!({"coding_failure": "checkpoint_drift: HEAD 75a4ea3 is ahead of b9c2ce0"})
            .to_string();

        // In scope: either agent may report the implementer's stale ledger.
        for (reporter, owner, implementer) in [
            (
                crate::collab::Agent::Claude,
                crate::collab::Agent::Codex,
                crate::collab::Agent::Codex,
            ),
            (
                crate::collab::Agent::Codex,
                crate::collab::Agent::Claude,
                crate::collab::Agent::Claude,
            ),
        ] {
            assert!(failure_report_is_off_turn_admissible(
                &drift,
                reporter,
                owner,
                Phase::CodeImplementPending,
                implementer,
            ));
        }

        // Out of scope: every phase past implementation, including the
        // pilot's own audit and PR turns.
        for frozen in [
            Phase::CodeReviewFixGlobalPending,
            Phase::CodeReviewLocalPending,
            Phase::CodeReviewFinalPending,
        ] {
            assert!(
                !failure_report_is_off_turn_admissible(
                    &drift,
                    crate::collab::Agent::Claude,
                    crate::collab::Agent::Codex,
                    frozen,
                    crate::collab::Agent::Codex,
                ),
                "{frozen} freezes the checkpoint and must not be off-turn admissible"
            );
        }

        // A bare prefix is never admissible, even in scope.
        assert!(!failure_report_is_off_turn_admissible(
            &json!({"coding_failure": "checkpoint_drift:"}).to_string(),
            crate::collab::Agent::Claude,
            crate::collab::Agent::Codex,
            Phase::CodeImplementPending,
            crate::collab::Agent::Codex,
        ));
    }

    #[test]
    fn failure_report_compaction_preserves_error_and_classification() {
        let _guard = crate::config::EnvGuard::set("IRONMEM_COMPACT_RESPONSES", "1");
        let verbose = (0..200)
            .map(|index| format!("remote: Counting objects: {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let error = "error: failed to push some refs to 'origin'\nhint: Updates were rejected because the remote contains work\nhint: that you do not have locally.";
        let full_log = format!("git_push_failed:\n{verbose}\n{error}");
        assert!(full_log.chars().count() > MAX_CODING_FAILURE_CHARS);

        let event = parse_failure_report_event(&json!({ "coding_failure": full_log }).to_string())
            .expect("enabled failure-report compaction should fit the field limit");
        let CollabEvent::FailureReport { coding_failure } = event else {
            panic!("expected FailureReport event");
        };

        assert!(coding_failure.chars().count() <= MAX_CODING_FAILURE_CHARS);
        assert!(coding_failure.starts_with("git_push_failed:"));
        assert!(coding_failure.contains("hint: that you do not have locally."));
        assert!(coding_failure.contains("[..."));
        assert_eq!(
            crate::collab::classify(&coding_failure),
            crate::collab::FailureClass::Tooling
        );
    }

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
            "head_sha": SHA,
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
        let raw = task_list_with_plan_file_path(json!("docs/iron/plans/today-feature.md"));
        let event = parse_task_list_event(&raw).expect("valid plan_file_path should parse");
        let CollabEvent::SubmitTaskList { task_list_json, .. } = event else {
            panic!("expected SubmitTaskList event");
        };
        // Canonicalized JSON must round-trip the field so reviewers can find
        // the markdown plan that drove subagent execution.
        assert!(
            task_list_json.contains("docs/iron/plans/today-feature.md"),
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
            "head_sha": SHA,
            "tasks": [{ "id": 1, "title": "t", "acceptance": ["ok"] }],
        })
    }

    #[test]
    fn task_list_rejects_more_than_fifteen_tasks() {
        let mut payload = base_task_list();
        let tasks: Vec<_> = (1..=16)
            .map(|id| {
                json!({
                    "id": id,
                    "title": format!("task-{id}"),
                    "acceptance": ["ok"],
                })
            })
            .collect();
        payload
            .as_object_mut()
            .unwrap()
            .insert("tasks".to_string(), json!(tasks));

        let err = parse_task_list_event(&payload.to_string()).unwrap_err();
        assert!(
            err.to_string()
                .contains("at most 15 tasks; split it into smaller issues"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn task_list_accepts_exactly_fifteen_tasks() {
        let mut payload = base_task_list();
        let tasks: Vec<_> = (1..=15)
            .map(|id| {
                json!({
                    "id": id,
                    "title": format!("task-{id}"),
                    "acceptance": ["ok"],
                })
            })
            .collect();
        payload
            .as_object_mut()
            .unwrap()
            .insert("tasks".to_string(), json!(tasks));

        let event = parse_task_list_event(&payload.to_string())
            .expect("a fifteen-task collab issue should be accepted");
        let CollabEvent::SubmitTaskList { tasks_count, .. } = event else {
            panic!("expected SubmitTaskList event");
        };
        assert_eq!(tasks_count, 15);
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

    /// The two paths that supply a `head_sha` must normalize it identically.
    /// `CollabCheckpoint::from_json` trims (values arrive transcribed out of a
    /// turn template), and `require_checkpoint_proof` then compares the two
    /// with raw string equality — so an untrimmed value here would make a
    /// `collab_checkpoint(head_sha="<sha> ")` and an
    /// `implementation_done{"head_sha":"<sha> "}` naming the same commit
    /// refuse each other as different commits.
    #[test]
    fn a_padded_head_sha_is_trimmed_to_match_the_checkpoint_path() {
        let padded = format!("  {SHA}\t\n");

        let from_event = parse_required_head_sha(
            &json!({ "head_sha": padded }).to_string(),
            "implementation_done",
        )
        .expect("a padded head_sha must be accepted, not rejected");
        let from_checkpoint = crate::collab::CollabCheckpoint::from_json(&json!({
            "session_id": "s1",
            "status": "batch_complete",
            "head_sha": padded,
        }))
        .expect("the checkpoint path already trims")
        .head_sha;

        assert_eq!(from_event, SHA);
        assert_eq!(
            from_event, from_checkpoint,
            "the two head_sha paths must agree byte-for-byte — require_checkpoint_proof \
             compares them with ==",
        );
    }

    /// Trimming must not turn a whitespace-only value into a stored blank: it
    /// is checked *before* the emptiness test, so `" "` is still a rejection.
    #[test]
    fn a_whitespace_only_required_field_is_still_rejected() {
        let err = parse_required_head_sha(
            &json!({ "head_sha": "   " }).to_string(),
            "implementation_done",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("non-empty \"head_sha\""),
            "got: {err}"
        );
    }

    /// Trimming runs before the `is_hex_sha` guard `head_sha` now faces at
    /// this seed site, so a sha padded in transcription is still accepted as
    /// the object name it is. Drop the trim and it would be refused here as
    /// malformed while naming a commit that is on the branch. All three
    /// header fields are trimmed alike.
    #[test]
    fn task_list_header_shas_are_trimmed() {
        let mut payload = base_task_list();
        let object = payload.as_object_mut().unwrap();
        object.insert("plan_hash".to_string(), json!(" h "));
        object.insert("base_sha".to_string(), json!(" b\n"));
        object.insert("head_sha".to_string(), json!(format!("\t{SHA} ")));

        let event = parse_task_list_event(&payload.to_string()).expect("padding must be tolerated");
        let CollabEvent::SubmitTaskList {
            plan_hash,
            base_sha,
            head_sha,
            ..
        } = event
        else {
            panic!("expected SubmitTaskList");
        };
        assert_eq!(plan_hash, "h");
        assert_eq!(base_sha, "b");
        assert_eq!(head_sha, SHA);
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

    /// The `head_sha` a `task_list` reports becomes the session's
    /// `last_head_sha`, so it must be a fixed commit rather than a revision
    /// expression. The turn template shows this field with a `HEAD`
    /// placeholder, so the literal string "HEAD" is the reachable mistake
    /// rather than a theoretical one: stored, it would make every later
    /// ancestry check in the session compare against whatever HEAD happened
    /// to be at that moment.
    #[test]
    fn task_list_rejects_a_head_sha_that_is_a_revision_expression() {
        let mut payload = base_task_list();
        payload
            .as_object_mut()
            .unwrap()
            .insert("head_sha".to_string(), json!("HEAD"));

        let err = parse_task_list_event(&payload.to_string())
            .expect_err("a revision expression must not be storable as last_head_sha");

        let message = err.to_string();
        assert!(
            message.contains("head_sha"),
            "the refusal must name the field the caller has to correct: {message}"
        );
        assert!(
            message.contains("7-64 hex characters"),
            "the refusal must state the shape it wants: {message}"
        );
    }

    /// An abbreviation below the 7-character floor is refused for the same
    /// reason a branch name is: it is not a unique object name.
    #[test]
    fn task_list_rejects_a_head_sha_below_the_seven_character_floor() {
        let mut payload = base_task_list();
        payload
            .as_object_mut()
            .unwrap()
            .insert("head_sha".to_string(), json!("abc123"));

        let err = parse_task_list_event(&payload.to_string())
            .expect_err("a 6-character abbreviation is not an object name");

        // Pinned to the same contract its sibling asserts, so this cannot
        // pass on an unrelated rejection — a later field added to
        // `base_task_list`, or some other guard that happens to dislike
        // `"abc123"` once this one is gone.
        let message = err.to_string();
        assert!(
            message.contains("7-64 hex characters"),
            "the refusal must be the shape check, stating the shape it wants: {message}"
        );
    }

    /// The refusal echoes the offending `head_sha` back, and it is the one
    /// path on which that value has *not* passed `is_hex_sha` — so it is the
    /// one place an unbounded caller string would reach an MCP response body
    /// verbatim. [`MAX_ECHOED_HEAD_SHA_CHARS`] is what stops it.
    ///
    /// The cut is `chars().take()` rather than a byte slice because the bound
    /// lands in the middle of bytes the caller chose: slicing a multibyte
    /// value at byte 80 would panic inside the error path. That case is the
    /// reason the implementation looks the way it does, so it is the one most
    /// worth pinning — and none of it is exercised by the ordinary refusals,
    /// whose values (`"HEAD"`, `"abc123"`) sit far below the bound.
    #[test]
    fn task_list_bounds_the_head_sha_it_echoes_back_in_a_refusal() {
        let refuse = |head_sha: &str| -> String {
            let mut payload = base_task_list();
            payload
                .as_object_mut()
                .unwrap()
                .insert("head_sha".to_string(), json!(head_sha));
            parse_task_list_event(&payload.to_string())
                .expect_err("a head_sha this long is not an object name")
                .to_string()
        };

        // Past the bound the message stops growing: its length is the
        // constant's to decide, not the caller's. Two inputs an order of
        // magnitude apart must produce byte-identical messages.
        let long = refuse(&"a".repeat(500));
        let longer = refuse(&"a".repeat(5000));
        assert_eq!(
            long, longer,
            "the echo must be capped, not passed through — a 10x longer \
             head_sha produced a different message"
        );
        assert!(
            !long.contains(&"a".repeat(MAX_ECHOED_HEAD_SHA_CHARS + 1)),
            "at most {MAX_ECHOED_HEAD_SHA_CHARS} characters may survive: {long}"
        );
        assert!(
            long.contains('…'),
            "a value that was cut must say so: {long}"
        );

        // Exactly at the bound is not a cut. Such a value is still refused —
        // `is_hex_sha` tops out at 64, well below 80 — so it reaches this
        // same message with nothing removed, which is the branch the ordinary
        // refusals never reach and the one most able to rot unnoticed.
        let at_bound = refuse(&"a".repeat(MAX_ECHOED_HEAD_SHA_CHARS));
        assert!(
            at_bound.contains(&"a".repeat(MAX_ECHOED_HEAD_SHA_CHARS)),
            "a value at the bound must be echoed whole: {at_bound}"
        );
        assert!(
            !at_bound.contains('…'),
            "a value that was not cut must not be marked as cut: {at_bound}"
        );

        // Multibyte, spelled as an escape so a decomposed `e` + combining
        // accent in this source file cannot quietly make it two chars. The
        // cut falls mid-character by byte count; `chars().take()` is why this
        // yields a valid String instead of panicking.
        let multibyte = refuse(&'\u{00e9}'.to_string().repeat(500));
        assert!(
            multibyte.contains('…'),
            "a cut multibyte value must say so: {multibyte}"
        );
        assert_eq!(
            multibyte.matches('\u{00e9}').count(),
            MAX_ECHOED_HEAD_SHA_CHARS,
            "the cap counts characters, not bytes: {multibyte}"
        );
    }
}

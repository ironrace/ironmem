use std::fmt;
use std::path::{Component, Path};

use serde_json::Value;

use super::MAX_TASKS_PER_COLLAB_ISSUE;

// Allowed values for `execution_mode` on a `task_list` payload. Absence means
// default (subagent-driven). The string `"subagent_driven"` is intentionally
// NOT in this set — callers omit the field for the default path.
const ALLOWED_EXECUTION_MODES: &[&str] = &["mechanical_direct"];

/// Validation failure for the JSON body stored with a collab task list.
///
/// This stays crate-private because MCP callers receive `MemoryError`, while
/// direct state-machine callers receive `CollabError` after phase-specific
/// mapping of empty and oversized lists.
#[derive(Debug)]
pub(crate) enum TaskListValidationError {
    EmptyTasks,
    TooManyTasks { actual: u32 },
    Invalid(String),
}

impl fmt::Display for TaskListValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTasks => f.write_str("task_list must contain at least one task"),
            Self::TooManyTasks { actual } => write!(
                f,
                "task_list contains {actual} tasks; a collab issue may contain at most {MAX_TASKS_PER_COLLAB_ISSUE} tasks; split it into smaller issues"
            ),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

/// Validate the task-specific JSON that is persisted with `SubmitTaskList`.
///
/// Both the MCP parser and the public state-machine API call this function so
/// direct Rust callers cannot persist a task list that MCP callers would have
/// rejected. Header fields (`plan_hash`, `base_sha`, and `head_sha`) remain
/// event fields owned by the state machine.
pub(crate) fn validate_task_list_body(payload: &Value) -> Result<u32, TaskListValidationError> {
    if let Some(raw) = payload.get("plan_file_path") {
        let path = raw.as_str().ok_or_else(|| {
            TaskListValidationError::Invalid(
                "task_list plan_file_path must be a string".to_string(),
            )
        })?;
        validate_plan_file_path(path)?;
    }
    if let Some(raw) = payload.get("execution_mode") {
        let mode = raw.as_str().ok_or_else(|| {
            TaskListValidationError::Invalid(
                "task_list execution_mode must be a string".to_string(),
            )
        })?;
        if !ALLOWED_EXECUTION_MODES.contains(&mode) {
            return Err(TaskListValidationError::Invalid(format!(
                "task_list execution_mode \"{}\" is not allowed; allowed values: [{}]",
                mode,
                ALLOWED_EXECUTION_MODES
                    .iter()
                    .map(|value| format!("\"{value}\""))
                    .collect::<Vec<_>>()
                    .join(", "),
            )));
        }
    }

    let task_count = task_count_from_payload(payload)?;
    if task_count > MAX_TASKS_PER_COLLAB_ISSUE {
        return Err(TaskListValidationError::TooManyTasks { actual: task_count });
    }

    let tasks = payload
        .get("tasks")
        .and_then(Value::as_array)
        .expect("task_count_from_payload already verified the tasks array");

    for (idx, task) in tasks.iter().enumerate() {
        let task_id = task.get("id").and_then(Value::as_i64).ok_or_else(|| {
            TaskListValidationError::Invalid(format!(
                "task_list task[{idx}] missing integer \"id\""
            ))
        })?;
        // Ids must be exactly `1..=tasks.len()`, in order — not merely
        // strictly increasing, which is what this checked before issue #273
        // Task 7.
        //
        // Strict ordering alone is too weak for the `implementation_done`
        // checkpoint gate built on top of it. That gate asks whether the
        // checkpoint's ledger covers the batch, and it answers by comparing
        // `completed_task_ids` against `1..=total` where `total` is
        // `tasks.len()` (`CollabSession::tasks_count` →
        // `CollabCheckpoint::covers_all_tasks`). Under strict-ordering-only
        // those two disagree, in both directions and both wrong:
        //
        // - Ids `4,5,6` give `total = 3`, so the gate demands ids 1, 2 and 3 —
        //   which that task list does not contain and no honest checkpoint can
        //   ever report. The batch becomes permanently unable to finish.
        // - Ids `1,5,9` also give `total = 3`, so a checkpoint listing `1,2,3`
        //   satisfies the gate while tasks 5 and 9 were never done — the exact
        //   false progress report issue #273 exists to prevent.
        //
        // Closing it here rather than at the gate keeps a single source of
        // truth for "which tasks exist": with this check, `tasks.len()` *is*
        // the id set, so every downstream consumer that treats a count as a
        // range is correct by construction rather than by coincidence.
        //
        // This tightens a previously-accepted shape. It applies at send time
        // only, so no stored task list is re-validated; a pre-existing session
        // whose ids are not 1-based keeps whatever behavior it already had.
        let expected_id = idx as i64 + 1;
        if task_id != expected_id {
            return Err(TaskListValidationError::Invalid(format!(
                "task_list task ids must be exactly 1..={} in order (task[{idx}].id={task_id}, expected {expected_id}); \
                 the implementation_done checkpoint gate treats the task count as the id set, so a gap or a non-1-based \
                 id makes the batch either unfinishable or falsely reportable as complete",
                tasks.len(),
            )));
        }
        let acceptance = task
            .get("acceptance")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                TaskListValidationError::Invalid(format!(
                    "task_list task[{idx}] missing \"acceptance\" array"
                ))
            })?;
        if acceptance.is_empty() {
            return Err(TaskListValidationError::Invalid(format!(
                "task_list task[{idx}] must include at least one acceptance criterion"
            )));
        }
    }

    Ok(task_count)
}

/// Count a task-list JSON body after checking only its canonical array shape.
///
/// The state machine uses this before comparing the untrusted declared count,
/// then calls [`validate_task_list_body`] for the complete shared validation.
pub(crate) fn task_count_from_payload(payload: &Value) -> Result<u32, TaskListValidationError> {
    let tasks = payload
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            TaskListValidationError::Invalid("task_list missing \"tasks\" array".to_string())
        })?;
    if tasks.is_empty() {
        return Err(TaskListValidationError::EmptyTasks);
    }
    u32::try_from(tasks.len()).map_err(|_| {
        TaskListValidationError::Invalid("task_list contains too many tasks".to_string())
    })
}

fn validate_plan_file_path(path: &str) -> Result<(), TaskListValidationError> {
    const MAX_LEN: usize = 512;

    if path.is_empty() {
        return Err(TaskListValidationError::Invalid(
            "task_list plan_file_path must be non-empty when present".to_string(),
        ));
    }
    if path.chars().count() > MAX_LEN {
        return Err(TaskListValidationError::Invalid(format!(
            "task_list plan_file_path exceeds {MAX_LEN} chars"
        )));
    }
    if path
        .bytes()
        .any(|byte| byte == 0 || (byte < 0x20 && byte != b'\t'))
    {
        return Err(TaskListValidationError::Invalid(
            "task_list plan_file_path must not contain control bytes (incl. NUL)".to_string(),
        ));
    }
    if path.contains('%') {
        return Err(TaskListValidationError::Invalid(
            "task_list plan_file_path must not contain percent-encoded sequences (no '%' allowed)"
                .to_string(),
        ));
    }
    for component in Path::new(path).components() {
        match component {
            Component::Normal(_) => {}
            Component::ParentDir => {
                return Err(TaskListValidationError::Invalid(
                    "task_list plan_file_path must not contain '..' segments".to_string(),
                ));
            }
            Component::RootDir => {
                return Err(TaskListValidationError::Invalid(
                    "task_list plan_file_path must be repo-relative (no leading '/')".to_string(),
                ));
            }
            Component::Prefix(_) => {
                return Err(TaskListValidationError::Invalid(
                    "task_list plan_file_path must not contain a path prefix (e.g. drive letter or UNC root)"
                        .to_string(),
                ));
            }
            Component::CurDir => {
                return Err(TaskListValidationError::Invalid(
                    "task_list plan_file_path must not contain '.' segments".to_string(),
                ));
            }
        }
    }
    Ok(())
}

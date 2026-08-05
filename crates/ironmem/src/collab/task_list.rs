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

    let mut last_id: Option<i64> = None;
    for (idx, task) in tasks.iter().enumerate() {
        let task_id = task.get("id").and_then(Value::as_i64).ok_or_else(|| {
            TaskListValidationError::Invalid(format!(
                "task_list task[{idx}] missing integer \"id\""
            ))
        })?;
        if let Some(previous) = last_id {
            if task_id <= previous {
                return Err(TaskListValidationError::Invalid(format!(
                    "task_list tasks must be strictly ordered by id (task[{idx}].id={task_id} follows {previous})"
                )));
            }
        }
        last_id = Some(task_id);
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

use std::io::Read;
use std::path::PathBuf;

use serde::Serialize;

use crate::bootstrap::{ensure_bootstrapped, record_workspace_mine, resolve_workspace_root};
use crate::config::Config;
use crate::diary;
use crate::error::MemoryError;
use crate::ingest::mine_directory;
use crate::mcp::app::App;
use crate::sanitize::sanitize_session_id;

#[derive(Debug, Serialize)]
pub struct HookResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub hook: String,
    pub harness: String,
    pub workspace_root: Option<String>,
}

pub fn run_hook(
    hook_name: &str,
    harness: &str,
    config: Config,
) -> Result<HookResponse, MemoryError> {
    let input = read_hook_input()?;
    let workspace_root = parse_workspace_root(&input);
    let session_id = parse_session_id(&input);
    let app = App::new(config)?;
    let response = HookResponse {
        decision: None,
        reason: None,
        hook: hook_name.to_string(),
        harness: harness.to_string(),
        workspace_root: workspace_root
            .as_ref()
            .map(|path| path.display().to_string()),
    };

    match hook_name {
        "session-start" => {
            ensure_bootstrapped(&app, workspace_root.as_deref())?;
        }
        "precompact" | "stop" => {
            ensure_bootstrapped(&app, workspace_root.as_deref())?;
            if let Some(root) = workspace_root.as_deref() {
                mine_directory(&app, root.to_string_lossy().as_ref())?;
                record_workspace_mine(&app.config, root)?;
            }

            if let Some(summary) =
                session_summary(&input, hook_name, harness, session_id.as_deref())
            {
                persist_diary_summary(&app, &summary)?;
            }
        }
        other => {
            return Err(MemoryError::NotFound(format!(
                "Hook '{other}' (harness: {harness}) is not supported"
            )))
        }
    }

    Ok(response)
}

fn persist_diary_summary(app: &App, content: &str) -> Result<(), MemoryError> {
    let _ = diary::write_entry(app, content, "diary", "hook", 8_000)?;
    Ok(())
}

fn read_hook_input() -> Result<serde_json::Value, MemoryError> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    if raw.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    match serde_json::from_str(&raw) {
        Ok(value) => Ok(value),
        Err(_) => Ok(serde_json::json!({})),
    }
}

fn parse_workspace_root(input: &serde_json::Value) -> Option<PathBuf> {
    let explicit = input
        .get("cwd")
        .and_then(|value| value.as_str())
        .or_else(|| input.get("workspace_root").and_then(|value| value.as_str()))
        .map(PathBuf::from);
    resolve_workspace_root(explicit.as_deref())
}

fn parse_session_id(input: &serde_json::Value) -> Option<String> {
    input
        .get("session_id")
        .and_then(|value| value.as_str())
        .map(sanitize_session_id)
}

fn session_summary(
    input: &serde_json::Value,
    hook_name: &str,
    harness: &str,
    session_id: Option<&str>,
) -> Option<String> {
    let transcript_path = input
        .get("transcript_path")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let cwd = input
        .get("cwd")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if transcript_path.is_empty() && cwd.is_empty() && session_id.is_none() {
        return None;
    }

    Some(format!(
        "Hook {hook_name} ran for harness {harness}. session_id={} cwd={} transcript_path={}",
        session_id.unwrap_or("unknown"),
        sanitize_path_for_log(cwd),
        sanitize_path_for_log(transcript_path),
    ))
}

/// Sanitize a file-system path for inclusion in diary/log entries.
///
/// Allows printable ASCII path characters only. Strips anything that could be
/// used for injection (backticks, quotes, semicolons, pipe, ampersand) and
/// caps length at 512 characters so a crafted hook payload cannot inflate
/// diary entries.
fn sanitize_path_for_log(raw: &str) -> String {
    raw.chars()
        .filter(|c| {
            c.is_ascii_graphic()
                && !matches!(
                    c,
                    '"' | '\'' | '`' | ';' | '|' | '&' | '<' | '>' | '$' | '!'
                )
        })
        .take(512)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::protocol::JsonRpcRequest;
    use crate::mcp::server::dispatch;

    #[test]
    fn parses_workspace_root_from_payload() {
        let payload = serde_json::json!({
            "cwd": "/tmp/workspace",
            "session_id": "../bad"
        });
        let path = parse_workspace_root(&payload).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/workspace"));
        assert_eq!(parse_session_id(&payload).unwrap(), "bad");
    }

    #[test]
    fn builds_session_summary_when_context_exists() {
        let payload = serde_json::json!({
            "cwd": "/tmp/workspace",
            "transcript_path": "/tmp/transcript.jsonl"
        });
        let summary = session_summary(&payload, "stop", "codex", Some("abc")).unwrap();
        assert!(summary.contains("Hook stop ran"));
        assert!(summary.contains("/tmp/workspace"));
    }

    #[test]
    fn persisted_session_summary_is_readable_via_diary_api() {
        let app = App::open_for_test().unwrap();
        persist_diary_summary(&app, "Hook stop ran for test session").unwrap();

        let req: JsonRpcRequest = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "ironmem_diary_read",
                "arguments": { "wing": "diary", "limit": 10 }
            }
        }))
        .unwrap();
        let resp = dispatch(&app, &req).unwrap();
        let result = resp.result.unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let read: serde_json::Value = serde_json::from_str(text).unwrap();
        let entries = read["entries"].as_array().unwrap();
        assert!(
            entries
                .iter()
                .any(|entry| entry["content"] == "Hook stop ran for test session"),
            "hook summaries must be readable through the diary API"
        );
    }
}

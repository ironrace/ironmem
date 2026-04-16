//! Session lifecycle hooks for Codex and Claude Code integrations.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::bootstrap::{ensure_bootstrapped, record_workspace_mine, resolve_workspace_root};
use crate::config::Config;
use crate::db::drawers::generate_id;
use crate::diary;
use crate::error::MemoryError;
use crate::ingest::mine_directory;
use crate::mcp::app::App;
use crate::sanitize::{sanitize_harness, sanitize_session_id};

const REVIEW_WING: &str = "reviews";
const REVIEW_MAX_BYTES: usize = 24_000;

static REVIEW_FILE_REF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9_./-]+\.[A-Za-z0-9]+:\d+").unwrap());
static REVIEW_PR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:pr|pull request)\s*#?\s*(\d+)\b").unwrap());

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredReview {
    id: String,
    room: String,
}

pub fn run_hook(
    hook_name: &str,
    harness: &str,
    config: Config,
) -> Result<HookResponse, MemoryError> {
    let input = read_hook_input()?;
    run_hook_with_input(hook_name, harness, config, input)
}

fn run_hook_with_input(
    hook_name: &str,
    harness: &str,
    config: Config,
    input: serde_json::Value,
) -> Result<HookResponse, MemoryError> {
    let workspace_root = parse_workspace_root(&input);
    let transcript_path = parse_transcript_path(&input);
    let session_id = parse_session_id(&input);
    let app = App::new(config)?;
    let allows_writes = app.config.mcp_access_mode.allows_writes();
    let bootstrap_workspace = if allows_writes {
        workspace_root.as_deref()
    } else {
        None
    };
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
            ensure_bootstrapped(&app, bootstrap_workspace)?;
        }
        "precompact" | "stop" => {
            ensure_bootstrapped(&app, bootstrap_workspace)?;
            if allows_writes {
                if let Some(root) = workspace_root.as_deref() {
                    mine_directory(&app, root.to_string_lossy().as_ref())?;
                    record_workspace_mine(&app.config, root)?;
                }

                let stored_review = persist_transcript_review(
                    &app,
                    workspace_root.as_deref(),
                    transcript_path.as_deref(),
                    session_id.as_deref(),
                );
                if let Some(summary) = session_summary(
                    &input,
                    hook_name,
                    harness,
                    session_id.as_deref(),
                    stored_review,
                ) {
                    persist_diary_summary(&app, &summary)?;
                }
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

fn parse_transcript_path(input: &serde_json::Value) -> Option<PathBuf> {
    input
        .get("transcript_path")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
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
    stored_review: Option<StoredReview>,
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

    let mut summary = format!(
        "Hook {} ran for harness {}. session_id={} cwd={} transcript_path={}",
        sanitize_harness(hook_name),
        sanitize_harness(harness),
        session_id.unwrap_or("unknown"),
        sanitize_path_for_log(cwd),
        sanitize_path_for_log(transcript_path),
    );
    if let Some(review) = stored_review {
        summary.push_str(&format!(" stored_review={REVIEW_WING}/{}", review.room));
    }
    Some(summary)
}

fn persist_transcript_review(
    app: &App,
    workspace_root: Option<&Path>,
    transcript_path: Option<&Path>,
    session_id: Option<&str>,
) -> Option<StoredReview> {
    let path = transcript_path?;
    match persist_transcript_review_from_path(app, workspace_root, path, session_id) {
        Ok(review) => review,
        Err(error) => {
            tracing::warn!(
                "Skipping transcript-derived review capture for {}: {error}",
                path.display()
            );
            None
        }
    }
}

fn persist_transcript_review_from_path(
    app: &App,
    workspace_root: Option<&Path>,
    transcript_path: &Path,
    session_id: Option<&str>,
) -> Result<Option<StoredReview>, MemoryError> {
    let Some(review_text) = extract_review_from_transcript(transcript_path)? else {
        return Ok(None);
    };
    let room = derive_review_room(&review_text, workspace_root);
    let content = truncate_text_to_byte_limit(&review_text, REVIEW_MAX_BYTES);
    let content = crate::sanitize::sanitize_content(&content, REVIEW_MAX_BYTES)?;
    let dedupe_key = format!(
        "{}:{}:{}",
        session_id.unwrap_or("unknown"),
        transcript_path.display(),
        content
    );
    let id = generate_id(&dedupe_key, REVIEW_WING, &room);
    let embedding = {
        let mut embedder = app
            .embedder
            .write()
            .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;
        embedder.embed_one(content).map_err(MemoryError::Embed)?
    };
    let source_file = transcript_path.to_string_lossy();
    app.db.insert_drawer(
        &id,
        content,
        &embedding,
        REVIEW_WING,
        &room,
        source_file.as_ref(),
        "hook",
    )?;
    app.mark_dirty();
    Ok(Some(StoredReview { id, room }))
}

fn extract_review_from_transcript(path: &Path) -> Result<Option<String>, MemoryError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(find_review_in_transcript(&raw))
}

fn find_review_in_transcript(raw: &str) -> Option<String> {
    for line in raw.lines().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let mut candidates = Vec::new();
        collect_assistant_texts(&value, &mut candidates);
        for candidate in candidates.into_iter().rev() {
            let normalized = normalize_candidate_text(&candidate);
            if is_review_like(&normalized) {
                return Some(normalized);
            }
        }
    }
    None
}

fn collect_assistant_texts(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if is_assistant_message(map) {
                let text = extract_message_text(map);
                if !text.is_empty() {
                    out.push(text);
                }
            } else {
                for nested in map.values() {
                    collect_assistant_texts(nested, out);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_assistant_texts(item, out);
            }
        }
        _ => {}
    }
}

fn is_assistant_message(map: &serde_json::Map<String, serde_json::Value>) -> bool {
    ["role", "speaker", "author", "sender"].iter().any(|key| {
        map.get(*key)
            .and_then(|value| value.as_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("assistant"))
    }) || map
        .get("type")
        .and_then(|value| value.as_str())
        .is_some_and(|value| {
            value.eq_ignore_ascii_case("assistant")
                || value.eq_ignore_ascii_case("assistant_message")
        })
}

fn extract_message_text(map: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut parts = Vec::new();
    for key in ["content", "message", "text", "parts"] {
        if let Some(value) = map.get(key) {
            collect_text_fragments(value, &mut parts);
        }
    }
    parts.join("\n").trim().to_string()
}

fn collect_text_fragments(value: &serde_json::Value, parts: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) if !text.trim().is_empty() => {
            parts.push(text.trim().to_string());
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_text_fragments(item, parts);
            }
        }
        serde_json::Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(|value| value.as_str()) {
                if !text.trim().is_empty() {
                    parts.push(text.trim().to_string());
                }
            }
            for key in ["content", "message", "parts"] {
                if let Some(nested) = map.get(key) {
                    collect_text_fragments(nested, parts);
                }
            }
        }
        _ => {}
    }
}

fn normalize_candidate_text(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn is_review_like(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }

    let lower = text.to_ascii_lowercase();

    if lower.starts_with("findings") || lower.starts_with("no findings") {
        return true;
    }

    let review_line_markers = [
        "### high",
        "### medium",
        "### low",
        "- high:",
        "- medium:",
        "- low:",
        "**high**",
        "**medium**",
        "**low**",
    ];
    if review_line_markers.iter().any(|m| lower.contains(m)) {
        return true;
    }

    if ["request changes", "would not merge", "approve", "lgtm"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return true;
    }

    REVIEW_FILE_REF_RE.is_match(text) && text.len() >= 80
}

fn derive_review_room(review_text: &str, workspace_root: Option<&Path>) -> String {
    if let Some(captures) = REVIEW_PR_RE.captures(review_text) {
        return format!("pr-{}", &captures[1]);
    }

    workspace_root
        .and_then(|root| root.file_name())
        .and_then(|value| value.to_str())
        .and_then(|value| crate::sanitize::sanitize_name(value, "room").ok())
        .unwrap_or_else(|| "general".to_string())
}

fn truncate_text_to_byte_limit(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }

    let mut end = 0;
    for (idx, ch) in text.char_indices() {
        let next = idx + ch.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }
    text[..end].to_string()
}

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
    use crate::config::{Config, EmbedMode, McpAccessMode};
    use crate::mcp::protocol::JsonRpcRequest;
    use crate::mcp::server::dispatch;
    use std::sync::{LazyLock, Mutex};

    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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
        let summary = session_summary(&payload, "stop", "codex", Some("abc"), None).unwrap();
        assert!(summary.contains("Hook stop ran"));
        assert!(summary.contains("/tmp/workspace"));
    }

    #[test]
    fn session_summary_mentions_stored_review_room() {
        let payload = serde_json::json!({
            "cwd": "/tmp/workspace",
            "transcript_path": "/tmp/transcript.jsonl"
        });
        let summary = session_summary(
            &payload,
            "stop",
            "codex",
            Some("abc"),
            Some(StoredReview {
                id: "review-1".to_string(),
                room: "pr-2".to_string(),
            }),
        )
        .unwrap();
        assert!(summary.contains("stored_review=reviews/pr-2"));
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

    #[test]
    fn extracts_review_from_transcript_jsonl() {
        let temp = tempfile::tempdir().unwrap();
        let transcript = temp.path().join("transcript.jsonl");
        std::fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                serde_json::json!({
                    "role": "user",
                    "content": "please review this PR"
                }),
                serde_json::json!({
                    "message": {
                        "role": "assistant",
                        "content": [
                            {
                                "type": "text",
                                "text": "Findings\n- High: duplicate writes still happen in crates/ironrace-memory/src/hook.rs:52\n- Medium: add a regression test\nPR #2"
                            }
                        ]
                    }
                })
            ),
        )
        .unwrap();

        let extracted = extract_review_from_transcript(&transcript)
            .unwrap()
            .unwrap();
        assert!(extracted.starts_with("Findings"));
        assert!(extracted.contains("PR #2"));
    }

    #[test]
    fn transcript_review_storage_is_deduplicated() {
        let app = App::open_for_test().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ironrace-memory");
        std::fs::create_dir_all(&workspace).unwrap();
        let transcript = temp.path().join("transcript.jsonl");
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                serde_json::json!({
                    "role": "assistant",
                    "content": "Findings\n- High: race condition in crates/ironrace-memory/src/ingest/mod.rs:374\n- Medium: keep a regression test\nPR #1"
                })
            ),
        )
        .unwrap();

        let first = persist_transcript_review_from_path(
            &app,
            Some(&workspace),
            &transcript,
            Some("session-1"),
        )
        .unwrap()
        .unwrap();
        let second = persist_transcript_review_from_path(
            &app,
            Some(&workspace),
            &transcript,
            Some("session-1"),
        )
        .unwrap()
        .unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(first.room, "pr-1");

        let stored = app
            .db
            .get_drawers(Some("reviews"), Some("pr-1"), 10)
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert!(stored[0].content.contains("Findings"));
        assert!(stored[0].source_file.ends_with("transcript.jsonl"));
    }

    #[test]
    fn read_only_stop_hook_skips_mining_and_diary_writes() {
        let _env = ENV_MUTEX.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("README.md"), "# Workspace\n\nMine me.").unwrap();

        std::env::set_var("IRONMEM_DISABLE_MIGRATION", "1");

        let config = Config {
            db_path: temp.path().join("memory.sqlite3"),
            model_dir: temp.path().join("model"),
            model_dir_explicit: true,
            state_dir: temp.path().join("hook_state"),
            mcp_access_mode: McpAccessMode::ReadOnly,
            embed_mode: EmbedMode::Noop,
        };

        let response = run_hook_with_input(
            "stop",
            "codex",
            config.clone(),
            serde_json::json!({
                "cwd": workspace,
                "session_id": "session-1",
                "transcript_path": "/tmp/transcript.jsonl"
            }),
        )
        .unwrap();

        let app = App::new(config).unwrap();
        assert_eq!(response.hook, "stop");
        assert_eq!(app.db.count_drawers(None).unwrap(), 0);

        std::env::remove_var("IRONMEM_DISABLE_MIGRATION");
    }

    #[test]
    fn is_review_like_accepts_clear_review_signals() {
        assert!(is_review_like(
            "Findings\n- High: foo.rs:12\n- Medium: bar.rs:34"
        ));
        assert!(is_review_like("No findings. All looks good."));
        assert!(is_review_like(
            "Some changes needed.\n### High\nSomething is wrong\n### Medium\nStyle nit"
        ));
        assert!(is_review_like(
            "- High: src/foo.rs:12 — missing error handling"
        ));
        assert!(is_review_like("request changes: the auth check is missing"));
        assert!(is_review_like("LGTM"));
        assert!(is_review_like("Approve — looks good to me"));
    }

    #[test]
    fn is_review_like_rejects_non_review_messages() {
        assert!(!is_review_like("This uses a blocking I/O call"));
        assert!(!is_review_like("high: performance is the goal here"));
        assert!(!is_review_like("The latency is high: 200ms average"));
        assert!(!is_review_like("Let me explain the architecture"));
        assert!(!is_review_like("Here is the updated implementation"));
        assert!(!is_review_like("see foo.rs:12"));
    }

    #[test]
    fn collect_assistant_texts_does_not_double_count_nested_content() {
        let value = serde_json::json!({
            "role": "assistant",
            "content": [
                {
                    "type": "text",
                    "text": "### High\n- foo.rs:12 missing check"
                }
            ]
        });
        let mut candidates = Vec::new();
        collect_assistant_texts(&value, &mut candidates);
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn truncate_text_to_byte_limit_respects_char_boundaries() {
        let s = "a".repeat(23_999) + "é";
        assert_eq!(s.len(), 24_001);
        let truncated = truncate_text_to_byte_limit(&s, 24_000);
        assert_eq!(truncated.len(), 23_999);
        assert!(truncated.is_char_boundary(truncated.len()));
    }
}

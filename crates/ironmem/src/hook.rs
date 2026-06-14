//! Session lifecycle hooks for Codex and Claude Code integrations.

use std::io::{Read, Seek, SeekFrom};
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
const METRICS_TRANSCRIPT_TAIL_BYTES: u64 = 2 * 1024 * 1024;

/// ~400-token budget for the injected block (~4 chars/token).
const SESSION_CONTEXT_MAX_BYTES: usize = 1600;
const SESSION_CONTEXT_DIARY_EXCERPT_BYTES: usize = 200;
const SESSION_CONTEXT_TOP_N: usize = 5;
const SESSION_CONTEXT_SHORT_ID: usize = 8;

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
    #[serde(rename = "hookSpecificOutput", skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

/// Claude Code's `SessionStart` additional-context channel. Serialized only
/// when populated (non-Codex harness); camelCase keys match the Claude Code
/// hook output contract.
#[derive(Debug, Serialize)]
pub struct HookSpecificOutput {
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,
    #[serde(rename = "additionalContext")]
    pub additional_context: String,
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
    if crate::search::tunables::metrics_enabled() && allows_writes {
        sample_occupancy(
            &app,
            hook_name,
            harness,
            session_id.as_deref(),
            workspace_root.as_deref(),
            transcript_path.as_deref(),
        );
    }
    let bootstrap_workspace = if allows_writes {
        workspace_root.as_deref()
    } else {
        None
    };
    let mut response = HookResponse {
        decision: None,
        reason: None,
        hook: hook_name.to_string(),
        harness: harness.to_string(),
        workspace_root: workspace_root
            .as_ref()
            .map(|path| path.display().to_string()),
        hook_specific_output: None,
    };

    match hook_name {
        "session-start" => {
            ensure_bootstrapped(&app, bootstrap_workspace)?;
            // Claude Code-specific: push a compact memory status block via
            // hookSpecificOutput.additionalContext. Codex has no such channel,
            // so it silently degrades (field stays None → omitted from JSON).
            if !harness.starts_with("codex") {
                if let Some(ctx) = build_session_start_context(&app, workspace_root.as_deref()) {
                    response.hook_specific_output = Some(HookSpecificOutput {
                        hook_event_name: "SessionStart".to_string(),
                        additional_context: ctx,
                    });
                }
            }
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

fn sample_occupancy(
    app: &App,
    hook_name: &str,
    harness: &str,
    session_id: Option<&str>,
    workspace_root: Option<&Path>,
    transcript_path: Option<&Path>,
) {
    let Some(event) = crate::metrics::hook_event_for(hook_name) else {
        return; // unsupported hook → no sample
    };
    let Some(session_id) = session_id else {
        return; // D4: absent session id → skip (never create an empty key)
    };
    // `parse_session_id` sanitizes to "unknown" for path-traversal-ish input;
    // skip it so the hook never keys a summary the MCP side (which maps
    // sanitized "unknown" → None) would never co-key.
    if session_id == "unknown" {
        return;
    }
    // CHECK constraint: occupancy_samples.harness ∈ {claude, codex}.
    let harness_norm = if harness.starts_with("codex") {
        "codex"
    } else {
        "claude"
    };
    let usage = transcript_path
        .and_then(read_transcript_tail)
        .and_then(|raw| crate::metrics::extract_last_assistant_usage(&raw));
    let workspace = workspace_root.map(|p| p.to_string_lossy().to_string());
    crate::metrics::record_occupancy_sample(
        &app.db,
        harness_norm,
        session_id,
        workspace.as_deref(),
        event,
        usage,
        crate::search::tunables::context_window(),
    );
}

fn read_transcript_tail(path: &Path) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    let start = metadata.len().saturating_sub(METRICS_TRANSCRIPT_TAIL_BYTES);
    // `O_NOFOLLOW` rejects a symlink swapped in after the `symlink_metadata`
    // check, closing the TOCTOU window atomically at open() time.
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .ok()?
    };
    if start > 0 {
        file.seek(SeekFrom::Start(start)).ok()?;
    }
    // Decode lossily: an arbitrary byte-offset seek can split a multibyte
    // codepoint, which would make a strict UTF-8 read fail and silently drop a
    // real transcript's usage. Lossy decode never fails; the partial first line
    // is discarded below anyway.
    let mut buf = Vec::new();
    let mut reader = file.take(METRICS_TRANSCRIPT_TAIL_BYTES);
    reader.read_to_end(&mut buf).ok()?;
    let mut raw = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        let first_newline = raw.find('\n')?;
        raw = raw[first_newline + 1..].to_string();
    }
    Some(raw)
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

/// Normalize free-text (diary/drawer content) for safe inclusion in the
/// session-start context block: trim, collapse any whitespace/control run to a
/// single space, then byte-cap on a char boundary. serde handles JSON escaping
/// when the enclosing `HookResponse` is serialized.
fn compact_excerpt(text: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_bytes + 4));
    let mut prev_space = false;
    for ch in text.trim().chars() {
        if ch.is_whitespace() || ch.is_control() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    truncate_text_to_byte_limit(out.trim_end(), max_bytes)
}

/// Sort `(name, count)` pairs by count descending (ascending name tie-break for
/// determinism), take the top `n`, and format each as `name:count`. The DB
/// count helpers (`wing_counts`/`room_counts`) return rows alphabetically, so
/// this re-sort is what surfaces the largest buckets.
fn top_counts(mut pairs: Vec<(String, usize)>, n: usize) -> Vec<String> {
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    pairs
        .into_iter()
        .take(n)
        .map(|(name, count)| format!("{name}:{count}"))
        .collect()
}

/// Build the compact session-start memory block (Claude Code only). Every read
/// is best-effort: on a DB error we `warn!` and drop that line rather than fail
/// the hook. `MEMORY_PROTOCOL` is always included with a reserved byte budget so
/// a pile of long wing/room names can never truncate the one behavior-changing
/// line; the status lines share whatever budget is left and may be dropped or
/// truncated. The active collab session and diary pointer come from the DB
/// because this hook runs in a separate process where
/// `App::active_collab_session_snapshot()` is empty.
fn build_session_start_context(app: &App, workspace_root: Option<&Path>) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();

    // 1. Drawer total + top wings (largest first; the DB returns them alphabetically).
    match app.db.count_drawers(None) {
        Ok(total) => {
            let wings = match app.db.wing_counts() {
                Ok(wings) => wings,
                Err(e) => {
                    tracing::warn!("session-start context: wing counts unavailable: {e}");
                    Vec::new()
                }
            };
            let n_wings = wings.len();
            let top = top_counts(wings, SESSION_CONTEXT_TOP_N);
            if top.is_empty() {
                lines.push(format!("[ironmem] {total} drawers"));
            } else {
                lines.push(format!(
                    "[ironmem] {total} drawers · {n_wings} wings (top: {})",
                    top.join(", ")
                ));
            }
        }
        Err(e) => tracing::warn!("session-start context: drawer counts unavailable: {e}"),
    }

    // 1b. Top rooms across all wings (largest first).
    match app.db.room_counts(None) {
        Ok(rooms) => {
            let top = top_counts(rooms, SESSION_CONTEXT_TOP_N);
            if !top.is_empty() {
                lines.push(format!("rooms (top: {})", top.join(", ")));
            }
        }
        Err(e) => tracing::warn!("session-start context: room counts unavailable: {e}"),
    }

    // 2. Active collab session + phase (DB-backed; snapshot is empty in the hook process).
    if let Some(root) = workspace_root {
        let repo_path = root.to_string_lossy();
        match app.db.with_transaction(|tx| {
            crate::collab::queue::find_active_session_by_repo(tx, repo_path.as_ref())
        }) {
            Ok(Some((id, phase))) => {
                let short = id.get(..SESSION_CONTEXT_SHORT_ID).unwrap_or(&id);
                lines.push(format!("collab {short} @ {phase}"));
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("session-start context: active collab lookup failed: {e}"),
        }
    }

    // 3. Last diary pointer (most recent entry in the diary wing, any room).
    match app.db.get_drawers(Some("diary"), None, 1) {
        Ok(entries) => {
            if let Some(d) = entries.first() {
                let short = d.id.get(..SESSION_CONTEXT_SHORT_ID).unwrap_or(&d.id);
                let excerpt = compact_excerpt(&d.content, SESSION_CONTEXT_DIARY_EXCERPT_BYTES);
                lines.push(format!("last diary {} ({short}): {excerpt}", d.date));
            }
        }
        Err(e) => tracing::warn!("session-start context: diary lookup failed: {e}"),
    }

    // 4. Memory protocol (verbatim) — reserve its budget so the status lines,
    // not the protocol, absorb any truncation.
    let protocol_line = format!("MEMORY_PROTOCOL: {}", crate::bootstrap::MEMORY_PROTOCOL);
    if lines.is_empty() {
        return Some(protocol_line);
    }
    let reserved = SESSION_CONTEXT_MAX_BYTES.saturating_sub(protocol_line.len() + 1);
    let status = truncate_text_to_byte_limit(&lines.join("\n"), reserved);
    if status.is_empty() {
        Some(protocol_line)
    } else {
        Some(format!("{status}\n{protocol_line}"))
    }
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
    use crate::mcp::server::{dispatch, run_server_io};
    use std::sync::{Arc, LazyLock, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

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
                "name": "diary_read",
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
                                "text": "Findings\n- High: duplicate writes still happen in crates/ironmem/src/hook.rs:52\n- Medium: add a regression test\nPR #2"
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
        let workspace = temp.path().join("ironmem");
        std::fs::create_dir_all(&workspace).unwrap();
        let transcript = temp.path().join("transcript.jsonl");
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                serde_json::json!({
                    "role": "assistant",
                    "content": "Findings\n- High: race condition in crates/ironmem/src/ingest/mod.rs:374\n- Medium: keep a regression test\nPR #1"
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
        let _metrics = crate::metrics::METRICS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("README.md"), "# Workspace\n\nMine me.").unwrap();

        std::env::remove_var("IRONMEM_METRICS");
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
        assert_eq!(
            app.db
                .occupancy_samples_for_session("session-1", 10)
                .unwrap()
                .len(),
            0
        );
        assert!(app.db.get_session_summary("session-1").unwrap().is_none());

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
    fn precompact_writes_occupancy_sample_and_increments_compactions() {
        let _env = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _metrics = crate::metrics::METRICS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::set_var("IRONMEM_DISABLE_MIGRATION", "1");
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("workspace")).unwrap();
        let transcript = temp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"assistant","message":{"usage":{"input_tokens":120000,"output_tokens":800,"cache_read_input_tokens":40000}}}"#,
                "\n",
                r#"{"type":"user","message":{"content":"next"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let config = Config {
            db_path: temp.path().join("memory.sqlite3"),
            model_dir: temp.path().join("model"),
            model_dir_explicit: true,
            state_dir: temp.path().join("hook_state"),
            mcp_access_mode: McpAccessMode::Trusted,
            embed_mode: EmbedMode::Noop,
        };
        {
            let app = App::new(config.clone()).unwrap();
            let s = crate::db::metrics::SessionSummary {
                session_id: "sess-occ".to_string(),
                harness: "claude".to_string(),
                workspace_root: None,
                started_at: Some("2026-06-11T00:00:00Z".to_string()),
                ended_at: None,
                peak_occupancy_pct: None,
                total_input_tokens: 0,
                total_output_tokens: 0,
                mcp_chars_served: 999,
                compactions: 0,
            };
            app.db.upsert_session_summary(&s).unwrap();
        }
        run_hook_with_input(
            "precompact",
            "claude",
            config.clone(),
            serde_json::json!({
                "cwd": temp.path().join("workspace"),
                "session_id": "sess-occ",
                "transcript_path": transcript.to_string_lossy(),
            }),
        )
        .unwrap();
        let app = App::new(config).unwrap();
        let samples = app
            .db
            .occupancy_samples_for_session("sess-occ", 10)
            .unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].hook_event.as_deref(), Some("precompact"));
        assert_eq!(samples[0].input_tokens, 120000);
        assert_eq!(samples[0].cache_read_input_tokens, 40000);
        assert_eq!(samples[0].context_window, 200000);
        assert!((samples[0].occupancy_pct.unwrap() - 0.8).abs() < 1e-9);
        let s = app.db.get_session_summary("sess-occ").unwrap().unwrap();
        assert_eq!(s.compactions, 1);
        assert_eq!(s.mcp_chars_served, 999, "RMW preserved mcp_chars_served");
        assert!((s.peak_occupancy_pct.unwrap() - 0.8).abs() < 1e-9);
        std::env::remove_var("IRONMEM_DISABLE_MIGRATION");
    }

    #[test]
    fn missing_usage_writes_zero_token_sample_when_session_present() {
        let _env = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _metrics = crate::metrics::METRICS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::set_var("IRONMEM_DISABLE_MIGRATION", "1");
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("workspace")).unwrap();
        let config = Config {
            db_path: temp.path().join("memory.sqlite3"),
            model_dir: temp.path().join("model"),
            model_dir_explicit: true,
            state_dir: temp.path().join("hook_state"),
            mcp_access_mode: McpAccessMode::Trusted,
            embed_mode: EmbedMode::Noop,
        };
        run_hook_with_input(
            "session-start",
            "claude",
            config.clone(),
            serde_json::json!({
                "cwd": temp.path().join("workspace"),
                "session_id": "sess-empty",
                "transcript_path": "/nonexistent/path.jsonl",
            }),
        )
        .unwrap();
        let app = App::new(config).unwrap();
        let samples = app
            .db
            .occupancy_samples_for_session("sess-empty", 10)
            .unwrap();
        assert_eq!(samples.len(), 1, "deterministic zero-token sample (D4)");
        assert_eq!(samples[0].input_tokens, 0);
        assert_eq!(samples[0].hook_event.as_deref(), Some("session-start"));
        std::env::remove_var("IRONMEM_DISABLE_MIGRATION");
    }

    #[test]
    fn absent_session_id_skips_occupancy_and_summary() {
        let _env = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _metrics = crate::metrics::METRICS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::set_var("IRONMEM_DISABLE_MIGRATION", "1");
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("workspace")).unwrap();
        let transcript = temp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"output_tokens":2}}}"#,
        )
        .unwrap();
        let config = Config {
            db_path: temp.path().join("memory.sqlite3"),
            model_dir: temp.path().join("model"),
            model_dir_explicit: true,
            state_dir: temp.path().join("hook_state"),
            mcp_access_mode: McpAccessMode::Trusted,
            embed_mode: EmbedMode::Noop,
        };
        run_hook_with_input(
            "session-start",
            "claude",
            config.clone(),
            serde_json::json!({
                "cwd": temp.path().join("workspace"),
                "transcript_path": transcript.to_string_lossy(),
            }),
        )
        .unwrap();
        let app = App::new(config).unwrap();
        assert_eq!(
            app.db.occupancy_samples_for_session("", 10).unwrap().len(),
            0
        );
        assert!(app.db.get_session_summary("").unwrap().is_none());
        assert!(app.db.get_session_summary("unknown").unwrap().is_none());
        std::env::remove_var("IRONMEM_DISABLE_MIGRATION");
    }

    #[test]
    fn stop_writes_session_stop_sample_and_summary_totals() {
        let _env = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _metrics = crate::metrics::METRICS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::set_var("IRONMEM_DISABLE_MIGRATION", "1");
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("workspace")).unwrap();
        let transcript = temp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":5}}}"#,
        )
        .unwrap();
        let config = Config {
            db_path: temp.path().join("memory.sqlite3"),
            model_dir: temp.path().join("model"),
            model_dir_explicit: true,
            state_dir: temp.path().join("hook_state"),
            mcp_access_mode: McpAccessMode::Trusted,
            embed_mode: EmbedMode::Noop,
        };
        run_hook_with_input(
            "stop",
            "codex",
            config.clone(),
            serde_json::json!({
                "cwd": temp.path().join("workspace"),
                "session_id": "sess-stop",
                "transcript_path": transcript.to_string_lossy(),
            }),
        )
        .unwrap();
        let app = App::new(config).unwrap();
        let samples = app
            .db
            .occupancy_samples_for_session("sess-stop", 10)
            .unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].harness, "codex");
        assert_eq!(samples[0].hook_event.as_deref(), Some("session-stop"));
        assert_eq!(samples[0].input_tokens, 10);
        assert_eq!(samples[0].cache_read_input_tokens, 5);
        let s = app.db.get_session_summary("sess-stop").unwrap().unwrap();
        assert_eq!(s.harness, "codex");
        assert!(s.ended_at.is_some());
        assert_eq!(s.total_input_tokens, 10);
        assert_eq!(s.total_output_tokens, 2);
        assert_eq!(s.compactions, 0);
        std::env::remove_var("IRONMEM_DISABLE_MIGRATION");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_and_hook_share_sanitized_session_summary_key() {
        let _env = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _metrics = crate::metrics::METRICS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::remove_var("IRONMEM_HARNESS");
        std::env::set_var("IRONMEM_DISABLE_MIGRATION", "1");
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let transcript = temp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":5}}}"#,
        )
        .unwrap();
        let config = Config {
            db_path: temp.path().join("memory.sqlite3"),
            model_dir: temp.path().join("model"),
            model_dir_explicit: true,
            state_dir: temp.path().join("hook_state"),
            mcp_access_mode: McpAccessMode::Trusted,
            embed_mode: EmbedMode::Noop,
        };

        {
            #[allow(clippy::arc_with_non_send_sync)]
            let app = Arc::new(App::new(config.clone()).unwrap());
            let (mut client_in, server_in) = tokio::io::duplex(4096);
            let (server_out, mut client_out) = tokio::io::duplex(4096);
            client_in
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"sessionId\":\"../same-session\"}}\n",
                )
                .await
                .unwrap();
            client_in.shutdown().await.unwrap();
            run_server_io(Arc::clone(&app), BufReader::new(server_in), server_out)
                .await
                .unwrap();
            let mut out = String::new();
            client_out.read_to_string(&mut out).await.unwrap();
        }

        run_hook_with_input(
            "session-start",
            "claude",
            config.clone(),
            serde_json::json!({
                "cwd": workspace,
                "session_id": "../same-session",
                "transcript_path": transcript.to_string_lossy(),
            }),
        )
        .unwrap();

        let app = App::new(config).unwrap();
        assert!(app
            .db
            .get_session_summary("../same-session")
            .unwrap()
            .is_none());
        let summary = app
            .db
            .get_session_summary("same-session")
            .unwrap()
            .expect("sanitized summary key exists");
        assert!(summary.mcp_chars_served > 0);
        assert_eq!(summary.total_input_tokens, 10);
        assert_eq!(summary.total_output_tokens, 2);
        assert_eq!(
            app.db
                .occupancy_samples_for_session("same-session", 10)
                .unwrap()
                .len(),
            1
        );
        std::env::remove_var("IRONMEM_DISABLE_MIGRATION");
    }

    #[test]
    fn kill_switch_suppresses_occupancy() {
        let _env = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _metrics = crate::metrics::METRICS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("IRONMEM_METRICS", "0");
        std::env::set_var("IRONMEM_DISABLE_MIGRATION", "1");
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("workspace")).unwrap();
        let config = Config {
            db_path: temp.path().join("memory.sqlite3"),
            model_dir: temp.path().join("model"),
            model_dir_explicit: true,
            state_dir: temp.path().join("hook_state"),
            mcp_access_mode: McpAccessMode::Trusted,
            embed_mode: EmbedMode::Noop,
        };
        run_hook_with_input(
            "precompact",
            "claude",
            config.clone(),
            serde_json::json!({
                "cwd": temp.path().join("workspace"),
                "session_id": "sess-off",
                "transcript_path": "/nonexistent.jsonl",
            }),
        )
        .unwrap();
        let app = App::new(config).unwrap();
        assert_eq!(
            app.db
                .occupancy_samples_for_session("sess-off", 10)
                .unwrap()
                .len(),
            0
        );
        std::env::remove_var("IRONMEM_METRICS");
        std::env::remove_var("IRONMEM_DISABLE_MIGRATION");
    }

    #[test]
    fn truncate_text_to_byte_limit_respects_char_boundaries() {
        let s = "a".repeat(23_999) + "é";
        assert_eq!(s.len(), 24_001);
        let truncated = truncate_text_to_byte_limit(&s, 24_000);
        assert_eq!(truncated.len(), 23_999);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn hook_response_serializes_hook_specific_output_camelcase_and_omits_when_none() {
        let none = HookResponse {
            decision: None,
            reason: None,
            hook: "session-start".into(),
            harness: "claude-code".into(),
            workspace_root: None,
            hook_specific_output: None,
        };
        let v = serde_json::to_value(&none).unwrap();
        assert!(
            v.get("hookSpecificOutput").is_none(),
            "None must omit the key"
        );

        let some = HookResponse {
            decision: None,
            reason: None,
            hook: "session-start".into(),
            harness: "claude-code".into(),
            workspace_root: None,
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: "SessionStart".into(),
                additional_context: "hi".into(),
            }),
        };
        let v = serde_json::to_value(&some).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "hi");
    }

    #[test]
    fn compact_excerpt_collapses_control_chars_and_caps_bytes() {
        let out = compact_excerpt("  line one\nline\ttwo\r\nthree  ", 1000);
        assert_eq!(out, "line one line two three");
        assert!(!out.contains('\n') && !out.contains('\t') && !out.contains('\r'));

        let multi = "é".repeat(50); // 100 bytes
        let capped = compact_excerpt(&multi, 10);
        assert!(capped.len() <= 10);
        assert!(capped.is_char_boundary(capped.len()));
    }

    fn seed_drawer(app: &App, content: &str, wing: &str, room: &str) {
        let embedding = {
            let mut e = app.embedder.write().unwrap();
            e.embed_one(content).unwrap()
        };
        let id = generate_id(content, wing, room);
        app.db
            .insert_drawer(&id, content, &embedding, wing, room, "test", "test")
            .unwrap();
    }

    #[test]
    fn build_session_start_context_includes_counts_collab_diary_and_protocol() {
        let app = App::open_for_test().unwrap();
        let repo = "/tmp/repo-ctx-87";

        // "zeta" (alphabetically last) gets the MOST drawers; "alpha" the fewest —
        // proves the count-DESC re-sort actually reorders the alphabetical DB output.
        for i in 0..3 {
            seed_drawer(&app, &format!("zeta body {i}"), "zeta", "r");
        }
        seed_drawer(&app, "alpha body", "alpha", "r");

        // Long diary entry → must appear as a capped excerpt, never in full.
        let long_body = "x".repeat(1000);
        diary::write_entry(&app, &long_body, "diary", "test", 8000).unwrap();

        // Active collab session for this repo.
        app.db
            .with_transaction(|tx| {
                crate::collab::queue::create_session(
                    tx,
                    "ctxsess-1234abcd",
                    repo,
                    "main",
                    None,
                    crate::collab::Agent::Claude,
                )
            })
            .unwrap();

        let block = build_session_start_context(&app, Some(std::path::Path::new(repo))).unwrap();

        assert!(block.contains("drawers"), "drawer count line present");
        let zpos = block.find("zeta").expect("zeta listed");
        let apos = block.find("alpha").expect("alpha listed");
        assert!(
            zpos < apos,
            "top wings must be sorted by count desc, not alphabetical"
        );
        assert!(
            block.contains("collab ctxsess"),
            "active collab line present"
        );
        assert!(block.contains("last diary"), "diary pointer present");
        assert!(
            !block.contains(&long_body),
            "diary body must be excerpted, not dumped"
        );
        assert!(block.contains("MEMORY_PROTOCOL"), "memory protocol present");
        assert!(
            block.len() <= SESSION_CONTEXT_MAX_BYTES,
            "within byte budget"
        );
    }

    #[test]
    fn session_start_context_preserves_memory_protocol_under_long_names() {
        let app = App::open_for_test().unwrap();
        // Many wings/rooms with long names so the status lines alone would blow
        // the byte budget. MEMORY_PROTOCOL (the behavior-changing instruction)
        // must still survive — it gets a reserved budget, never truncated off.
        for w in 0..8 {
            let wing = format!("w{w}{}", "x".repeat(200));
            let room = format!("r{w}{}", "y".repeat(200));
            for i in 0..2 {
                seed_drawer(&app, &format!("body {w} {i}"), &wing, &room);
            }
        }
        let block = build_session_start_context(&app, None).unwrap();
        assert!(
            block.contains("MEMORY_PROTOCOL"),
            "memory protocol must survive truncation under long wing/room names"
        );
        assert!(
            block.len() <= SESSION_CONTEXT_MAX_BYTES,
            "still within byte budget"
        );
    }

    #[test]
    fn session_start_claude_emits_hook_specific_output() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let config = Config {
            db_path: temp.path().join("memory.sqlite3"),
            model_dir: temp.path().join("model"),
            model_dir_explicit: true,
            state_dir: temp.path().join("hook_state"),
            mcp_access_mode: McpAccessMode::Trusted,
            embed_mode: EmbedMode::Noop,
        };
        let resp = run_hook_with_input(
            "session-start",
            "claude-code",
            config,
            serde_json::json!({ "cwd": workspace, "session_id": "s-cc" }),
        )
        .unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(!ctx.is_empty());
        assert!(ctx.contains("MEMORY_PROTOCOL"));
        assert!(ctx.len() <= SESSION_CONTEXT_MAX_BYTES);
    }

    #[test]
    fn session_start_codex_omits_hook_specific_output() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let config = Config {
            db_path: temp.path().join("memory.sqlite3"),
            model_dir: temp.path().join("model"),
            model_dir_explicit: true,
            state_dir: temp.path().join("hook_state"),
            mcp_access_mode: McpAccessMode::Trusted,
            embed_mode: EmbedMode::Noop,
        };
        let resp = run_hook_with_input(
            "session-start",
            "codex",
            config,
            serde_json::json!({ "cwd": workspace, "session_id": "s-cx" }),
        )
        .unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert!(
            v.get("hookSpecificOutput").is_none(),
            "codex must silently degrade"
        );
        assert_eq!(v["hook"], "session-start");
    }
}

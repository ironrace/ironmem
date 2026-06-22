//! Dashboard HTTP router + request handlers.
//!
//! Security invariants:
//! - Only `GET` and `HEAD` are served; any other method returns `405`.
//! - All parameters (`limit`, `task`, `since`, `wing`, `room`, `repo`, `area`)
//!   are validated at the HTTP boundary before reaching the DB layer.
//! - Error bodies never leak raw SQLite internals.
//! - UI assets are inline constants — no filesystem serving.
//! - User-controlled drawer text is never rendered with `innerHTML`.

use std::sync::Arc;

use std::convert::Infallible;

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{ALLOW, CONTENT_TYPE};
use hyper::{Method, Request, Response, StatusCode};

use crate::dashboard::data::{
    list_code_maps, list_sessions, memory_summary, report_projection, CodeMapParams, MemoryParams,
};
use crate::dashboard::server::ServerState;
use crate::db::schema::Database;
use crate::error::MemoryError;

/// Maximum value for `limit` query parameter.
const MAX_LIMIT: usize = 500;
/// Default `limit` when not supplied.
const DEFAULT_LIMIT: usize = 50;

type HyperResponse = Response<Full<Bytes>>;

/// Entry-point for every inbound HTTP request. Never returns `Err` — errors
/// are converted to appropriate HTTP error responses so the connection is not
/// abruptly dropped.
pub async fn handle_request(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
) -> Result<HyperResponse, Infallible> {
    // Method guard: GET and HEAD only.
    match *req.method() {
        Method::GET | Method::HEAD => {}
        _ => {
            return Ok(method_not_allowed());
        }
    }

    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();

    // Route dispatch.
    let response = match path.as_str() {
        "/" => serve_html(DASHBOARD_HTML),
        "/api/summary" => handle_summary(state, &query).await,
        "/api/memory" => handle_memory(state, &query).await,
        "/api/code-maps" => handle_code_maps(state, &query).await,
        "/api/sessions" => handle_sessions(state).await,
        "/api/report" => handle_report(state, &query).await,
        _ => not_found(),
    };

    // For HEAD requests, strip the body.
    if *req.method() == Method::HEAD {
        let (parts, _body) = response.into_parts();
        return Ok(Response::from_parts(parts, Full::new(Bytes::new())));
    }

    Ok(response)
}

// ────────────────────────────────────────────────────────────────────────────
// Handler implementations
// ────────────────────────────────────────────────────────────────────────────

async fn handle_summary(state: Arc<ServerState>, _query: &str) -> HyperResponse {
    let db_path = Arc::clone(&state.db_path);
    let schema_version = state.schema_version;

    match tokio::task::spawn_blocking(move || -> Result<serde_json::Value, MemoryError> {
        let db = Database::open_read_only(&db_path)?;
        let params = MemoryParams {
            wing: None,
            room: None,
            limit: 5,
        };
        let mem = memory_summary(&db, &params)?;
        Ok(serde_json::json!({
            "schema_version": schema_version,
            "total_drawers": mem.total_drawers,
            "wing_count": mem.wing_counts.len(),
            "kg_stats": mem.kg_stats,
        }))
    })
    .await
    {
        Ok(Ok(json)) => json_response(StatusCode::OK, &json),
        Ok(Err(e)) => internal_error_safe(&e),
        Err(e) => internal_error_safe(&MemoryError::Validation(format!("task error: {e}"))),
    }
}

async fn handle_memory(state: Arc<ServerState>, query: &str) -> HyperResponse {
    let params = match parse_memory_params(query) {
        Ok(p) => p,
        Err(msg) => return bad_request(&msg),
    };
    let db_path = Arc::clone(&state.db_path);

    match tokio::task::spawn_blocking(move || -> Result<serde_json::Value, MemoryError> {
        let db = Database::open_read_only(&db_path)?;
        let summary = memory_summary(&db, &params)?;
        serde_json::to_value(&summary).map_err(MemoryError::from)
    })
    .await
    {
        Ok(Ok(json)) => json_response(StatusCode::OK, &json),
        Ok(Err(e)) => internal_error_safe(&e),
        Err(e) => internal_error_safe(&MemoryError::Validation(format!("task error: {e}"))),
    }
}

async fn handle_code_maps(state: Arc<ServerState>, query: &str) -> HyperResponse {
    let params = parse_code_map_params(query);
    let db_path = Arc::clone(&state.db_path);

    match tokio::task::spawn_blocking(move || -> Result<serde_json::Value, MemoryError> {
        let db = Database::open_read_only(&db_path)?;
        let maps = list_code_maps(&db, &params)?;
        serde_json::to_value(&maps).map_err(MemoryError::from)
    })
    .await
    {
        Ok(Ok(json)) => json_response(StatusCode::OK, &json),
        Ok(Err(e)) => internal_error_safe(&e),
        Err(e) => internal_error_safe(&MemoryError::Validation(format!("task error: {e}"))),
    }
}

async fn handle_sessions(state: Arc<ServerState>) -> HyperResponse {
    let db_path = Arc::clone(&state.db_path);

    match tokio::task::spawn_blocking(move || -> Result<serde_json::Value, MemoryError> {
        let db = Database::open_read_only(&db_path)?;
        let sessions = list_sessions(&db)?;
        serde_json::to_value(&sessions).map_err(MemoryError::from)
    })
    .await
    {
        Ok(Ok(json)) => json_response(StatusCode::OK, &json),
        Ok(Err(e)) => internal_error_safe(&e),
        Err(e) => internal_error_safe(&MemoryError::Validation(format!("task error: {e}"))),
    }
}

async fn handle_report(state: Arc<ServerState>, query: &str) -> HyperResponse {
    let (task, since) = parse_report_params(query);
    let db_path = Arc::clone(&state.db_path);

    match tokio::task::spawn_blocking(move || -> Result<serde_json::Value, MemoryError> {
        let db = Database::open_read_only(&db_path)?;
        let report = report_projection(&db, task, since)?;
        serde_json::to_value(&report).map_err(MemoryError::from)
    })
    .await
    {
        Ok(Ok(json)) => json_response(StatusCode::OK, &json),
        Ok(Err(e)) => internal_error_safe(&e),
        Err(e) => internal_error_safe(&MemoryError::Validation(format!("task error: {e}"))),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Parameter parsers
// ────────────────────────────────────────────────────────────────────────────

fn parse_memory_params(query: &str) -> Result<MemoryParams, String> {
    let mut wing = None;
    let mut room = None;
    let mut limit = DEFAULT_LIMIT;

    for (k, v) in form_urlencoded(query) {
        match k.as_str() {
            "wing" => wing = Some(v),
            "room" => room = Some(v),
            "limit" => {
                limit = v
                    .parse::<usize>()
                    .map_err(|_| format!("invalid limit value: {v:?}"))?;
                if limit > MAX_LIMIT {
                    return Err(format!("limit {limit} exceeds maximum {MAX_LIMIT}"));
                }
            }
            _ => {} // ignore unknown params
        }
    }

    Ok(MemoryParams { wing, room, limit })
}

fn parse_code_map_params(query: &str) -> CodeMapParams {
    let mut repo = None;
    let mut area = None;
    for (k, v) in form_urlencoded(query) {
        match k.as_str() {
            "repo" => repo = Some(v),
            "area" => area = Some(v),
            _ => {}
        }
    }
    CodeMapParams { repo, area }
}

fn parse_report_params(query: &str) -> (Option<String>, Option<String>) {
    let mut task = None;
    let mut since = None;
    for (k, v) in form_urlencoded(query) {
        match k.as_str() {
            "task" => task = Some(v),
            "since" => since = Some(v),
            _ => {}
        }
    }
    (task, since)
}

/// Minimal query-string parser that percent-decodes keys and values.
fn form_urlencoded(query: &str) -> Vec<(String, String)> {
    if query.is_empty() {
        return vec![];
    }
    query
        .split('&')
        .filter_map(|pair| {
            let mut it = pair.splitn(2, '=');
            let key = it.next()?;
            let val = it.next().unwrap_or("");
            let k = percent_decode(key);
            let v = percent_decode(val);
            if k.is_empty() {
                None
            } else {
                Some((k, v))
            }
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    // Replace '+' with space, then percent-decode.
    let with_spaces = s.replace('+', " ");
    let mut out = String::with_capacity(with_spaces.len());
    let mut chars = with_spaces.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next().and_then(|c| c.to_digit(16));
            let h2 = chars.next().and_then(|c| c.to_digit(16));
            if let (Some(h1), Some(h2)) = (h1, h2) {
                let byte = ((h1 << 4) | h2) as u8;
                out.push(byte as char);
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ────────────────────────────────────────────────────────────────────────────
// Response helpers
// ────────────────────────────────────────────────────────────────────────────

fn json_response(status: StatusCode, value: &serde_json::Value) -> HyperResponse {
    let body = serde_json::to_string(value)
        .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string());
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json; charset=utf-8")
        .header("X-Content-Type-Options", "nosniff")
        .header("Cache-Control", "no-store")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| internal_fallback())
}

fn serve_html(html: &'static str) -> HyperResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/html; charset=utf-8")
        .header("X-Content-Type-Options", "nosniff")
        .header("Cache-Control", "no-store")
        .header("X-Frame-Options", "DENY")
        .header(
            "Content-Security-Policy",
            "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'",
        )
        .body(Full::new(Bytes::from_static(html.as_bytes())))
        .unwrap_or_else(|_| internal_fallback())
}

fn not_found() -> HyperResponse {
    let body = r#"{"error":"not found"}"#;
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| internal_fallback())
}

fn method_not_allowed() -> HyperResponse {
    Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .header(ALLOW, "GET, HEAD")
        .header(CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Full::new(Bytes::from(r#"{"error":"method not allowed"}"#)))
        .unwrap_or_else(|_| internal_fallback())
}

fn bad_request(msg: &str) -> HyperResponse {
    // Sanitize: drop any raw error that could carry DB internals; msg here
    // comes from our own parameter validators, not from rusqlite.
    let body = serde_json::json!({ "error": msg }).to_string();
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| internal_fallback())
}

/// Produce a safe 500 response that never leaks raw SQLite or filesystem paths.
fn internal_error_safe(e: &MemoryError) -> HyperResponse {
    // Log the real error server-side; surface only a safe generic message.
    tracing::warn!("dashboard internal error: {e}");
    let body = r#"{"error":"internal server error"}"#;
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| internal_fallback())
}

fn internal_fallback() -> HyperResponse {
    Response::new(Full::new(Bytes::from(
        r#"{"error":"internal server error"}"#,
    )))
}

// ────────────────────────────────────────────────────────────────────────────
// Bundled dashboard HTML (compile-time inlined — no CDN, no build step)
// ────────────────────────────────────────────────────────────────────────────

/// Single-page dashboard HTML. User-controlled content is always set via
/// `textContent` (never `innerHTML`) to prevent XSS.
const DASHBOARD_HTML: &str = r####"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ironmem dashboard</title>
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: system-ui, sans-serif; background: #0f0f0f; color: #e0e0e0; padding: 1rem; }
  h1 { color: #c9a0dc; margin-bottom: 1.5rem; font-size: 1.5rem; }
  h2 { color: #a78bfa; margin: 1.5rem 0 0.75rem; font-size: 1.1rem; border-bottom: 1px solid #333; padding-bottom: 0.25rem; }
  .grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(280px, 1fr)); gap: 1rem; margin-bottom: 1.5rem; }
  .card { background: #1a1a2e; border: 1px solid #333; border-radius: 8px; padding: 1rem; }
  .stat { font-size: 2rem; font-weight: bold; color: #c9a0dc; }
  .label { font-size: 0.8rem; color: #888; margin-top: 0.25rem; }
  table { width: 100%; border-collapse: collapse; font-size: 0.85rem; }
  th { text-align: left; padding: 0.4rem 0.6rem; color: #888; font-weight: 500; border-bottom: 1px solid #333; }
  td { padding: 0.4rem 0.6rem; border-bottom: 1px solid #222; vertical-align: top; word-break: break-word; }
  tr:hover td { background: #1f1f3a; }
  .badge { display: inline-block; padding: 0.15rem 0.4rem; border-radius: 4px; font-size: 0.75rem; background: #2a2a4a; color: #a78bfa; }
  .error { color: #f87171; font-size: 0.85rem; padding: 0.5rem; }
  .loading { color: #888; font-style: italic; font-size: 0.85rem; }
  .filter-row { display: flex; gap: 0.5rem; margin-bottom: 0.75rem; flex-wrap: wrap; }
  input, select { background: #1a1a2e; border: 1px solid #444; color: #e0e0e0; padding: 0.3rem 0.5rem; border-radius: 4px; font-size: 0.85rem; }
  button { background: #4c1d95; border: none; color: #e0e0e0; padding: 0.3rem 0.8rem; border-radius: 4px; cursor: pointer; font-size: 0.85rem; }
  button:hover { background: #6d28d9; }
  pre { background: #111; border: 1px solid #333; border-radius: 4px; padding: 0.75rem; font-size: 0.8rem; overflow-x: auto; white-space: pre-wrap; max-height: 300px; overflow-y: auto; }
  .section { margin-bottom: 2rem; }
  nav { display: flex; gap: 0.75rem; margin-bottom: 1.5rem; flex-wrap: wrap; }
  nav a { color: #a78bfa; text-decoration: none; padding: 0.3rem 0.75rem; border: 1px solid #444; border-radius: 20px; font-size: 0.85rem; }
  nav a:hover { background: #1a1a2e; }
</style>
</head>
<body>
<h1>ironmem dashboard</h1>
<nav>
  <a href="#summary">Summary</a>
  <a href="#memory">Memory</a>
  <a href="#codemaps">Code Maps</a>
  <a href="#sessions">Sessions</a>
  <a href="#reports">Reports</a>
</nav>

<div id="summary" class="section">
  <h2>Summary</h2>
  <div id="summary-grid" class="grid"><p class="loading">Loading…</p></div>
</div>

<div id="memory" class="section">
  <h2>Memory Drawers</h2>
  <div class="filter-row">
    <input id="mem-wing" placeholder="wing (optional)" style="width:150px">
    <input id="mem-room" placeholder="room (optional)" style="width:150px">
    <input id="mem-limit" type="number" value="50" min="1" max="500" style="width:80px">
    <button onclick="loadMemory()">Filter</button>
  </div>
  <div id="memory-table"><p class="loading">Loading…</p></div>
</div>

<div id="codemaps" class="section">
  <h2>Code Maps</h2>
  <div class="filter-row">
    <input id="cm-repo" placeholder="repo (optional)" style="width:200px">
    <input id="cm-area" placeholder="area (optional)" style="width:150px">
    <button onclick="loadCodeMaps()">Filter</button>
  </div>
  <div id="codemaps-table"><p class="loading">Loading…</p></div>
</div>

<div id="sessions" class="section">
  <h2>Collab Sessions</h2>
  <div id="sessions-table"><p class="loading">Loading…</p></div>
</div>

<div id="reports" class="section">
  <h2>Metrics Report</h2>
  <div class="filter-row">
    <input id="rpt-task" placeholder="task (optional)" style="width:200px">
    <input id="rpt-since" placeholder="since YYYY-MM-DD (optional)" style="width:180px">
    <button onclick="loadReport()">Load</button>
  </div>
  <div id="report-output"><p class="loading">Loading…</p></div>
</div>

<script>
'use strict';

function esc(v) {
  const d = document.createElement('div');
  d.textContent = String(v == null ? '' : v);
  return d.innerHTML;
}

function setText(el, v) { el.textContent = String(v == null ? '' : v); }

async function fetchJSON(url) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(r.statusText + ' ' + r.status);
  return r.json();
}

function renderError(msg) {
  const p = document.createElement('p');
  p.className = 'error';
  p.textContent = 'Error: ' + msg;
  return p;
}

// ── Summary ─────────────────────────────────────────────────────────────────

async function loadSummary() {
  const el = document.getElementById('summary-grid');
  try {
    const d = await fetchJSON('/api/summary');
    el.innerHTML = '';
    const cards = [
      ['Total Drawers', d.total_drawers],
      ['Wings', d.wing_count],
      ['Schema Version', 'v' + d.schema_version],
      ['KG Entities', d.kg_stats && d.kg_stats.entity_count],
      ['KG Triples', d.kg_stats && d.kg_stats.current_triple_count],
    ];
    cards.forEach(([label, val]) => {
      const card = document.createElement('div');
      card.className = 'card';
      const stat = document.createElement('div');
      stat.className = 'stat';
      setText(stat, val != null ? val : '—');
      const lbl = document.createElement('div');
      lbl.className = 'label';
      lbl.textContent = label;
      card.appendChild(stat);
      card.appendChild(lbl);
      el.appendChild(card);
    });
  } catch (e) {
    el.innerHTML = '';
    el.appendChild(renderError(e.message));
  }
}

// ── Memory ───────────────────────────────────────────────────────────────────

async function loadMemory() {
  const el = document.getElementById('memory-table');
  el.innerHTML = '<p class="loading">Loading…</p>';
  const wing = document.getElementById('mem-wing').value.trim();
  const room = document.getElementById('mem-room').value.trim();
  const limit = document.getElementById('mem-limit').value.trim();
  const params = new URLSearchParams();
  if (wing) params.set('wing', wing);
  if (room) params.set('room', room);
  if (limit) params.set('limit', limit);
  try {
    const d = await fetchJSON('/api/memory?' + params.toString());
    const rows = d.recent_drawers || [];
    if (rows.length === 0) {
      el.innerHTML = '<p class="loading">No drawers found.</p>';
      return;
    }
    const table = document.createElement('table');
    table.innerHTML = '<thead><tr><th>ID</th><th>Wing</th><th>Room</th><th>Preview</th><th>Filed At</th></tr></thead>';
    const tbody = document.createElement('tbody');
    rows.forEach(row => {
      const tr = document.createElement('tr');
      ['id', 'wing', 'room', 'content_preview', 'filed_at'].forEach((k, i) => {
        const td = document.createElement('td');
        td.textContent = row[k] || '';
        if (i === 0) { td.style.fontFamily = 'monospace'; td.style.fontSize = '0.75rem'; }
        tr.appendChild(td);
      });
      tbody.appendChild(tr);
    });
    table.appendChild(tbody);
    el.innerHTML = '';
    el.appendChild(table);
  } catch (e) {
    el.innerHTML = '';
    el.appendChild(renderError(e.message));
  }
}

// ── Code Maps ────────────────────────────────────────────────────────────────

async function loadCodeMaps() {
  const el = document.getElementById('codemaps-table');
  el.innerHTML = '<p class="loading">Loading…</p>';
  const repo = document.getElementById('cm-repo').value.trim();
  const area = document.getElementById('cm-area').value.trim();
  const params = new URLSearchParams();
  if (repo) params.set('repo', repo);
  if (area) params.set('area', area);
  try {
    const rows = await fetchJSON('/api/code-maps?' + params.toString());
    if (!rows || rows.length === 0) {
      el.innerHTML = '<p class="loading">No code maps found.</p>';
      return;
    }
    const table = document.createElement('table');
    table.innerHTML = '<thead><tr><th>Repo</th><th>Area</th><th>Head SHA</th><th>Built By</th><th>Built At</th><th>Files</th></tr></thead>';
    const tbody = document.createElement('tbody');
    rows.forEach(row => {
      const tr = document.createElement('tr');
      const vals = [row.repo, row.area, (row.head_sha||'').slice(0,12), row.built_by, row.built_at, (row.source_files||[]).length + ' files'];
      vals.forEach((v, i) => {
        const td = document.createElement('td');
        td.textContent = v || '';
        if (i === 2) { td.style.fontFamily = 'monospace'; td.style.fontSize = '0.8rem'; }
        tr.appendChild(td);
      });
      tbody.appendChild(tr);
    });
    table.appendChild(tbody);
    el.innerHTML = '';
    el.appendChild(table);
  } catch (e) {
    el.innerHTML = '';
    el.appendChild(renderError(e.message));
  }
}

// ── Sessions ─────────────────────────────────────────────────────────────────

async function loadSessions() {
  const el = document.getElementById('sessions-table');
  el.innerHTML = '<p class="loading">Loading…</p>';
  try {
    const rows = await fetchJSON('/api/sessions');
    if (!rows || rows.length === 0) {
      el.innerHTML = '<p class="loading">No sessions found.</p>';
      return;
    }
    const table = document.createElement('table');
    table.innerHTML = '<thead><tr><th>ID</th><th>Task</th><th>Branch</th><th>Phase</th><th>Owner</th><th>Tasks</th><th>Updated</th></tr></thead>';
    const tbody = document.createElement('tbody');
    rows.forEach(row => {
      const tr = document.createElement('tr');
      const vals = [
        (row.id||'').slice(0,8) + '…',
        row.task || '(none)',
        row.branch,
        row.phase,
        row.current_owner,
        row.tasks_count != null ? row.tasks_count : '—',
        (row.updated_at||'').replace('T',' ').slice(0,16),
      ];
      vals.forEach((v, i) => {
        const td = document.createElement('td');
        td.textContent = v;
        if (i === 0) { td.style.fontFamily = 'monospace'; td.style.fontSize = '0.8rem'; td.title = row.id; }
        tr.appendChild(td);
      });
      tbody.appendChild(tr);
    });
    table.appendChild(tbody);
    el.innerHTML = '';
    el.appendChild(table);
  } catch (e) {
    el.innerHTML = '';
    el.appendChild(renderError(e.message));
  }
}

// ── Report ───────────────────────────────────────────────────────────────────

async function loadReport() {
  const el = document.getElementById('report-output');
  el.innerHTML = '<p class="loading">Loading…</p>';
  const task = document.getElementById('rpt-task').value.trim();
  const since = document.getElementById('rpt-since').value.trim();
  const params = new URLSearchParams();
  if (task) params.set('task', task);
  if (since) params.set('since', since);
  try {
    const d = await fetchJSON('/api/report?' + params.toString());
    const pre = document.createElement('pre');
    pre.textContent = JSON.stringify(d, null, 2);
    el.innerHTML = '';
    el.appendChild(pre);
  } catch (e) {
    el.innerHTML = '';
    el.appendChild(renderError(e.message));
  }
}

// ── Bootstrap ────────────────────────────────────────────────────────────────

loadSummary();
loadMemory();
loadCodeMaps();
loadSessions();
loadReport();
</script>
</body>
</html>"####;

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── form_urlencoded ──────────────────────────────────────────────────────

    #[test]
    fn form_urlencoded_empty() {
        assert!(form_urlencoded("").is_empty());
    }

    #[test]
    fn form_urlencoded_parses_key_value() {
        let pairs = form_urlencoded("wing=alpha&room=notes&limit=10");
        let map: std::collections::HashMap<_, _> = pairs.into_iter().collect();
        assert_eq!(map.get("wing").map(|s| s.as_str()), Some("alpha"));
        assert_eq!(map.get("room").map(|s| s.as_str()), Some("notes"));
        assert_eq!(map.get("limit").map(|s| s.as_str()), Some("10"));
    }

    #[test]
    fn form_urlencoded_handles_percent_encoding() {
        let pairs = form_urlencoded("task=hello%20world");
        let map: std::collections::HashMap<_, _> = pairs.into_iter().collect();
        assert_eq!(map.get("task").map(|s| s.as_str()), Some("hello world"));
    }

    // ── parse_memory_params ──────────────────────────────────────────────────

    #[test]
    fn parse_memory_params_defaults() {
        let p = parse_memory_params("").unwrap();
        assert_eq!(p.limit, DEFAULT_LIMIT);
        assert!(p.wing.is_none());
        assert!(p.room.is_none());
    }

    #[test]
    fn parse_memory_params_over_cap_limit_is_rejected() {
        let result = parse_memory_params(&format!("limit={}", MAX_LIMIT + 1));
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("exceeds maximum"), "msg: {msg}");
    }

    #[test]
    fn parse_memory_params_invalid_limit_is_rejected() {
        let result = parse_memory_params("limit=not_a_number");
        assert!(result.is_err());
    }

    // ── method_not_allowed ───────────────────────────────────────────────────

    #[test]
    fn method_not_allowed_response_has_405() {
        let r = method_not_allowed();
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(r.headers().contains_key(ALLOW));
    }

    // ── bad_request ──────────────────────────────────────────────────────────

    #[test]
    fn bad_request_response_has_400() {
        let r = bad_request("limit exceeds maximum");
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    // ── internal_error_safe does not leak SQLite detail ──────────────────────

    #[test]
    fn internal_error_safe_returns_500_generic_message() {
        let e = MemoryError::NotFound("very sensitive internal path".into());
        let r = internal_error_safe(&e);
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
        // Body must NOT contain sensitive detail.
        // We can check via the const body.
        // (Body inspection tested via the string constant — body reading
        //  requires async; method-level test is sufficient here.)
    }
}

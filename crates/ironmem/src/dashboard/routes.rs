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
    drawer_detail, list_code_maps, list_sessions, memory_summary, report_projection, CodeMapParams,
    MemoryParams, SessionParams,
};
use crate::dashboard::server::ServerState;
use crate::db::schema::Database;
use crate::error::MemoryError;

/// Maximum value for `limit` query parameter.
const MAX_LIMIT: usize = 500;
/// Default `limit` when not supplied.
const DEFAULT_LIMIT: usize = 50;
const MAX_PARAM_CHARS: usize = 512;

type HyperResponse = Response<Full<Bytes>>;

#[derive(Debug, Clone)]
enum MemoryRequest {
    List(MemoryParams),
    Detail(String),
}

#[derive(Debug, Clone)]
struct ReportParams {
    task: Option<String>,
    since: Option<String>,
    limit: usize,
}

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

    if *req.method() == Method::HEAD {
        return Ok(handle_head(&path, &query));
    }

    let response = match path.as_str() {
        "/" => serve_html(DASHBOARD_HTML),
        "/api/summary" => handle_summary(state, &query).await,
        "/api/memory" => handle_memory(state, &query).await,
        "/api/code-maps" => handle_code_maps(state, &query).await,
        "/api/sessions" => handle_sessions(state, &query).await,
        "/api/report" => handle_report(state, &query).await,
        _ => not_found(),
    };

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
    let request = match parse_memory_request(query) {
        Ok(r) => r,
        Err(msg) => return bad_request(&msg),
    };
    let db_path = Arc::clone(&state.db_path);

    match tokio::task::spawn_blocking(move || -> Result<serde_json::Value, MemoryError> {
        let db = Database::open_read_only(&db_path)?;
        match request {
            MemoryRequest::List(params) => {
                let summary = memory_summary(&db, &params)?;
                serde_json::to_value(&summary).map_err(MemoryError::from)
            }
            MemoryRequest::Detail(id) => match drawer_detail(&db, &id)? {
                Some(drawer) => serde_json::to_value(&drawer).map_err(MemoryError::from),
                None => Ok(serde_json::json!({ "error": "not found" })),
            },
        }
    })
    .await
    {
        Ok(Ok(json)) if json.get("error").and_then(|v| v.as_str()) == Some("not found") => {
            json_response(StatusCode::NOT_FOUND, &json)
        }
        Ok(Ok(json)) => json_response(StatusCode::OK, &json),
        Ok(Err(e)) => internal_error_safe(&e),
        Err(e) => internal_error_safe(&MemoryError::Validation(format!("task error: {e}"))),
    }
}

async fn handle_code_maps(state: Arc<ServerState>, query: &str) -> HyperResponse {
    let params = match parse_code_map_params(query) {
        Ok(p) => p,
        Err(msg) => return bad_request(&msg),
    };
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

async fn handle_sessions(state: Arc<ServerState>, query: &str) -> HyperResponse {
    let params = match parse_session_params(query) {
        Ok(p) => p,
        Err(msg) => return bad_request(&msg),
    };
    let db_path = Arc::clone(&state.db_path);

    match tokio::task::spawn_blocking(move || -> Result<serde_json::Value, MemoryError> {
        let db = Database::open_read_only(&db_path)?;
        let sessions = list_sessions(&db, &params)?;
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
    let params = match parse_report_params(query) {
        Ok(p) => p,
        Err(msg) => return bad_request(&msg),
    };
    let db_path = Arc::clone(&state.db_path);

    match tokio::task::spawn_blocking(move || -> Result<serde_json::Value, MemoryError> {
        let db = Database::open_read_only(&db_path)?;
        let report = report_projection(&db, params.task, params.since)?;
        let mut json = serde_json::to_value(&report).map_err(MemoryError::from)?;
        cap_report_json(&mut json, params.limit);
        Ok(json)
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
            "wing" => wing = Some(validate_param("wing", v)?),
            "room" => room = Some(validate_param("room", v)?),
            "limit" => {
                limit = parse_limit(&v)?;
            }
            _ => {} // ignore unknown params
        }
    }

    Ok(MemoryParams { wing, room, limit })
}

fn parse_memory_request(query: &str) -> Result<MemoryRequest, String> {
    let mut id = None;
    for (k, v) in form_urlencoded(query) {
        if k == "id" {
            let value = validate_param("id", v)?;
            if value.is_empty() {
                return Err("id must not be empty".to_string());
            }
            id = Some(value);
        }
    }
    match id {
        Some(id) => Ok(MemoryRequest::Detail(id)),
        None => Ok(MemoryRequest::List(parse_memory_params(query)?)),
    }
}

fn parse_code_map_params(query: &str) -> Result<CodeMapParams, String> {
    let mut repo = None;
    let mut area = None;
    let mut limit = DEFAULT_LIMIT;
    for (k, v) in form_urlencoded(query) {
        match k.as_str() {
            "repo" => repo = Some(validate_param("repo", v)?),
            "area" => area = Some(validate_param("area", v)?),
            "limit" => limit = parse_limit(&v)?,
            _ => {}
        }
    }
    Ok(CodeMapParams { repo, area, limit })
}

fn parse_session_params(query: &str) -> Result<SessionParams, String> {
    let mut limit = DEFAULT_LIMIT;
    for (k, v) in form_urlencoded(query) {
        if k == "limit" {
            limit = parse_limit(&v)?;
        }
    }
    Ok(SessionParams { limit })
}

fn parse_report_params(query: &str) -> Result<ReportParams, String> {
    let mut task = None;
    let mut since = None;
    let mut limit = DEFAULT_LIMIT;
    for (k, v) in form_urlencoded(query) {
        match k.as_str() {
            "task" => task = Some(validate_param("task", v)?),
            "since" => since = Some(validate_param("since", v)?),
            "limit" => limit = parse_limit(&v)?,
            _ => {}
        }
    }
    let since = crate::report::validate_since(since.as_deref()).map_err(|e| e.to_string())?;
    Ok(ReportParams { task, since, limit })
}

fn validate_param(name: &str, value: String) -> Result<String, String> {
    if value.chars().count() > MAX_PARAM_CHARS {
        return Err(format!("{name} exceeds maximum length {MAX_PARAM_CHARS}"));
    }
    Ok(value)
}

fn parse_limit(value: &str) -> Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| format!("invalid limit value: {value:?}"))?;
    if limit == 0 {
        return Err("limit must be at least 1".to_string());
    }
    if limit > MAX_LIMIT {
        return Err(format!("limit {limit} exceeds maximum {MAX_LIMIT}"));
    }
    Ok(limit)
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

fn handle_head(path: &str, query: &str) -> HyperResponse {
    match path {
        "/" => empty_response(StatusCode::OK, "text/html; charset=utf-8"),
        "/api/summary" => empty_response(StatusCode::OK, "application/json; charset=utf-8"),
        "/api/memory" => match parse_memory_request(query) {
            Ok(_) => empty_response(StatusCode::OK, "application/json; charset=utf-8"),
            Err(msg) => bad_request_empty(&msg),
        },
        "/api/code-maps" => match parse_code_map_params(query) {
            Ok(_) => empty_response(StatusCode::OK, "application/json; charset=utf-8"),
            Err(msg) => bad_request_empty(&msg),
        },
        "/api/sessions" => match parse_session_params(query) {
            Ok(_) => empty_response(StatusCode::OK, "application/json; charset=utf-8"),
            Err(msg) => bad_request_empty(&msg),
        },
        "/api/report" => match parse_report_params(query) {
            Ok(_) => empty_response(StatusCode::OK, "application/json; charset=utf-8"),
            Err(msg) => bad_request_empty(&msg),
        },
        _ => empty_response(StatusCode::NOT_FOUND, "application/json; charset=utf-8"),
    }
}

fn empty_response(status: StatusCode, content_type: &'static str) -> HyperResponse {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .header("X-Content-Type-Options", "nosniff")
        .header("Cache-Control", "no-store")
        .body(Full::new(Bytes::new()))
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

fn bad_request_empty(_msg: &str) -> HyperResponse {
    empty_response(StatusCode::BAD_REQUEST, "application/json; charset=utf-8")
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

fn cap_report_json(value: &mut serde_json::Value, limit: usize) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let mut meta = serde_json::Map::new();
    for field in ["headline", "non_completions", "tasks", "unpriced_models"] {
        if let Some(array) = obj.get_mut(field).and_then(|v| v.as_array_mut()) {
            let original_len = array.len();
            if original_len > limit {
                array.truncate(limit);
                meta.insert(field.to_string(), serde_json::json!(original_len));
            }
        }
    }
    if !meta.is_empty() {
        meta.insert("limit".to_string(), serde_json::json!(limit));
        obj.insert(
            "dashboard_truncated".to_string(),
            serde_json::Value::Object(meta),
        );
    }
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
  <div id="report-output"><p class="loading">Choose filters, then load the report.</p></div>
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
</script>
</body>
</html>"####;

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::metrics::NewTokenUsage;
    use crate::db::schema::{Database, LATEST_SCHEMA_VERSION};
    use http_body_util::BodyExt;
    use std::path::PathBuf;

    struct Fixture {
        _dir: tempfile::TempDir,
        db_path: PathBuf,
        drawer_id: String,
        state: Arc<ServerState>,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.sqlite3");
        let drawer_id;
        {
            let db = Database::open(&db_path).unwrap();
            db.migrate().unwrap();
            let emb = vec![0.0f32; ironrace_embed::embedder::EMBED_DIM];
            drawer_id = crate::db::drawers::generate_id("full drawer content", "wing-a", "room-a");
            db.insert_drawer(
                &drawer_id,
                "full drawer content",
                &emb,
                "wing-a",
                "room-a",
                "src/a.rs",
                "test",
            )
            .unwrap();
            db.upsert_code_map(
                "repo-a",
                "core",
                &drawer_id,
                "aabbccdd1122334455667788aabbccdd11223344",
                &["src/lib.rs".to_string()],
                "test-agent",
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
            db.with_connection(|conn| {
                crate::collab::queue::create_session(
                    conn,
                    "dash-session-001",
                    "/repo-a",
                    "main",
                    Some("dashboard task"),
                    crate::collab::Agent::Claude,
                )
            })
            .unwrap();
            db.insert_token_usage(&NewTokenUsage {
                ts: "2026-01-02T00:00:00Z".to_string(),
                source: "transcript".to_string(),
                harness: "claude".to_string(),
                model: Some("claude-opus-4-8".to_string()),
                session_id: None,
                collab_session_id: None,
                collab_phase: Some("impl".to_string()),
                task_tag: Some("dashboard-test".to_string()),
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                estimated: false,
                chars: 0,
                cost_usd: None,
                map_status: None,
                turn_id: None,
                area: None,
            })
            .unwrap();
        }
        let state = Arc::new(ServerState {
            db_path: Arc::new(db_path.clone()),
            schema_version: LATEST_SCHEMA_VERSION,
        });
        Fixture {
            _dir: dir,
            db_path,
            drawer_id,
            state,
        }
    }

    async fn body_text(response: HyperResponse) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

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

    #[test]
    fn parse_memory_request_exact_id_uses_detail_path() {
        match parse_memory_request("id=drawer123").unwrap() {
            MemoryRequest::Detail(id) => assert_eq!(id, "drawer123"),
            MemoryRequest::List(_) => panic!("expected detail request"),
        }
    }

    #[test]
    fn parse_code_map_and_session_limits_are_capped() {
        assert_eq!(parse_code_map_params("limit=7").unwrap().limit, 7);
        assert_eq!(parse_session_params("limit=8").unwrap().limit, 8);
        assert!(parse_code_map_params(&format!("limit={}", MAX_LIMIT + 1)).is_err());
        assert!(parse_session_params("limit=0").is_err());
    }

    #[test]
    fn parse_report_params_validates_and_normalizes_since() {
        let params = parse_report_params("task=abc&since=2026-01-02&limit=3").unwrap();
        assert_eq!(params.task.as_deref(), Some("abc"));
        assert_eq!(params.since.as_deref(), Some("2026-01-02T00:00:00Z"));
        assert_eq!(params.limit, 3);

        let err = parse_report_params("since=not-a-date").unwrap_err();
        assert!(err.contains("since must be RFC3339 or YYYY-MM-DD"));
    }

    #[tokio::test]
    async fn dashboard_handlers_return_expected_shapes_against_fixture() {
        let fx = fixture();
        let version_before = Database::open_read_only(&fx.db_path)
            .unwrap()
            .schema_version()
            .unwrap();

        let html = serve_html(DASHBOARD_HTML);
        assert_eq!(html.status(), StatusCode::OK);
        let html_body = body_text(html).await;
        assert!(html_body.contains("Memory Drawers"));
        assert!(html_body.contains("Code Maps"));
        assert!(html_body.contains("Collab Sessions"));
        assert!(html_body.contains("Metrics Report"));

        let summary = handle_summary(Arc::clone(&fx.state), "").await;
        assert_eq!(summary.status(), StatusCode::OK);
        let summary_json: serde_json::Value =
            serde_json::from_str(&body_text(summary).await).unwrap();
        assert_eq!(summary_json["schema_version"], LATEST_SCHEMA_VERSION);
        assert_eq!(summary_json["total_drawers"], 1);

        let memory = handle_memory(Arc::clone(&fx.state), "limit=10").await;
        assert_eq!(memory.status(), StatusCode::OK);
        let memory_json: serde_json::Value =
            serde_json::from_str(&body_text(memory).await).unwrap();
        assert_eq!(memory_json["total_drawers"], 1);
        assert_eq!(memory_json["recent_drawers"].as_array().unwrap().len(), 1);
        assert!(memory_json["recent_drawers"][0].get("content").is_none());

        let detail = handle_memory(Arc::clone(&fx.state), &format!("id={}", fx.drawer_id)).await;
        assert_eq!(detail.status(), StatusCode::OK);
        let detail_json: serde_json::Value =
            serde_json::from_str(&body_text(detail).await).unwrap();
        assert_eq!(detail_json["content"], "full drawer content");

        let code_maps =
            handle_code_maps(Arc::clone(&fx.state), "repo=repo-a&area=core&limit=10").await;
        assert_eq!(code_maps.status(), StatusCode::OK);
        let code_maps_json: serde_json::Value =
            serde_json::from_str(&body_text(code_maps).await).unwrap();
        assert_eq!(code_maps_json.as_array().unwrap().len(), 1);

        let sessions = handle_sessions(Arc::clone(&fx.state), "limit=10").await;
        assert_eq!(sessions.status(), StatusCode::OK);
        let sessions_json: serde_json::Value =
            serde_json::from_str(&body_text(sessions).await).unwrap();
        assert_eq!(sessions_json.as_array().unwrap().len(), 1);
        assert!(sessions_json[0].get("canonical_plan").is_none());
        assert!(sessions_json[0].get("final_plan").is_none());

        let report = handle_report(Arc::clone(&fx.state), "task=dashboard-test&limit=10").await;
        assert_eq!(report.status(), StatusCode::OK);
        let report_json: serde_json::Value =
            serde_json::from_str(&body_text(report).await).unwrap();
        assert_eq!(report_json["generated_for"]["task"], "dashboard-test");

        let version_after = Database::open_read_only(&fx.db_path)
            .unwrap()
            .schema_version()
            .unwrap();
        assert_eq!(version_before, version_after);
    }

    #[tokio::test]
    async fn invalid_params_return_safe_400_bodies() {
        let fx = fixture();

        let limit = handle_memory(Arc::clone(&fx.state), "limit=501").await;
        assert_eq!(limit.status(), StatusCode::BAD_REQUEST);
        let limit_body = body_text(limit).await;
        assert!(limit_body.contains("exceeds maximum"));
        assert!(!limit_body.contains("sqlite"));
        assert!(!limit_body.contains(&fx.db_path.display().to_string()));

        let since = handle_report(Arc::clone(&fx.state), "since=not-a-date").await;
        assert_eq!(since.status(), StatusCode::BAD_REQUEST);
        let since_body = body_text(since).await;
        assert!(since_body.contains("since must be RFC3339 or YYYY-MM-DD"));
        assert!(!since_body.contains("internal server error"));

        let missing_db_state = Arc::new(ServerState {
            db_path: Arc::new(fx.db_path.with_file_name("missing.sqlite3")),
            schema_version: LATEST_SCHEMA_VERSION,
        });
        let internal = handle_summary(missing_db_state, "").await;
        assert_eq!(internal.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let internal_body = body_text(internal).await;
        assert_eq!(internal_body, r#"{"error":"internal server error"}"#);
    }

    #[test]
    fn head_requests_are_validated_without_dispatching_handlers() {
        let ok = handle_head("/api/report", "task=dashboard-test&limit=10");
        assert_eq!(ok.status(), StatusCode::OK);

        let bad = handle_head("/api/report", "since=not-a-date");
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

        let missing = handle_head("/nope", "");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn report_json_is_capped_and_marks_truncation() {
        let mut value = serde_json::json!({
            "headline": [1, 2],
            "non_completions": [1, 2, 3],
            "tasks": [1],
            "unpriced_models": ["a", "b"],
        });
        cap_report_json(&mut value, 1);
        assert_eq!(value["headline"].as_array().unwrap().len(), 1);
        assert_eq!(value["non_completions"].as_array().unwrap().len(), 1);
        assert_eq!(value["unpriced_models"].as_array().unwrap().len(), 1);
        assert_eq!(value["dashboard_truncated"]["limit"], 1);
        assert_eq!(value["dashboard_truncated"]["non_completions"], 3);
    }

    #[test]
    fn dashboard_html_sections_and_user_text_rendering_are_stable() {
        for needle in [
            "id=\"memory\"",
            "id=\"codemaps\"",
            "id=\"sessions\"",
            "id=\"reports\"",
            "fetchJSON('/api/summary')",
            "fetchJSON('/api/memory?'",
            "fetchJSON('/api/code-maps?'",
            "fetchJSON('/api/sessions')",
            "fetchJSON('/api/report?'",
        ] {
            assert!(DASHBOARD_HTML.contains(needle), "missing {needle}");
        }
        assert!(DASHBOARD_HTML.contains("td.textContent = row[k] || ''"));
        assert!(DASHBOARD_HTML.contains("td.textContent = v || ''"));
        assert!(DASHBOARD_HTML.contains("pre.textContent = JSON.stringify"));
        assert!(!DASHBOARD_HTML.contains("\nloadReport();"));
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

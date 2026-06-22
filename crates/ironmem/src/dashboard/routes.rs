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
    MemoryParams, SessionParams, DEFAULT_LIMIT, MAX_DASHBOARD_LIMIT,
};
use crate::dashboard::server::ServerState;
use crate::db::schema::Database;
use crate::error::MemoryError;

/// Maximum value for the `limit` query parameter. Aliases the single source of
/// truth in [`crate::dashboard::data`] so the HTTP cap and the DB-layer clamp
/// can never drift apart.
const MAX_LIMIT: usize = MAX_DASHBOARD_LIMIT;
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
pub async fn handle_request<B>(
    req: Request<B>,
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

    // `None` here means the requested drawer id was not found — the absence is
    // carried in the type system so the 404 decision is not coupled to an error
    // string baked into the JSON body.
    match tokio::task::spawn_blocking(move || -> Result<Option<serde_json::Value>, MemoryError> {
        let db = Database::open_read_only(&db_path)?;
        match request {
            MemoryRequest::List(params) => {
                let summary = memory_summary(&db, &params)?;
                serde_json::to_value(&summary)
                    .map(Some)
                    .map_err(MemoryError::from)
            }
            MemoryRequest::Detail(id) => match drawer_detail(&db, &id)? {
                Some(drawer) => serde_json::to_value(&drawer)
                    .map(Some)
                    .map_err(MemoryError::from),
                None => Ok(None),
            },
        }
    })
    .await
    {
        Ok(Ok(Some(json))) => json_response(StatusCode::OK, &json),
        Ok(Ok(None)) => not_found(),
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
    // Replace '+' with space, then percent-decode into raw bytes and interpret
    // the result as UTF-8. Accumulating bytes (rather than pushing each decoded
    // byte as a `char`) is required so multi-byte UTF-8 sequences like
    // `%E2%9C%93` decode to their real code point instead of Latin-1 garbage.
    let with_spaces = s.replace('+', " ");
    let mut out: Vec<u8> = Vec::with_capacity(with_spaces.len());
    let mut chars = with_spaces.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next().and_then(|c| c.to_digit(16));
            let h2 = chars.next().and_then(|c| c.to_digit(16));
            if let (Some(h1), Some(h2)) = (h1, h2) {
                out.push(((h1 << 4) | h2) as u8);
            } else {
                // Malformed/short escape: keep the literal '%' rather than
                // silently dropping it (any consumed hex chars stay consumed).
                out.push(b'%');
            }
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ────────────────────────────────────────────────────────────────────────────
// Response helpers
// ────────────────────────────────────────────────────────────────────────────

/// Single construction point for every response (success and error). Always
/// sets the baseline security headers (`X-Content-Type-Options: nosniff`,
/// `Cache-Control: no-store`) and `Content-Type` so no response — including
/// error bodies — can drift out of the security envelope. On a builder error
/// it logs and falls back to a safe 500.
fn build_response(status: StatusCode, content_type: &'static str, body: Bytes) -> HyperResponse {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .header("X-Content-Type-Options", "nosniff")
        .header("Cache-Control", "no-store")
        .body(Full::new(body))
        .unwrap_or_else(|e| {
            tracing::error!("dashboard response build failed: {e}");
            internal_fallback()
        })
}

fn json_response(status: StatusCode, value: &serde_json::Value) -> HyperResponse {
    let body = serde_json::to_string(value)
        .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string());
    build_response(status, "application/json; charset=utf-8", Bytes::from(body))
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
    build_response(status, content_type, Bytes::new())
}

fn serve_html(html: &'static str) -> HyperResponse {
    // HTML root carries the extra framing/CSP hardening on top of the baseline
    // headers applied by `build_response`.
    let base = build_response(
        StatusCode::OK,
        "text/html; charset=utf-8",
        Bytes::from_static(html.as_bytes()),
    );
    let (mut parts, body) = base.into_parts();
    parts.headers.insert(
        hyper::header::HeaderName::from_static("x-frame-options"),
        hyper::header::HeaderValue::from_static("DENY"),
    );
    parts.headers.insert(
        hyper::header::HeaderName::from_static("content-security-policy"),
        hyper::header::HeaderValue::from_static(
            "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'",
        ),
    );
    Response::from_parts(parts, body)
}

fn not_found() -> HyperResponse {
    build_response(
        StatusCode::NOT_FOUND,
        "application/json; charset=utf-8",
        Bytes::from_static(br#"{"error":"not found"}"#),
    )
}

fn method_not_allowed() -> HyperResponse {
    let mut response = build_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "application/json; charset=utf-8",
        Bytes::from_static(br#"{"error":"method not allowed"}"#),
    );
    response
        .headers_mut()
        .insert(ALLOW, hyper::header::HeaderValue::from_static("GET, HEAD"));
    response
}

fn bad_request(msg: &str) -> HyperResponse {
    // Sanitize: drop any raw error that could carry DB internals; msg here
    // comes from our own parameter validators, not from rusqlite.
    let body = serde_json::json!({ "error": msg }).to_string();
    build_response(
        StatusCode::BAD_REQUEST,
        "application/json; charset=utf-8",
        Bytes::from(body),
    )
}

fn bad_request_empty(_msg: &str) -> HyperResponse {
    empty_response(StatusCode::BAD_REQUEST, "application/json; charset=utf-8")
}

/// Produce a safe 500 response that never leaks raw SQLite or filesystem paths.
fn internal_error_safe(e: &MemoryError) -> HyperResponse {
    // Log the real error server-side; surface only a safe generic message.
    tracing::error!(
        error_id = "DASHBOARD_INTERNAL",
        "dashboard internal error: {e}"
    );
    build_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "application/json; charset=utf-8",
        Bytes::from_static(br#"{"error":"internal server error"}"#),
    )
}

/// Last-resort response used only when a `Response::builder()` call itself
/// fails. Returns an honest `500` with an explicit JSON content type rather
/// than the default `200 OK`.
fn internal_fallback() -> HyperResponse {
    let mut response = Response::new(Full::new(Bytes::from_static(
        br#"{"error":"internal server error"}"#,
    )));
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response.headers_mut().insert(
        CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
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

/// Single-page dashboard HTML, pulled in from `index.html` at compile time so
/// the asset lives in one editable file instead of inline in this module.
/// User-controlled content is always set via `textContent` (never `innerHTML`)
/// to prevent XSS.
const DASHBOARD_HTML: &str = include_str!("index.html");

// ────────────────────────────────────────────────────────────────────────────
// Tests (kept in a sibling file to hold this module under the 800-line cap)
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "routes_tests.rs"]
mod routes_tests;

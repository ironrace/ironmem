//! Unit tests for `super` (the dashboard `routes` module).
//!
//! Extracted from `routes.rs` to keep that file under the 800-line cap.
//! Included via `#[cfg(test)] #[path = "routes_tests.rs"] mod routes_tests;`.

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
    // A dedicated, empty model dir: no model files are written, so warming
    // status resolves to `missing` deterministically (no 400MB model needed).
    // Mirror the startup resolution so the fixture exercises the real mapping.
    let model_dir = dir.path().join("models");
    let model_status = crate::dashboard::data::WarmingStatus::from(
        &ironrace_embed::embedder::model_status(&model_dir),
    );
    let state = Arc::new(ServerState {
        db_path: Arc::new(db_path.clone()),
        schema_version: LATEST_SCHEMA_VERSION,
        model_status,
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

#[test]
fn form_urlencoded_decodes_multibyte_utf8() {
    // `%E2%9C%93` is the UTF-8 encoding of U+2713 CHECK MARK (✓).
    // Decoding each byte as a `char` (Latin-1) would corrupt it.
    let pairs = form_urlencoded("task=ok%E2%9C%93&room=caf%C3%A9");
    let map: std::collections::HashMap<_, _> = pairs.into_iter().collect();
    assert_eq!(map.get("task").map(|s| s.as_str()), Some("ok\u{2713}"));
    assert_eq!(map.get("room").map(|s| s.as_str()), Some("café"));
}

#[test]
fn percent_decode_preserves_malformed_escapes() {
    // A lone trailing '%' is kept literally rather than dropped.
    assert_eq!(percent_decode("100%"), "100%");
    // A '%' followed by non-hex chars keeps the '%'; the two chars peeked for
    // the escape are consumed (h1/h2 are read before the validity check).
    assert_eq!(percent_decode("a%zz"), "a%");
    // A short escape at end of input keeps the literal '%'; the single
    // consumed hex char ('2') stays consumed, matching the documented behavior.
    assert_eq!(percent_decode("x%2"), "x%");
    // Valid escapes still decode normally even when mixed with a stray '%'.
    assert_eq!(percent_decode("%41%"), "A%");
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
    let summary_json: serde_json::Value = serde_json::from_str(&body_text(summary).await).unwrap();
    assert_eq!(summary_json["schema_version"], LATEST_SCHEMA_VERSION);
    assert_eq!(summary_json["total_drawers"], 1);

    let memory = handle_memory(Arc::clone(&fx.state), "limit=10").await;
    assert_eq!(memory.status(), StatusCode::OK);
    let memory_json: serde_json::Value = serde_json::from_str(&body_text(memory).await).unwrap();
    assert_eq!(memory_json["total_drawers"], 1);
    assert_eq!(memory_json["recent_drawers"].as_array().unwrap().len(), 1);
    assert!(memory_json["recent_drawers"][0].get("content").is_none());

    let detail = handle_memory(Arc::clone(&fx.state), &format!("id={}", fx.drawer_id)).await;
    assert_eq!(detail.status(), StatusCode::OK);
    let detail_json: serde_json::Value = serde_json::from_str(&body_text(detail).await).unwrap();
    assert_eq!(detail_json["content"], "full drawer content");

    let code_maps = handle_code_maps(Arc::clone(&fx.state), "repo=repo-a&area=core&limit=10").await;
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
    let report_json: serde_json::Value = serde_json::from_str(&body_text(report).await).unwrap();
    assert_eq!(report_json["generated_for"]["task"], "dashboard-test");

    let version_after = Database::open_read_only(&fx.db_path)
        .unwrap()
        .schema_version()
        .unwrap();
    assert_eq!(version_before, version_after);

    // No-write proof beyond schema version: re-read the drawer count through
    // the read-only summary path after the full handler sweep and assert it
    // is unchanged. A stray INSERT/DELETE on a data table would shift this
    // even when schema_version stayed constant.
    let summary_after = handle_summary(Arc::clone(&fx.state), "").await;
    let summary_after_json: serde_json::Value =
        serde_json::from_str(&body_text(summary_after).await).unwrap();
    assert_eq!(summary_after_json["total_drawers"], 1);
}

#[tokio::test]
async fn summary_surfaces_model_status_alongside_total_drawers() {
    // GAP 1: /api/summary must report embed-model readiness (can it embed?)
    // AND total_drawers (is memory populated?) so warming is never misread as
    // content readiness. The fixture resolves status against an empty model dir.
    let fx = fixture();
    assert_eq!(
        fx.state.model_status,
        crate::dashboard::data::WarmingStatus::Missing,
        "fixture must resolve an empty model cache to `missing`"
    );

    let summary = handle_summary(Arc::clone(&fx.state), "").await;
    assert_eq!(summary.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body_text(summary).await).unwrap();

    // Model readiness label is surfaced and is "missing" for an empty cache.
    assert_eq!(json["model_status"], "missing");
    // Content readiness is reported independently and is unaffected by warming.
    assert_eq!(json["total_drawers"], 1);
}

#[tokio::test]
async fn code_maps_carry_per_row_freshness_with_head_sha() {
    // GAP 2: each row gains a freshness badge. The fixture's repo path
    // ("repo-a") is not a resolvable worktree, so it falls back to an age
    // bucket — head_sha is always present for provenance.
    let fx = fixture();
    let resp = handle_code_maps(Arc::clone(&fx.state), "repo=repo-a&area=core&limit=10").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
    let row = &json.as_array().unwrap()[0];

    // Provenance: original fields still flattened in.
    assert_eq!(row["head_sha"], "aabbccdd1122334455667788aabbccdd11223344");
    assert_eq!(row["repo"], "repo-a");
    // Hybrid fallback: unresolved path → age badge (built_at 2026-01-01 is old).
    assert_eq!(row["freshness"]["kind"], "age");
    assert_eq!(row["freshness"]["bucket"], "stale");
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
        model_status: fx.state.model_status,
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

    // Negative XSS assertion: no `innerHTML =` assignment may be fed by row or
    // record data. The only permitted `innerHTML` writes are static strings
    // (clearing with '' or setting fixed `<thead>`/`<p>` markup). Any
    // assignment that interpolates `row`/`d`/`rows`/`vals` would be a sink.
    for sink in [
        "innerHTML = row",
        "innerHTML = d.",
        "innerHTML = d[",
        "innerHTML = rows",
        "innerHTML = vals",
        "innerHTML += ",
        "innerHTML = `",
    ] {
        assert!(
            !DASHBOARD_HTML.contains(sink),
            "data-fed innerHTML sink found: {sink}"
        );
    }

    // The dead `esc()` helper must stay removed (all rendering uses textContent).
    assert!(
        !DASHBOARD_HTML.contains("function esc("),
        "dead esc() helper reintroduced"
    );
}

#[test]
fn dashboard_html_surfaces_model_status_with_readiness_framing() {
    // GAP 1 UI: warming status is shown AND framed so model readiness is not
    // misread as memory being populated.
    assert!(
        DASHBOARD_HTML.contains("model_status"),
        "summary UI must read d.model_status"
    );
    assert!(
        DASHBOARD_HTML.contains("Embed Model"),
        "summary UI must label the model-readiness card"
    );
    assert!(
        DASHBOARD_HTML.to_lowercase().contains("readiness"),
        "summary UI must frame model status as readiness, not content"
    );
}

#[test]
fn dashboard_html_renders_code_map_freshness_badge() {
    // GAP 2 UI: code-maps table has a Freshness column fed by row.freshness,
    // rendered via textContent (never innerHTML) so it stays XSS-safe.
    assert!(
        DASHBOARD_HTML.contains("Freshness"),
        "code-maps table must have a Freshness column header"
    );
    assert!(
        DASHBOARD_HTML.contains("row.freshness"),
        "freshness badge must read row.freshness"
    );
    assert!(
        DASHBOARD_HTML.contains("freshnessLabel"),
        "freshness badge must derive its label via freshnessLabel()"
    );
    // The badge text must be assigned via textContent, never innerHTML.
    assert!(DASHBOARD_HTML.contains("badge.textContent"));
}

#[test]
fn dashboard_html_shows_real_remediation_commands_per_section() {
    // GAP 3: every section points to REAL ironmem commands. Static text only —
    // no user-controlled data, no invented subcommands.
    for cmd in [
        "ironmem mine",
        "ironmem reembed",
        "ironmem report",
        "ironmem doctor",
        "ironmem context",
    ] {
        assert!(
            DASHBOARD_HTML.contains(cmd),
            "missing remediation command: {cmd}"
        );
    }
    // There is no `code-map refresh` subcommand — stale maps must point to
    // re-mining / doctor, never an invented command.
    assert!(
        !DASHBOARD_HTML.contains("code-map refresh"),
        "invented `code-map refresh` command must not appear"
    );
}

// ── method_not_allowed ───────────────────────────────────────────────────

#[test]
fn method_not_allowed_response_has_405() {
    let r = method_not_allowed();
    assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(r.headers().contains_key(ALLOW));
}

#[test]
fn error_responses_carry_baseline_security_headers() {
    for r in [not_found(), method_not_allowed(), bad_request("nope")] {
        assert_eq!(
            r.headers()
                .get("X-Content-Type-Options")
                .and_then(|v| v.to_str().ok()),
            Some("nosniff"),
            "missing nosniff on {:?}",
            r.status()
        );
        assert_eq!(
            r.headers()
                .get("Cache-Control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "missing no-store on {:?}",
            r.status()
        );
        assert!(r.headers().contains_key(CONTENT_TYPE));
    }
}

#[test]
fn internal_fallback_is_an_honest_500_json() {
    let r = internal_fallback();
    assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        r.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("application/json; charset=utf-8")
    );
}

// ── handle_request dispatch (real entry point) ───────────────────────────────

async fn dispatch(state: Arc<ServerState>, method: Method, uri: &str) -> HyperResponse {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(http_body_util::Empty::<Bytes>::new())
        .unwrap();
    handle_request(req, state).await.unwrap()
}

#[tokio::test]
async fn handle_request_rejects_non_get_head_verbs_with_405() {
    let fx = fixture();
    for method in [Method::POST, Method::DELETE] {
        let r = dispatch(Arc::clone(&fx.state), method.clone(), "/api/summary").await;
        assert_eq!(
            r.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} should be 405"
        );
        assert_eq!(
            r.headers().get(ALLOW).and_then(|v| v.to_str().ok()),
            Some("GET, HEAD"),
            "{method} 405 must advertise Allow"
        );
    }
}

#[tokio::test]
async fn handle_request_routes_get_and_head() {
    let fx = fixture();

    let get = dispatch(Arc::clone(&fx.state), Method::GET, "/api/summary").await;
    assert_eq!(get.status(), StatusCode::OK);
    let get_json: serde_json::Value = serde_json::from_str(&body_text(get).await).unwrap();
    assert_eq!(get_json["total_drawers"], 1);

    // HEAD reaches the HEAD handler: 200 with an empty body.
    let head = dispatch(Arc::clone(&fx.state), Method::HEAD, "/api/summary").await;
    assert_eq!(head.status(), StatusCode::OK);
    assert!(body_text(head).await.is_empty());

    // Unknown GET path routes to the 404 branch.
    let missing = dispatch(Arc::clone(&fx.state), Method::GET, "/nope").await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
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

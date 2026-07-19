//! Task 1 (RED test, root cause of `daemon_autospawn_race.rs` flakiness):
//! write-shaped MCP tool handlers silently no-op during warm-up.
//!
//! `handle_add_drawer`, `handle_diary_write`, and `handle_code_map_write` each
//! begin with `if app.is_warming_up() { return Ok(json!({"warming_up": true,
//! ...})) }` — a success-shaped response that performs NO write. At the wire
//! level, `mcp::server::dispatch` renders any `Ok(_)` from `tools::call_tool`
//! as an ordinary JSON-RPC success with no `isError` — so this warm-up no-op
//! is indistinguishable from a genuine successful write to a caller. That is
//! the root cause of 5 concurrent `add_drawer` callers over a shared,
//! still-warming daemon landing only 4/5 rows.
//!
//! `handle_search` also checks `is_warming_up()`, but it is READ-shaped and
//! its soft `{"warming_up": true, "results": []}` body is correct, existing
//! behavior that must NOT change.
//!
//! This test drives each tool through the real wire-level `dispatch` entry
//! point (not the handler function directly) against a single in-process
//! `App::open_for_test()` forced into the warm-up state.
//!
//! History: this file started (Task 1) as a RED test — on HEAD at that point,
//! `add_drawer`/`diary_write`/`code_map_write` returned the soft
//! `warming_up` body as if it were a success, and this file failed on all
//! three while `search`'s companion case passed. Task 5 fixed the handlers to
//! call `app.wait_for_write_ready()` instead of no-opping, turning those
//! three RED cases GREEN.
//!
//! Task 6 (this file, extended): with the handlers now blocking on
//! `wait_for_write_ready()`, the original three tests became the **timeout**
//! terminal-path coverage — the readiness gate is forced `Pending` and a
//! short injected timeout proves each write tool errors out bounded rather
//! than hanging. This file additionally covers the **failed** terminal path:
//! the gate resolved explicitly to `Failed(reason)` rather than timing out.
//! In both terminal paths a completed write is structurally impossible (the
//! gate never becomes `Ready`), so the assertion for all six of these tests
//! is tightened to require `isError == true` outright (`assert_write_errored`),
//! not the looser either/or allowance that would still make sense for a
//! hypothetical fast-resolving-to-Ready race. `search` (read-shaped) keeps
//! its soft `{"warming_up": true, "results": []}` body unchanged throughout —
//! that behavior is correct and must not change.

use std::sync::{Arc, Mutex};

use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::readiness::ReadinessGate;
use ironmem::mcp::server::dispatch;
use serde_json::{json, Value};

/// Serializes tests that override `IRONMEM_WRITE_READINESS_TIMEOUT_SECS`: env
/// vars are process-global and would otherwise race under the parallel test
/// runner within this binary (mirrors `crates/ironmem/src/config.rs`'s own
/// `ENV_LOCK` pattern for the same class of hazard).
static WRITE_READINESS_TIMEOUT_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Runs `f` with `IRONMEM_WRITE_READINESS_TIMEOUT_SECS` set to a short
/// test-only override, restoring the env afterward.
///
/// Why this is needed: `force_warming_up` swaps in a *fresh* `Pending` gate
/// that this test never resolves (that's the point — it reproduces the
/// daemon's warm-up window without a real background init thread). Once the
/// write handlers block on `app.wait_for_write_ready()` instead of no-op'ing,
/// a still-pending gate means the call will genuinely block for the full
/// configured timeout before giving up with `Err(NotReady)`. Left at its
/// production default (tens of seconds, generous enough for a real model
/// load), each of the three write-tool tests below would take that long,
/// multiplying into a slow suite. `Config::write_readiness_timeout()` reads
/// its env var fresh on every call (not cached on `Config`), so overriding it
/// here — immediately before driving the request — takes effect without
/// needing to reconstruct `App`/`Config`. `assert_write_errored` requires
/// `isError: true` outright here, since a still-`Pending` gate that only
/// ever times out can never let the write complete — proving the
/// bounded-timeout-then-error path is a faithful exercise of the real
/// contract, not a weakening of it.
///
/// Panic-safe: `f`'s body is an assertion (`assert_write_errored`) that can
/// panic on failure. `catch_unwind` ensures the env var is always
/// removed — via the lock guard and unconditionally, not just on the happy
/// path — before the original panic (if any) is resumed, so a failing
/// assertion here can never leak the override into a later test.
fn with_short_write_readiness_timeout<F: FnOnce() + std::panic::UnwindSafe>(f: F) {
    let _guard = WRITE_READINESS_TIMEOUT_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IRONMEM_WRITE_READINESS_TIMEOUT_SECS", "1");
    let result = std::panic::catch_unwind(f);
    std::env::remove_var("IRONMEM_WRITE_READINESS_TIMEOUT_SECS");
    if let Err(panic_payload) = result {
        std::panic::resume_unwind(panic_payload);
    }
}

fn request(name: &str, arguments: Value) -> JsonRpcRequest {
    serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    }))
    .expect("request fixture must deserialize")
}

/// Drive a tool call through the real wire-level `dispatch` entrypoint and
/// return `(is_error, payload)`, where `payload` is the tool's inner JSON
/// body decoded from `result.content[0].text` — the same path a real MCP
/// client observes.
fn call_tool_raw(app: &App, name: &str, args: Value) -> (bool, Value) {
    let req = request(name, args);
    let resp = dispatch(app, &req).expect("tools/call must return a response");
    assert!(
        resp.error.is_none(),
        "unexpected RPC-level error for tool {name}"
    );
    let result = resp.result.expect("tool result must be present");
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let text = result["content"][0]["text"]
        .as_str()
        .expect("content[0].text must be a string");
    let payload: Value = serde_json::from_str(text).expect("tool response text must be valid JSON");
    (is_error, payload)
}

/// Force the `App` into the warm-up state that `is_warming_up()` reads.
/// `App::open_for_test()` starts with `memory_ready` already resolved
/// `Ready` (see `ReadinessGate::new_ready`). `ReadinessGate` has no raw
/// setter to flip a resolved gate back to pending (deliberately — first
/// resolution wins, by design), so this swaps in a *fresh* `Pending` gate
/// in place of the resolved one, reproducing the daemon's real warm-up
/// window without needing a real background init thread.
fn force_warming_up(app: &mut App) {
    app.memory_ready = Arc::new(ReadinessGate::new_pending());
    assert!(app.is_warming_up(), "failed to force warm-up state");
}

/// Force the `App`'s readiness gate into a terminal `Failed` state (e.g. a
/// background model load that errored out), rather than leaving it
/// `Pending`. Same swap-in pattern as `force_warming_up`, but the fresh gate
/// is resolved failed *before* being installed so `wait_for_write_ready`
/// observes the fast, already-terminal path (see
/// `ReadinessGate::peek_terminal`) instead of blocking at all.
fn force_readiness_failed(app: &mut App, reason: &str) {
    let gate = ReadinessGate::new_pending();
    gate.resolve_failed(reason.to_string());
    app.memory_ready = Arc::new(gate);
    assert!(
        app.is_warming_up(),
        "a Failed gate must still report is_warming_up() (is_ready() stays false on Failed)"
    );
}

/// Mirrors `code_maps.rs`'s own test helper: a minimal real git repo so
/// `code_map_write`'s `repo`/`head_sha` validation (which shells out to git)
/// can succeed.
fn make_git_repo_with_file(
    filename: &str,
    contents: &str,
) -> (tempfile::TempDir, std::path::PathBuf, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&root)
        .output()
        .unwrap();

    let file_path = root.join(filename);
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&file_path, contents).unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(&root)
        .output()
        .unwrap();

    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&root)
        .output()
        .unwrap();
    let sha = String::from_utf8(out.stdout).unwrap().trim().to_string();

    (dir, root, sha)
}

/// The Task 6 assertion shared by all six write-tool terminal-path tests
/// below (three timeout cases, three failed-resolution cases): in both
/// scenarios the readiness gate can never resolve `Ready` (it either times
/// out still `Pending`, or resolves explicitly `Failed`), so a completed
/// write is structurally impossible here — this requires `isError: true`
/// outright, and additionally rejects the soft warm-up no-op body even when
/// paired with `isError: true`.
fn assert_write_errored(tool: &str, is_error: bool, payload: &Value) {
    assert!(
        is_error,
        "{tool}: expected isError:true (readiness gate can never resolve Ready in \
         this scenario, so a completed write is impossible) but got isError=false, \
         payload={payload}"
    );
    let warming_up_body = payload.get("warming_up").and_then(Value::as_bool) == Some(true);
    assert!(
        !warming_up_body,
        "{tool}: must not return the soft warm-up no-op body even when paired with \
         isError:true in this scenario — payload={payload}"
    );
}

/// Task 6, timeout case: gate resolved `Pending` and never resolves within
/// the short injected `IRONMEM_WRITE_READINESS_TIMEOUT_SECS` override. This
/// can never produce a completed write (the gate stays `Pending` for the
/// full call), so the assertion is tightened to `isError == true` outright
/// rather than the looser `assert_write_completed_or_errored` allowance.
#[test]
fn add_drawer_does_not_silently_noop_during_warmup() {
    with_short_write_readiness_timeout(|| {
        let mut app = App::open_for_test().unwrap();
        force_warming_up(&mut app);

        let (is_error, payload) = call_tool_raw(
            &app,
            "add_drawer",
            json!({ "content": "warmup race test content", "wing": "ironrace-memory" }),
        );

        assert_write_errored("add_drawer", is_error, &payload);
    });
}

/// Task 6, timeout case (see doc comment above `add_drawer`'s counterpart).
#[test]
fn diary_write_does_not_silently_noop_during_warmup() {
    with_short_write_readiness_timeout(|| {
        let mut app = App::open_for_test().unwrap();
        force_warming_up(&mut app);

        let (is_error, payload) = call_tool_raw(
            &app,
            "diary_write",
            json!({ "content": "warmup race diary entry" }),
        );

        assert_write_errored("diary_write", is_error, &payload);
    });
}

/// Task 6, timeout case (see doc comment above `add_drawer`'s counterpart).
#[test]
fn code_map_write_does_not_silently_noop_during_warmup() {
    with_short_write_readiness_timeout(|| {
        let mut app = App::open_for_test().unwrap();
        force_warming_up(&mut app);

        let (_dir, root, sha) = make_git_repo_with_file("src/lib.rs", "// lib");

        let (is_error, payload) = call_tool_raw(
            &app,
            "code_map_write",
            json!({
                "repo": root.to_string_lossy(),
                "area": "core",
                "summary": "warmup race code map summary",
                "head_sha": sha,
                "source_files": ["src/lib.rs"],
                "built_by": "test",
            }),
        );

        assert_write_errored("code_map_write", is_error, &payload);
    });
}

/// Task 6, failed-resolution case: the readiness gate resolves explicitly
/// to `Failed` (e.g. background model init errored out) rather than timing
/// out still `Pending`. No timeout override is needed here — a `Failed`
/// gate is already terminal, so `wait_for_write_ready` returns immediately
/// via the fast path (`ReadinessGate::peek_terminal`).
#[test]
fn add_drawer_errors_when_readiness_resolves_failed() {
    let mut app = App::open_for_test().unwrap();
    force_readiness_failed(&mut app, "model load exploded");

    let (is_error, payload) = call_tool_raw(
        &app,
        "add_drawer",
        json!({ "content": "warmup race test content", "wing": "ironrace-memory" }),
    );

    assert_write_errored("add_drawer", is_error, &payload);
}

/// Task 6, failed-resolution case (see doc comment above `add_drawer`'s
/// counterpart).
#[test]
fn diary_write_errors_when_readiness_resolves_failed() {
    let mut app = App::open_for_test().unwrap();
    force_readiness_failed(&mut app, "model load exploded");

    let (is_error, payload) = call_tool_raw(
        &app,
        "diary_write",
        json!({ "content": "warmup race diary entry" }),
    );

    assert_write_errored("diary_write", is_error, &payload);
}

/// Task 6, failed-resolution case (see doc comment above `add_drawer`'s
/// counterpart).
#[test]
fn code_map_write_errors_when_readiness_resolves_failed() {
    let mut app = App::open_for_test().unwrap();
    force_readiness_failed(&mut app, "model load exploded");

    let (_dir, root, sha) = make_git_repo_with_file("src/lib.rs", "// lib");

    let (is_error, payload) = call_tool_raw(
        &app,
        "code_map_write",
        json!({
            "repo": root.to_string_lossy(),
            "area": "core",
            "summary": "warmup race code map summary",
            "head_sha": sha,
            "source_files": ["src/lib.rs"],
            "built_by": "test",
        }),
    );

    assert_write_errored("code_map_write", is_error, &payload);
}

/// Argument validation does not depend on readiness, so it must run BEFORE
/// the readiness wait: a malformed write has to be rejected immediately
/// rather than serving out the whole `IRONMEM_WRITE_READINESS_TIMEOUT_SECS`
/// window first. The override here is 1s (see
/// `with_short_write_readiness_timeout`); the assertion allows generously
/// less than that, so it fails if the handler waits at all.
#[test]
fn invalid_write_is_rejected_without_waiting_for_readiness() {
    with_short_write_readiness_timeout(|| {
        let mut app = App::open_for_test().unwrap();
        force_warming_up(&mut app);

        let start = std::time::Instant::now();
        // `content` is required by `add_drawer` and is absent here.
        let (is_error, payload) = call_tool_raw(&app, "add_drawer", json!({ "wing": "race" }));
        let elapsed = start.elapsed();

        assert!(is_error, "an invalid write must be rejected: {payload}");
        let message = payload["error"].as_str().unwrap_or_default();
        assert!(
            message.contains("content is required"),
            "expected the validation error, got: {payload}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "invalid input must fail fast, but the call took {elapsed:?} — it waited on \
             the readiness gate before validating"
        );
    });
}

/// Companion assertion: `search` is READ-shaped, and its soft
/// `{"warming_up": true, "results": []}` body during warm-up is CORRECT
/// existing behavior — it must keep passing, unlike the three write tools
/// above.
#[test]
fn search_is_allowed_to_return_soft_warmup_body() {
    let mut app = App::open_for_test().unwrap();
    force_warming_up(&mut app);

    let (is_error, payload) = call_tool_raw(&app, "search", json!({ "query": "anything" }));

    assert!(!is_error, "search must not error during warm-up");
    assert_eq!(
        payload["warming_up"].as_bool(),
        Some(true),
        "search's soft warm-up body must be preserved: {payload}"
    );
    assert_eq!(
        payload["results"].as_array().map(Vec::len),
        Some(0),
        "search's warm-up body must still report empty results: {payload}"
    );
}

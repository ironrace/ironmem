//! End-to-end integration tests for workspace mining.
//!
//! Uses an in-memory App (noop embedder) + real temp directories to verify
//! the manifest lifecycle, file discovery, chunking, and DB state.

use ironmem::ingest::mine_directory;
use ironmem::mcp::app::App;
use ironrace_embed::Embedder;
use tempfile::TempDir;

fn setup() -> (App, TempDir) {
    let app = App::open_for_test().unwrap();
    let dir = tempfile::tempdir().unwrap();
    (app, dir)
}

#[test]
fn mine_directory_ingests_files() {
    let (app, dir) = setup();

    std::fs::write(dir.path().join("notes.md"), "# Notes\n\nSome content here.").unwrap();
    std::fs::write(
        dir.path().join("README.md"),
        "# Project\n\nThis is a test project.",
    )
    .unwrap();

    mine_directory(&app, dir.path().to_str().unwrap()).unwrap();

    let count = app.db.count_drawers(None).unwrap();
    assert!(count > 0, "mining should insert drawers into the DB");
}

#[test]
fn mine_is_idempotent() {
    let (app, dir) = setup();

    std::fs::write(
        dir.path().join("data.md"),
        "# Data\n\nContent that should not be duplicated.",
    )
    .unwrap();

    mine_directory(&app, dir.path().to_str().unwrap()).unwrap();
    let count_first = app.db.count_drawers(None).unwrap();
    assert!(count_first > 0);

    mine_directory(&app, dir.path().to_str().unwrap()).unwrap();
    let count_second = app.db.count_drawers(None).unwrap();

    assert_eq!(
        count_first, count_second,
        "re-mining unchanged files must not create duplicate drawers"
    );
}

#[test]
fn mine_detects_changed_files() {
    let (app, dir) = setup();
    let file = dir.path().join("evolving.md");

    std::fs::write(&file, "# Version 1\n\nOriginal content.").unwrap();
    mine_directory(&app, dir.path().to_str().unwrap()).unwrap();
    let count_v1 = app.db.count_drawers(None).unwrap();
    assert!(count_v1 > 0);

    // Overwrite with different content
    std::fs::write(
        &file,
        "# Version 2\n\nCompletely different content that changed.",
    )
    .unwrap();
    mine_directory(&app, dir.path().to_str().unwrap()).unwrap();
    let count_v2 = app.db.count_drawers(None).unwrap();

    // Old drawers deleted, new ones inserted — count should equal a fresh mine
    assert_eq!(
        count_v2, count_v1,
        "changed file should replace its drawers, not accumulate them"
    );
}

#[test]
fn mine_removes_deleted_files() {
    let (app, dir) = setup();
    let persistent = dir.path().join("keep.md");
    let removable = dir.path().join("remove.md");

    std::fs::write(&persistent, "# Keep\n\nThis file stays.").unwrap();
    std::fs::write(&removable, "# Remove\n\nThis file will be deleted.").unwrap();

    mine_directory(&app, dir.path().to_str().unwrap()).unwrap();
    let count_before = app.db.count_drawers(None).unwrap();
    assert!(count_before >= 2, "both files should be indexed");

    std::fs::remove_file(&removable).unwrap();
    mine_directory(&app, dir.path().to_str().unwrap()).unwrap();
    let count_after = app.db.count_drawers(None).unwrap();

    assert!(
        count_after < count_before,
        "deleted file's drawers should be removed on next mine"
    );
}

#[test]
fn mine_failure_does_not_delete_existing_drawers_for_changed_file() {
    let (app, dir) = setup();
    let file = dir.path().join("volatile.md");

    std::fs::write(&file, "# Version 1\n\nStable content.").unwrap();
    mine_directory(&app, dir.path().to_str().unwrap()).unwrap();
    let count_before = app.db.count_drawers(None).unwrap();
    assert!(count_before > 0);

    std::fs::write(&file, "# Version 2\n\nThis update should fail to embed.").unwrap();
    *app.embedder.write().unwrap() = Embedder::new_failing_for_test("forced embed failure");

    let err = mine_directory(&app, dir.path().to_str().unwrap()).unwrap_err();
    assert!(
        err.to_string().contains("forced embed failure"),
        "expected injected embed failure, got: {err}"
    );

    let count_after = app.db.count_drawers(None).unwrap();
    assert_eq!(
        count_after, count_before,
        "existing drawers must remain if re-mining a changed file fails"
    );
}

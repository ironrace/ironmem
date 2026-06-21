use std::path::Path;
use std::process::Command;

/// Build a hermetic `ironmem` invocation: isolated HOME, explicit DB, noop embedder.
fn context_command(home: &Path, db_path: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ironmem"));
    cmd.env("HOME", home)
        .env("IRONMEM_DB_PATH", db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .arg("context");
    cmd
}

#[test]
fn context_json_smoke_emits_well_formed_pack() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("context.sqlite3");
    std::fs::create_dir_all(&home).unwrap();

    // An initialized but empty store is enough for the smoke test.
    {
        let db = ironmem::db::schema::Database::open(&db_path).unwrap();
        db.migrate().unwrap();
    }

    let out = context_command(&home, &db_path)
        .arg("--repo")
        .arg(temp.path())
        .arg("--task")
        .arg("explain metrics reporting")
        .arg("--json")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["task"].as_str(), Some("explain metrics reporting"));
    assert!(value["memory_hits"].is_array());
    assert!(value["decisions"].is_array());
    assert!(value["areas"].is_array());
    assert!(value["budget_tokens"].is_number());
}

#[test]
fn context_missing_area_reports_scout_required() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("ctx_missing.sqlite3");
    std::fs::create_dir_all(&home).unwrap();
    {
        let db = ironmem::db::schema::Database::open(&db_path).unwrap();
        db.migrate().unwrap();
    }

    let out = context_command(&home, &db_path)
        .arg("--repo")
        .arg(temp.path())
        .arg("--task")
        .arg("touch collab")
        .arg("--area")
        .arg("collab")
        .arg("--json")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    let areas = value["areas"].as_array().unwrap();
    assert_eq!(areas.len(), 1);
    assert_eq!(areas[0]["area"].as_str(), Some("collab"));
    assert_eq!(areas[0]["status"].as_str(), Some("missing"));
    assert!(areas[0]["reason"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("scout"));
}

#[test]
fn context_fresh_map_surfaces_summary_and_sha() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("ctx_fresh.sqlite3");
    std::fs::create_dir_all(&home).unwrap();

    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let run = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            ok.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&ok.stderr)
        );
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "t@t.t"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "seed"]);
    let head = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let repo_canon = std::fs::canonicalize(&repo)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    {
        let db = ironmem::db::schema::Database::open(&db_path).unwrap();
        db.migrate().unwrap();
        // Seed a code-map drawer + sidecar row via the real DB APIs.
        let content = "collab handoff lives in state_machine.rs";
        let wing = "code-maps";
        let room = "core";
        let drawer_id = ironmem::db::drawers::generate_id(content, wing, room);
        let embedding = vec![0.0f32; ironrace_embed::embedder::EMBED_DIM];
        db.insert_drawer(&drawer_id, content, &embedding, wing, room, "a.rs", "test")
            .unwrap();
        db.upsert_code_map(
            &repo_canon,
            "core",
            &drawer_id,
            &head,
            &["a.rs".to_string()],
            "test",
            "2026-06-20T00:00:00Z",
        )
        .unwrap();
    }

    let out = context_command(&home, &db_path)
        .arg("--repo")
        .arg(&repo)
        .arg("--task")
        .arg("touch core")
        .arg("--area")
        .arg("core")
        .arg("--json")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    let area = &value["areas"][0];
    assert_eq!(area["status"].as_str(), Some("fresh"));
    assert_eq!(area["source_file_count"].as_u64(), Some(1));
    assert!(area["summary"].as_str().unwrap().contains("collab handoff"));
    assert_eq!(area["head_sha"].as_str(), Some(&head[..7]));
}

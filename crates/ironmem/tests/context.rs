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

#[test]
fn context_stale_map_surfaces_changed_files_and_recommendation() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("ctx_stale.sqlite3");
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
    // Commit the tracked source file at the build SHA the map is seeded with.
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "seed"]);
    let build_head = String::from_utf8(
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
        // Seed a code-map drawer + sidecar row pinned to the original (build) HEAD.
        let content = "collab handoff lives in a.rs";
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
            &build_head,
            &["a.rs".to_string()],
            "test",
            "2026-06-20T00:00:00Z",
        )
        .unwrap();
    }

    // AFTER seeding the map at build_head, modify the tracked file and commit so
    // HEAD moves; the mapped file "a.rs" now shows changed in build_head..HEAD,
    // which makes `classify` return Stale.
    std::fs::write(repo.join("a.rs"), "fn a() { let _ = 1; }\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "change a.rs"]);

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
    assert_eq!(area["status"].as_str(), Some("stale"));
    let changed = area["changed_files"].as_array().unwrap();
    assert!(!changed.is_empty(), "expected non-empty changed_files");
    assert!(
        changed.iter().any(|f| f.as_str() == Some("a.rs")),
        "expected changed_files to contain a.rs, got {changed:?}"
    );
    assert!(
        !area["refresh_recommendation"].as_str().unwrap().is_empty(),
        "expected a non-empty refresh_recommendation"
    );
}

#[test]
fn context_surfaces_lexically_matching_memory_hit() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("ctx_mem.sqlite3");
    std::fs::create_dir_all(&home).unwrap();
    {
        let db = ironmem::db::schema::Database::open(&db_path).unwrap();
        db.migrate().unwrap();
        // Seed one drawer whose content lexically matches the task query.
        let content = "metrics reporting is rendered by report::render_text";
        let wing = "ironmem";
        let room = "docs";
        let drawer_id = ironmem::db::drawers::generate_id(content, wing, room);
        let embedding = vec![0.0f32; ironrace_embed::embedder::EMBED_DIM];
        db.insert_drawer(
            &drawer_id,
            content,
            &embedding,
            wing,
            room,
            "report.rs",
            "test",
        )
        .unwrap();
    }

    let out = context_command(&home, &db_path)
        .arg("--repo")
        .arg(temp.path())
        .arg("--task")
        .arg("metrics reporting")
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
    let hits = value["memory_hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "expected at least one memory hit");
    assert!(hits[0]["snippet"]
        .as_str()
        .unwrap()
        .contains("metrics reporting"));
    assert_eq!(hits[0]["wing"].as_str(), Some("ironmem"));
    // Snippet is bounded.
    assert!(
        hits[0]["snippet"].as_str().unwrap().chars().count() <= ironmem::context::SNIPPET_MAX_CHARS
    );
}

#[test]
fn context_surfaces_decisions_for_requested_area() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("ctx_kg.sqlite3");
    std::fs::create_dir_all(&home).unwrap();
    {
        let db = ironmem::db::schema::Database::open(&db_path).unwrap();
        db.migrate().unwrap();
        // Seed a decision keyed on the area name "collab" using the real KG
        // write API. add_triple auto-creates the "collab" entity (subject) and
        // stores the triple by entity id; resolve_entity("collab", None) then
        // recovers that entity and query_entity_current surfaces this triple.
        let kg = ironmem::db::knowledge_graph::KnowledgeGraph::new(&db);
        kg.add_triple(
            "collab",
            "area",
            "uses_state_machine",
            "bounded planning v3",
            "design",
            None,
            1.0,
            None,
        )
        .unwrap();
    }

    let out = context_command(&home, &db_path)
        .arg("--repo")
        .arg(temp.path())
        .arg("--task")
        .arg("change collab")
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
    let decisions = value["decisions"].as_array().unwrap();
    assert!(
        decisions
            .iter()
            .any(|d| d["predicate"].as_str() == Some("uses_state_machine")),
        "expected the seeded decision, got {decisions:?}"
    );
    let decision = decisions
        .iter()
        .find(|d| d["predicate"].as_str() == Some("uses_state_machine"))
        .expect("seeded decision present");
    assert_eq!(decision["subject"].as_str(), Some("collab"));
    assert_eq!(decision["object"].as_str(), Some("bounded planning v3"));
}

#[test]
fn context_tiny_budget_sets_truncated_flag() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("ctx_budget.sqlite3");
    std::fs::create_dir_all(&home).unwrap();
    {
        let db = ironmem::db::schema::Database::open(&db_path).unwrap();
        db.migrate().unwrap();
        // Seed 6 drawers that all lexically match the task query.
        for i in 0..6 {
            let content =
                format!("metrics reporting note number {i} with extra padding words here");
            let wing = "ironmem";
            let room = "docs";
            let drawer_id = ironmem::db::drawers::generate_id(&content, wing, room);
            let embedding = vec![0.0f32; ironrace_embed::embedder::EMBED_DIM];
            db.insert_drawer(
                &drawer_id,
                &content,
                &embedding,
                wing,
                room,
                "report.rs",
                "test",
            )
            .unwrap();
        }
    }

    let out = context_command(&home, &db_path)
        .arg("--repo")
        .arg(temp.path())
        .arg("--task")
        .arg("metrics reporting note")
        .arg("--budget")
        .arg("10")
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
    assert_eq!(value["truncated"].as_bool(), Some(true));
    assert!(value["memory_hits"].as_array().unwrap().len() < 6);
}

#[test]
fn context_invalid_slash_area_reports_missing_invalid_area_name() {
    // An area name containing a slash is rejected by sanitize_name (path
    // traversal guard); the area must surface as Missing with a reason that
    // names the invalid-area-name rejection, not a generic "no code map".
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("ctx_invalid.sqlite3");
    std::fs::create_dir_all(&home).unwrap();
    {
        let db = ironmem::db::schema::Database::open(&db_path).unwrap();
        db.migrate().unwrap();
    }

    let out = context_command(&home, &db_path)
        .arg("--repo")
        .arg(temp.path())
        .arg("--task")
        .arg("touch invalid")
        .arg("--area")
        .arg("a/b")
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
    assert_eq!(areas[0]["status"].as_str(), Some("missing"));
    assert!(
        areas[0]["reason"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("invalid area name"),
        "expected reason to flag the invalid area name, got {:?}",
        areas[0]["reason"]
    );
}

#[test]
fn context_same_area_requested_twice_dedups_decision() {
    // Requesting the same area name twice must yield two area entries (one per
    // request) but the underlying decision triple must appear exactly once —
    // the (subject, predicate, object) dedup guards against duplicates.
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("ctx_dedup.sqlite3");
    std::fs::create_dir_all(&home).unwrap();
    {
        let db = ironmem::db::schema::Database::open(&db_path).unwrap();
        db.migrate().unwrap();
        let kg = ironmem::db::knowledge_graph::KnowledgeGraph::new(&db);
        kg.add_triple(
            "collab",
            "area",
            "uses_state_machine",
            "bounded planning v3",
            "design",
            None,
            1.0,
            None,
        )
        .unwrap();
    }

    let out = context_command(&home, &db_path)
        .arg("--repo")
        .arg(temp.path())
        .arg("--task")
        .arg("change collab")
        .arg("--area")
        .arg("collab")
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
    // Two requests → two area entries.
    let areas = value["areas"].as_array().unwrap();
    assert_eq!(areas.len(), 2);
    // But the decision is deduped to a single occurrence.
    let decisions = value["decisions"].as_array().unwrap();
    let matches: Vec<_> = decisions
        .iter()
        .filter(|d| d["predicate"].as_str() == Some("uses_state_machine"))
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected the decision exactly once after dedup, got {decisions:?}"
    );
}

#[test]
fn context_text_marks_fresh_maps_as_pointers_and_missing_as_scout() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("ctx_render.sqlite3");
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
        .output() // text mode (no --json)
        .unwrap();

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("touch collab"));
    // Missing map renders an explicit scout-required section.
    assert!(text.to_lowercase().contains("scout required"));
    // The code-map pointer disclaimer is present whenever areas are shown.
    assert!(text.to_lowercase().contains("pointer"));
}

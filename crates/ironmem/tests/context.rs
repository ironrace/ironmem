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

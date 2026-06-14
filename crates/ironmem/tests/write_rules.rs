use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ironmem")
}

#[test]
fn write_rules_no_target_creates_both_files() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path();
    let out = Command::new(bin())
        .arg("write-rules")
        .arg("--workspace")
        .arg(ws)
        .output()
        .unwrap();
    assert!(out.status.success(), "write-rules failed: {out:?}");
    for name in ["CLAUDE.md", "AGENTS.md"] {
        let content = std::fs::read_to_string(ws.join(name)).unwrap();
        assert!(
            content.contains("BEGIN IRONMEM MEMORY PROTOCOL"),
            "{name} missing BEGIN marker"
        );
        assert!(
            content.contains("END IRONMEM MEMORY PROTOCOL"),
            "{name} missing END marker"
        );
    }
}

#[test]
fn write_rules_single_target_only_writes_that_file() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path();
    let out = Command::new(bin())
        .arg("write-rules")
        .arg("--workspace")
        .arg(ws)
        .arg("--target")
        .arg("AGENTS.md")
        .output()
        .unwrap();
    assert!(out.status.success(), "write-rules failed: {out:?}");
    assert!(ws.join("AGENTS.md").exists(), "AGENTS.md must be written");
    assert!(
        !ws.join("CLAUDE.md").exists(),
        "CLAUDE.md must not be touched when --target=AGENTS.md"
    );
}

#[test]
fn write_rules_cli_is_byte_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path();
    let run = || {
        Command::new(bin())
            .arg("write-rules")
            .arg("--workspace")
            .arg(ws)
            .output()
            .unwrap()
    };
    assert!(run().status.success());
    let first = std::fs::read(ws.join("CLAUDE.md")).unwrap();
    assert!(run().status.success());
    let second = std::fs::read(ws.join("CLAUDE.md")).unwrap();
    assert_eq!(first, second, "second CLI run must be byte-identical");
}

#[test]
fn write_rules_rejects_invalid_target() {
    let temp = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .arg("write-rules")
        .arg("--workspace")
        .arg(temp.path())
        .arg("--target")
        .arg("NOPE.md")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "invalid --target must be rejected by clap"
    );
}

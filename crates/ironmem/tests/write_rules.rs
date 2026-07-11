use std::process::Command;
use std::process::Output;

use std::path::Path;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ironmem")
}

fn canonical_block() -> String {
    ironmem::write_rules::render_block(ironmem::bootstrap::MEMORY_PROTOCOL)
}

fn claude_pointer_block() -> String {
    ironmem::write_rules::render_block("@AGENTS.md")
}

fn run_write_rules(workspace: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(bin());
    cmd.arg("write-rules").arg("--workspace").arg(workspace);
    cmd.args(args);
    cmd.output().unwrap()
}

fn write_rules_lines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| line.starts_with("ironmem write-rules:"))
        .map(ToString::to_string)
        .collect()
}

#[test]
fn write_rules_no_target_populates_canonical_and_pointer() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path();
    let out = run_write_rules(ws, &[]);
    assert!(out.status.success(), "write-rules failed: {out:?}");
    let expected_canonical = canonical_block();
    let expected_claude = claude_pointer_block();
    for name in ["CLAUDE.md", "AGENTS.md"] {
        let content = std::fs::read_to_string(ws.join(name)).unwrap();
        match name {
            "AGENTS.md" => assert_eq!(
                content, expected_canonical,
                "AGENTS.md must contain canonical block"
            ),
            _ => assert_eq!(
                content, expected_claude,
                "CLAUDE.md must contain @AGENTS.md pointer"
            ),
        }
    }
}

#[test]
fn write_rules_non_native_only_updates_specified_file_and_preserves_existing_claude() {
    let cases = [vec!["--target", "AGENTS.md"], vec!["--harness", "codex"]];

    for args in cases {
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path();
        let sentinel = "SENTINEL CLAUDE.md must remain unchanged\n";
        let stale_agents = canonical_block().replace(
            ironmem::bootstrap::MEMORY_PROTOCOL,
            "legacy protocol that should be updated",
        );
        std::fs::write(ws.join("CLAUDE.md"), sentinel).unwrap();
        std::fs::write(ws.join("AGENTS.md"), &stale_agents).unwrap();
        let out = run_write_rules(ws, &args);

        assert!(out.status.success(), "write-rules failed: {out:?}");
        assert_eq!(
            std::fs::read_to_string(ws.join("CLAUDE.md")).unwrap(),
            sentinel,
            "existing CLAUDE.md must remain byte-identical for {args:?}"
        );
        assert!(ws.join("AGENTS.md").exists(), "AGENTS.md must be written");
        assert_eq!(
            std::fs::read_to_string(ws.join("AGENTS.md")).unwrap(),
            canonical_block(),
            "AGENTS.md must be updated for {args:?}"
        );
    }
}

#[test]
fn write_rules_native_or_targeted_claude_updates_canonical_and_claude() {
    let cases = [vec!["--harness", "claude"], vec!["--target", "CLAUDE.md"]];

    for args in cases {
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path();
        let out = run_write_rules(ws, &args);
        assert!(out.status.success(), "write-rules failed: {out:?}");
        assert_eq!(
            std::fs::read_to_string(ws.join("AGENTS.md")).unwrap(),
            canonical_block(),
            "AGENTS.md must contain canonical block for {args:?}"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("CLAUDE.md")).unwrap(),
            claude_pointer_block(),
            "CLAUDE.md must contain @AGENTS.md pointer for {args:?}"
        );
    }
}

#[test]
fn write_rules_cli_is_byte_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path();
    let run = || run_write_rules(ws, &[]);
    assert!(run().status.success());
    let first_agents = std::fs::read(ws.join("AGENTS.md")).unwrap();
    let first_claude = std::fs::read(ws.join("CLAUDE.md")).unwrap();

    let second_output = run();
    assert!(second_output.status.success());
    let second_agents = std::fs::read(ws.join("AGENTS.md")).unwrap();
    let second_claude = std::fs::read(ws.join("CLAUDE.md")).unwrap();

    assert_eq!(
        first_agents, second_agents,
        "AGENTS.md must be byte-identical"
    );
    assert_eq!(
        first_claude, second_claude,
        "CLAUDE.md must be byte-identical"
    );

    let lines = write_rules_lines(&second_output);
    assert_eq!(lines.len(), 2, "both files should be reported");
    assert!(
        lines[0].contains("unchanged")
            && lines[0].contains("AGENTS.md")
            && lines[1].contains("unchanged")
            && lines[1].contains("CLAUDE.md"),
        "rerun should report unchanged for both files: {lines:?}"
    );
    assert!(
        lines[0].contains("AGENTS.md") && lines[1].contains("CLAUDE.md"),
        "canonical should be reported before dependent: {lines:?}"
    );
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
    assert!(!out.status.success(), "invalid --target must be rejected");
}

#[test]
fn write_rules_preflight_prevents_any_file_changes_on_invalid_canonical() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path();
    let malformed = "<!-- BEGIN IRONMEM MEMORY PROTOCOL -->\nno end marker\n";
    std::fs::write(ws.join("AGENTS.md"), malformed).unwrap();
    std::fs::write(ws.join("CLAUDE.md"), "existing claude").unwrap();

    let out = run_write_rules(ws, &[]);

    assert!(
        !out.status.success(),
        "malformed AGENTS.md must fail the default two-target run"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("AGENTS.md")).unwrap(),
        malformed,
        "malformed canonical must be left untouched"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("CLAUDE.md")).unwrap(),
        "existing claude",
        "CLAUDE.md must not be created or changed"
    );
}

#[test]
fn write_rules_preflight_prevents_any_file_changes_on_invalid_dependent() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path();
    let stale_agents = canonical_block().replace(
        ironmem::bootstrap::MEMORY_PROTOCOL,
        "legacy canonical managed block that should be updated",
    );
    std::fs::write(ws.join("AGENTS.md"), &stale_agents).unwrap();
    let malformed =
        "random before\n<!-- BEGIN IRONMEM MEMORY PROTOCOL -->\nno end marker\nrandom after\n";
    std::fs::write(ws.join("CLAUDE.md"), malformed).unwrap();

    let out = run_write_rules(ws, &[]);

    assert!(
        !out.status.success(),
        "malformed CLAUDE.md must fail the default run"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("AGENTS.md")).unwrap(),
        stale_agents,
        "stale AGENTS.md should be preserved"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("CLAUDE.md")).unwrap(),
        malformed,
        "malformed dependent must not be modified"
    );
}

#[test]
fn write_rules_migrates_full_protocol_to_pointer_with_user_lines_intact() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path();
    let before = "BEFORE\n";
    let after = "AFTER\n";
    let full = canonical_block();
    let claude_prior = format!("{before}{full}{after}");
    std::fs::write(ws.join("CLAUDE.md"), &claude_prior).unwrap();

    let out = run_write_rules(ws, &["--target", "CLAUDE.md"]);
    assert!(out.status.success(), "write-rules failed: {out:?}");

    let updated = std::fs::read_to_string(ws.join("CLAUDE.md")).unwrap();
    let expected = format!("{before}{}{after}", claude_pointer_block());
    assert_eq!(updated, expected, "full protocol should migrate to pointer");
    assert!(
        updated.starts_with(before),
        "content before managed block should remain unchanged"
    );
    assert!(
        updated.ends_with(after),
        "content after managed block should remain unchanged"
    );
}

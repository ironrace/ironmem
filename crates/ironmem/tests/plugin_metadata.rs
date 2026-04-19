//! Validate plugin metadata files for both Codex and Claude Code.
//!
//! Ensures required JSON fields are present and plugin versions stay in sync
//! with the crate version in Cargo.toml.

use std::path::PathBuf;

/// Walk up from CARGO_MANIFEST_DIR until we find the workspace root
/// (the directory containing a Cargo.toml with `[workspace]`).
/// This is resilient to crate restructuring.
fn workspace_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let toml = dir.join("Cargo.toml");
        if toml.exists() {
            let content = std::fs::read_to_string(&toml)
                .unwrap_or_else(|_| panic!("Could not read {}", toml.display()));
            if content.contains("[workspace]") {
                return dir;
            }
        }
        dir = dir
            .parent()
            .expect("reached filesystem root without finding workspace Cargo.toml")
            .to_path_buf();
    }
}

fn read_json(rel_path: &str) -> serde_json::Value {
    let path = workspace_root().join(rel_path);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("Could not read {}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("Invalid JSON in {rel_path}: {e}"))
}

#[test]
fn codex_plugin_json_has_required_fields() {
    let json = read_json(".codex-plugin/plugin.json");
    assert!(
        json["name"].is_string(),
        "codex plugin.json: missing 'name'"
    );
    assert!(
        json["version"].is_string(),
        "codex plugin.json: missing 'version'"
    );
    assert!(
        json["mcpServers"].is_object(),
        "codex plugin.json: missing 'mcpServers'"
    );
    assert!(
        json["hooks"].is_string(),
        "codex plugin.json: missing 'hooks' path"
    );
}

#[test]
fn codex_hooks_json_has_required_hooks() {
    let json = read_json(".codex-plugin/hooks.json");
    let hooks = &json["hooks"];
    assert!(
        hooks["SessionStart"].is_array(),
        "codex hooks.json: missing 'SessionStart'"
    );
    assert!(hooks["Stop"].is_array(), "codex hooks.json: missing 'Stop'");
    assert!(
        hooks["PreCompact"].is_array(),
        "codex hooks.json: missing 'PreCompact'"
    );
}

#[test]
fn claude_plugin_json_has_required_fields() {
    let json = read_json(".claude-plugin/plugin.json");
    assert!(
        json["name"].is_string(),
        "claude plugin.json: missing 'name'"
    );
    assert!(
        json["version"].is_string(),
        "claude plugin.json: missing 'version'"
    );
    assert!(
        json["mcpServers"].is_object(),
        "claude plugin.json: missing 'mcpServers'"
    );
}

#[test]
fn claude_mcp_json_has_required_fields() {
    let json = read_json(".claude-plugin/.mcp.json");
    let server = &json["ironmem"];
    assert!(
        server.is_object(),
        "claude .mcp.json: missing 'ironmem' server entry"
    );
    assert!(
        server["command"].is_string(),
        "claude .mcp.json: missing 'command'"
    );
    assert!(
        server["args"].is_array(),
        "claude .mcp.json: missing 'args'"
    );
}

#[test]
fn plugin_versions_match_cargo_toml() {
    let cargo_version = env!("CARGO_PKG_VERSION");

    let codex = read_json(".codex-plugin/plugin.json");
    let codex_version = codex["version"].as_str().unwrap_or("");
    assert_eq!(
        codex_version, cargo_version,
        "codex plugin.json version ({codex_version}) must match Cargo.toml ({cargo_version})"
    );

    let claude = read_json(".claude-plugin/plugin.json");
    let claude_version = claude["version"].as_str().unwrap_or("");
    assert_eq!(
        claude_version, cargo_version,
        "claude plugin.json version ({claude_version}) must match Cargo.toml ({cargo_version})"
    );
}

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
    let interface = &json["interface"];
    assert!(
        interface["displayName"].is_string(),
        "codex plugin.json: missing interface.displayName"
    );
    assert!(
        interface["shortDescription"].is_string(),
        "codex plugin.json: missing interface.shortDescription"
    );
    assert_eq!(
        interface["category"].as_str(),
        Some("Coding"),
        "codex plugin.json: interface.category must remain Coding"
    );
    assert_eq!(
        interface["brandColor"].as_str(),
        Some("#1C5D6B"),
        "codex plugin.json: interface.brandColor changed unexpectedly"
    );
    let capabilities = interface["capabilities"]
        .as_array()
        .expect("codex plugin.json: interface.capabilities must be an array");
    for capability in ["Interactive", "Read", "Write"] {
        assert!(
            capabilities
                .iter()
                .any(|value| value.as_str() == Some(capability)),
            "codex plugin.json: missing interface capability {capability}"
        );
    }
    assert!(
        interface["defaultPrompt"].is_null(),
        "codex plugin.json: defaultPrompt is forbidden under the canonical rules model; duplicating project rules or MEMORY_PROTOCOL here violates write-rules ownership"
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

/// Return the text between the first two `---` fences of a markdown file.
fn frontmatter(raw: &str) -> Option<&str> {
    let rest = raw.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// Read-only Claude review sub-agents must advertise an explicit lean tool
/// allowlist that excludes every ironmem MCP tool (issue #189). A missing
/// `tools:` key means the agent inherits the full MCP surface — including
/// memory tools — which is exactly the drift this guards against.
#[test]
fn claude_review_agents_advertise_lean_profile() {
    let review_agents = [
        "code-reviewer",
        "architect",
        "doc-reviewer",
        "security-reviewer",
    ];
    for agent in review_agents {
        let rel = format!(".claude-plugin/agents/{agent}.md");
        let path = workspace_root().join(&rel);
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("Could not read {}", path.display()));
        let front =
            frontmatter(&raw).unwrap_or_else(|| panic!("{rel}: missing YAML frontmatter block"));
        let tools_line = front
            .lines()
            .find(|l| l.trim_start().starts_with("tools:"))
            .unwrap_or_else(|| {
                panic!(
                    "{rel}: review agent must declare an explicit `tools:` allowlist \
                     so it does not inherit the full MCP surface (issue #189)"
                )
            });
        assert!(
            !tools_line.contains("ironmem"),
            "{rel}: review agent `tools:` must not list any ironmem MCP tool (found: {tools_line})"
        );
    }
}

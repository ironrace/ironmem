//! Idempotent registration of the ironmem MCP server into assistant config.
//! Re-running is a no-op (`AlreadyRegistered`), which preserves any existing
//! manual setup. Paths and config locations mirror `scripts/install-ironmem.sh`
//! and `doctor::detect_*`.

use std::path::Path;

use crate::error::MemoryError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegisterOutcome {
    Registered,
    AlreadyRegistered,
}

/// Write `contents` to `path` atomically (write to a temp sibling, then rename).
fn write_atomic(path: &Path, contents: &str) -> Result<(), MemoryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| MemoryError::Config(format!("create {}: {e}", parent.display())))?;
    }
    let tmp = path.with_extension("ironmem-tmp");
    std::fs::write(&tmp, contents)
        .map_err(|e| MemoryError::Config(format!("write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| MemoryError::Config(format!("rename to {}: {e}", path.display())))?;
    Ok(())
}

/// Ensure `mcpServers.ironmem = { command: <exe>, args: ["serve"] }` exists in a
/// Claude `~/.claude.json`-shaped file. Idempotent.
pub(crate) fn ensure_claude_registered(
    config_path: &Path,
    exe: &str,
) -> Result<RegisterOutcome, MemoryError> {
    let mut root: serde_json::Value = if config_path.exists() {
        let raw = std::fs::read_to_string(config_path)
            .map_err(|e| MemoryError::Config(format!("read {}: {e}", config_path.display())))?;
        serde_json::from_str(&raw)
            .map_err(|e| MemoryError::Config(format!("parse {}: {e}", config_path.display())))?
    } else {
        serde_json::json!({})
    };

    let obj = root.as_object_mut().ok_or_else(|| {
        MemoryError::Config(format!("{} is not a JSON object", config_path.display()))
    })?;
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers.as_object_mut().ok_or_else(|| {
        MemoryError::Config(format!(
            "mcpServers in {} is not an object",
            config_path.display()
        ))
    })?;

    if servers.contains_key("ironmem") {
        return Ok(RegisterOutcome::AlreadyRegistered);
    }
    servers.insert(
        "ironmem".to_string(),
        serde_json::json!({ "command": exe, "args": ["serve"] }),
    );

    let pretty = serde_json::to_string_pretty(&root)
        .map_err(|e| MemoryError::Config(format!("serialize claude config: {e}")))?;
    write_atomic(config_path, &pretty)?;
    Ok(RegisterOutcome::Registered)
}

/// Ensure a `[mcp_servers.ironmem]` block exists in a Codex `config.toml`.
/// Appends (never rewrites existing content) so manual edits survive. Idempotent.
pub(crate) fn ensure_codex_registered(
    config_path: &Path,
    exe: &str,
) -> Result<RegisterOutcome, MemoryError> {
    let existing = if config_path.exists() {
        std::fs::read_to_string(config_path)
            .map_err(|e| MemoryError::Config(format!("read {}: {e}", config_path.display())))?
    } else {
        String::new()
    };

    if existing
        .lines()
        .any(|line| line.trim() == "[mcp_servers.ironmem]")
    {
        return Ok(RegisterOutcome::AlreadyRegistered);
    }

    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&format!(
        "\n[mcp_servers.ironmem]\ncommand = \"{exe}\"\nargs = [\"serve\"]\n\n[mcp_servers.ironmem.env]\nIRONMEM_MCP_MODE = \"trusted\"\n"
    ));
    write_atomic(config_path, &next)?;
    Ok(RegisterOutcome::Registered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_registers_into_missing_file_then_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");

        let first = ensure_claude_registered(&cfg, "/usr/local/bin/ironmem").unwrap();
        assert_eq!(first, RegisterOutcome::Registered);

        let raw = std::fs::read_to_string(&cfg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v["mcpServers"]["ironmem"]["command"].as_str(),
            Some("/usr/local/bin/ironmem")
        );
        assert_eq!(
            v["mcpServers"]["ironmem"]["args"][0].as_str(),
            Some("serve")
        );

        let second = ensure_claude_registered(&cfg, "/usr/local/bin/ironmem").unwrap();
        assert_eq!(second, RegisterOutcome::AlreadyRegistered);
    }

    #[test]
    fn claude_preserves_unrelated_servers() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        std::fs::write(
            &cfg,
            r#"{"mcpServers":{"other":{"command":"x"}},"theme":"dark"}"#,
        )
        .unwrap();

        let outcome = ensure_claude_registered(&cfg, "/bin/ironmem").unwrap();
        assert_eq!(outcome, RegisterOutcome::Registered);

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["other"]["command"].as_str(), Some("x"));
        assert_eq!(v["theme"].as_str(), Some("dark"));
        assert_eq!(
            v["mcpServers"]["ironmem"]["command"].as_str(),
            Some("/bin/ironmem")
        );
    }

    #[test]
    fn codex_appends_block_then_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(&cfg, "# existing user config\nfoo = 1\n").unwrap();

        let first = ensure_codex_registered(&cfg, "/bin/ironmem").unwrap();
        assert_eq!(first, RegisterOutcome::Registered);

        let body = std::fs::read_to_string(&cfg).unwrap();
        assert!(body.contains("foo = 1"), "existing config preserved");
        assert!(body.contains("[mcp_servers.ironmem]"));
        assert!(body.contains("command = \"/bin/ironmem\""));
        assert!(body.contains("args = [\"serve\"]"));

        let second = ensure_codex_registered(&cfg, "/bin/ironmem").unwrap();
        assert_eq!(second, RegisterOutcome::AlreadyRegistered);
        // Idempotent: the block appears exactly once.
        let body2 = std::fs::read_to_string(&cfg).unwrap();
        assert_eq!(body2.matches("[mcp_servers.ironmem]").count(), 1);
    }

    #[test]
    fn codex_creates_missing_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".codex").join("config.toml");
        let outcome = ensure_codex_registered(&cfg, "/bin/ironmem").unwrap();
        assert_eq!(outcome, RegisterOutcome::Registered);
        assert!(cfg.exists());
    }
}

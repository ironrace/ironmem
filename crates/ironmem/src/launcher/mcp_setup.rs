//! Idempotent registration of the ironmem MCP server into assistant config.
//! Re-running is a no-op (`AlreadyRegistered`), which preserves any existing
//! manual setup. Paths and config locations mirror `scripts/install-ironmem.sh`
//! and `doctor::detect_*`.
//!
//! #190 Task 12: fresh installs register the shared-daemon proxy command
//! (`harness::proxy_command_args` — `["serve", "--connect", <socket>]`)
//! instead of bare `["serve"]`. A pre-existing bare `["serve"]` entry (this
//! crate's own prior output, before #190) is idempotently upgraded in place
//! to the proxy command; any other existing `args` value (already upgraded,
//! or hand-customized) is left untouched. Bare `serve` itself stays a valid,
//! fully-supported fallback (in-process stdio) regardless of what a harness's
//! registered command happens to be.

use std::path::Path;

use crate::error::MemoryError;

/// Escape a value for a TOML basic (double-quoted) string.
fn toml_basic_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// Render `items` as a TOML array-of-strings literal, e.g. `["a", "b"]`.
fn toml_string_array(items: &[String]) -> String {
    let escaped: Vec<String> = items
        .iter()
        .map(|s| format!("\"{}\"", toml_basic_escape(s)))
        .collect();
    format!("[{}]", escaped.join(", "))
}

/// The exact TOML line this crate wrote pre-#190 for a bare `serve` install.
/// Used ONLY to detect (and upgrade) that specific stale shape — a
/// hand-customized `args` line is never touched.
const STALE_BARE_SERVE_TOML_LINE: &str = "args = [\"serve\"]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegisterOutcome {
    Registered,
    AlreadyRegistered,
    /// A pre-existing bare `["serve"]` entry was migrated in place to the
    /// proxy command.
    Upgraded,
}

/// Write `contents` to `path` atomically (write to a temp sibling, then rename).
fn write_atomic(path: &Path, contents: &str) -> Result<(), MemoryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| MemoryError::Config(format!("create {}: {e}", parent.display())))?;
    }
    let tmp = path.with_extension(format!("ironmem-tmp-{}", std::process::id()));
    std::fs::write(&tmp, contents)
        .map_err(|e| MemoryError::Config(format!("write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        MemoryError::Config(format!("rename to {}: {e}", path.display()))
    })?;
    Ok(())
}

/// True when `args` is exactly the single-element `["serve"]` array this
/// crate wrote pre-#190 — the one shape eligible for automatic upgrade.
fn is_bare_serve_json(entry: &serde_json::Value) -> bool {
    entry
        .get("args")
        .and_then(|a| a.as_array())
        .map(|arr| arr.as_slice() == [serde_json::Value::String("serve".to_string())])
        .unwrap_or(false)
}

/// Ensure `mcpServers.ironmem = { command: <exe>, args: <proxy_args> }` exists
/// in ANY config file that shares Claude's object-shaped `mcpServers` JSON:
/// Claude's `~/.claude.json`, Gemini CLI's and (best-effort) Grok CLI's
/// `settings.json` (#190 Task 13), and Muse's `settings.json` (object shape
/// proven live on Muse Code 1.0.2). Idempotent; upgrades a stale bare
/// `["serve"]` entry in place (see module docs).
///
/// `seed` is the document a MISSING file starts from before the entry is
/// inserted — see [`fresh_file_seed`]. It is ignored when the file exists,
/// and must be a JSON object (anything else is refused like a non-object
/// file would be).
///
/// `entry_extras` are harness-specific keys merged into a BRAND-NEW
/// `ironmem` entry alongside `command`/`args` — see [`fresh_entry_extras`].
/// They are applied only on the fresh-registration path: an existing entry
/// (customized, or the bare-`["serve"]` upgrade) keeps exactly the keys the
/// user already has, so registration never adds behaviour to a config it
/// did not create.
pub(crate) fn ensure_json_mcpservers_registered(
    config_path: &Path,
    exe: &str,
    proxy_args: &[String],
    seed: serde_json::Value,
    entry_extras: serde_json::Map<String, serde_json::Value>,
) -> Result<RegisterOutcome, MemoryError> {
    let mut root: serde_json::Value = if config_path.exists() {
        let raw =
            crate::error::read_to_string_with_path(config_path).map_err(MemoryError::Config)?;
        serde_json::from_str(&raw)
            .map_err(|e| MemoryError::Config(format!("parse {}: {e}", config_path.display())))?
    } else {
        seed
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

    // Clone the (small) existing entry so this borrow ends before we need a
    // mutable borrow of `servers` to write the upgrade/fresh entry below.
    let existing_entry = servers.get("ironmem").cloned();
    let outcome = match existing_entry {
        Some(entry) if !is_bare_serve_json(&entry) => {
            return Ok(RegisterOutcome::AlreadyRegistered)
        }
        Some(mut entry) => {
            // Upgrade IN PLACE: mutate only `command`/`args` on the existing
            // entry object, preserving `env` and any other existing keys
            // (H2) — e.g. a script-installed `IRONMEM_MCP_MODE=trusted`
            // survives an automatic upgrade instead of silently reverting to
            // read-only. Only the fresh-registration (`None`) path below
            // writes a brand-new object from scratch.
            let entry_obj = entry.as_object_mut().ok_or_else(|| {
                MemoryError::Config(format!(
                    "mcpServers.ironmem in {} is not an object",
                    config_path.display()
                ))
            })?;
            entry_obj.insert("command".to_string(), serde_json::json!(exe));
            entry_obj.insert("args".to_string(), serde_json::json!(proxy_args));
            servers.insert("ironmem".to_string(), entry);
            RegisterOutcome::Upgraded
        }
        None => {
            let mut entry = entry_extras;
            entry.insert("command".to_string(), serde_json::json!(exe));
            entry.insert("args".to_string(), serde_json::json!(proxy_args));
            servers.insert("ironmem".to_string(), serde_json::Value::Object(entry));
            RegisterOutcome::Registered
        }
    };

    let pretty = serde_json::to_string_pretty(&root)
        .map_err(|e| MemoryError::Config(format!("serialize {}: {e}", config_path.display())))?;
    write_atomic(config_path, &pretty)?;
    Ok(outcome)
}

/// The document a missing JSON-`mcpServers` config starts from, per harness.
///
/// Muse's settings file carries a measured `schema_version: 1` envelope
/// (read live from `~/.config/muse/settings.json` on Muse Code 1.0.2), so a
/// file ironmem creates from scratch must carry it too rather than be a bare
/// `{"mcpServers": ...}` in a schema ironmem knows to be incomplete. Every
/// other JSON harness (Claude, Gemini CLI, Grok CLI) is unversioned and
/// starts from `{}`. Making this the writer's own seed (rather than a
/// separate pre-write step) means any caller that registers Muse gets the
/// envelope, and a fresh file is written exactly once.
pub(crate) fn fresh_file_seed(harness_id: &str) -> serde_json::Value {
    match harness_id {
        "muse" => serde_json::json!({ "schema_version": 1 }),
        _ => serde_json::json!({}),
    }
}

/// Harness-specific keys a freshly written `mcpServers.ironmem` entry
/// carries beyond `command`/`args`, per harness.
///
/// Muse's per-server settings (measured from the Muse Code 1.0.2 binary's
/// `McpServerSettings` field list: `enabled, mode, transport, command, args,
/// env, framing, url, headers`) include a `mode` whose values are
/// `required` (the default) and `optional`. Measured live with `muse exec
/// --provider echo` and a `command` pointing at a missing file: with no
/// `mode` key, or `required`, Muse exits 1 ("Required MCP server `ironmem`
/// failed during startup: the configured command is unavailable") before
/// answering anything; with `optional` it exits 0 and answers, running on
/// without the server. Memory is an
/// enhancement, not a prerequisite for editing code, and the proxy command
/// depends on a binary path and a daemon socket that can go stale (moved
/// install, unspawnable daemon) — so ironmem registers itself as
/// `optional`, matching how the other harnesses behave when the server is
/// down (Claude, Gemini CLI and Grok CLI have no abort-on-failure mode).
/// Every other JSON harness gets no extra keys.
pub(crate) fn fresh_entry_extras(harness_id: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut extras = serde_json::Map::new();
    if harness_id == "muse" {
        extras.insert("mode".to_string(), serde_json::json!("optional"));
    }
    extras
}

/// Ensure a `[mcp_servers.ironmem]` block exists in a Codex `config.toml`.
/// Appends (never rewrites existing content) so manual edits survive.
/// Idempotent; upgrades a stale bare `args = ["serve"]` line in place (see
/// module docs).
pub(crate) fn ensure_codex_registered(
    config_path: &Path,
    exe: &str,
    proxy_args: &[String],
) -> Result<RegisterOutcome, MemoryError> {
    let existing = if config_path.exists() {
        crate::error::read_to_string_with_path(config_path).map_err(MemoryError::Config)?
    } else {
        String::new()
    };

    let args_toml = toml_string_array(proxy_args);

    // H3: scope the stale-line search to the `[mcp_servers.ironmem]` section
    // itself — from its header to the next `\n[` (or EOF) — mirroring
    // `doctor::codex_proxy_wiring`'s section-slicing. A whole-file
    // `existing.find(STALE_BARE_SERVE_TOML_LINE)` would happily rewrite a
    // DIFFERENT `[mcp_servers.*]` block's `args = ["serve"]` line if one
    // happened to appear earlier in the file.
    if let Some(header_idx) = existing.find("[mcp_servers.ironmem]") {
        let section = &existing[header_idx..];
        let section_end = section[1..]
            .find("\n[")
            .map(|i| i + 1)
            .unwrap_or(section.len());
        let scoped = &section[..section_end];

        if let Some(rel_idx) = scoped.find(STALE_BARE_SERVE_TOML_LINE) {
            let abs_idx = header_idx + rel_idx;
            let mut upgraded = existing.clone();
            upgraded.replace_range(
                abs_idx..abs_idx + STALE_BARE_SERVE_TOML_LINE.len(),
                &format!("args = {args_toml}"),
            );
            write_atomic(config_path, &upgraded)?;
            return Ok(RegisterOutcome::Upgraded);
        }
        return Ok(RegisterOutcome::AlreadyRegistered);
    }

    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&format!(
        "\n[mcp_servers.ironmem]\ncommand = \"{}\"\nargs = {args_toml}\n\n[mcp_servers.ironmem.env]\nIRONMEM_MCP_MODE = \"trusted\"\n",
        toml_basic_escape(exe)
    ));
    write_atomic(config_path, &next)?;
    Ok(RegisterOutcome::Registered)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Claude-shaped writer with an empty seed — what every harness
    /// except Muse registers through.
    fn ensure_claude_registered(
        config_path: &Path,
        exe: &str,
        proxy_args: &[String],
    ) -> Result<RegisterOutcome, MemoryError> {
        ensure_json_mcpservers_registered(
            config_path,
            exe,
            proxy_args,
            serde_json::json!({}),
            serde_json::Map::new(),
        )
    }

    /// Representative proxy args used by tests that don't care about the
    /// exact socket path, only that registration writes/upgrades to it.
    fn test_proxy_args() -> Vec<String> {
        vec![
            "serve".to_string(),
            "--connect".to_string(),
            "/tmp/ironmem-test/daemon.sock".to_string(),
        ]
    }

    #[test]
    fn claude_registers_into_missing_file_then_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        let proxy_args = test_proxy_args();

        let first = ensure_claude_registered(&cfg, "/usr/local/bin/ironmem", &proxy_args).unwrap();
        assert_eq!(first, RegisterOutcome::Registered);

        let raw = std::fs::read_to_string(&cfg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v["mcpServers"]["ironmem"]["command"].as_str(),
            Some("/usr/local/bin/ironmem")
        );
        assert_eq!(
            v["mcpServers"]["ironmem"]["args"],
            serde_json::json!(proxy_args)
        );

        let second = ensure_claude_registered(&cfg, "/usr/local/bin/ironmem", &proxy_args).unwrap();
        assert_eq!(second, RegisterOutcome::AlreadyRegistered);
        // Idempotent: content unchanged by the second run.
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), raw);
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

        let outcome = ensure_claude_registered(&cfg, "/bin/ironmem", &test_proxy_args()).unwrap();
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

    /// #190 Task 12 acceptance: a pre-existing bare `["serve"]` entry (this
    /// crate's own pre-#190 output) is upgraded in place to the proxy command.
    #[test]
    fn claude_upgrades_stale_bare_serve_entry_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        std::fs::write(
            &cfg,
            r#"{"mcpServers":{"ironmem":{"command":"/bin/ironmem","args":["serve"]},"other":{"command":"x"}}}"#,
        )
        .unwrap();
        let proxy_args = test_proxy_args();

        let outcome = ensure_claude_registered(&cfg, "/bin/ironmem", &proxy_args).unwrap();
        assert_eq!(outcome, RegisterOutcome::Upgraded);

        let raw = std::fs::read_to_string(&cfg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v["mcpServers"]["ironmem"]["args"],
            serde_json::json!(proxy_args)
        );
        // Unrelated server entries survive the upgrade untouched.
        assert_eq!(v["mcpServers"]["other"]["command"].as_str(), Some("x"));

        // Idempotent: re-running after the upgrade makes no further changes.
        let second = ensure_claude_registered(&cfg, "/bin/ironmem", &proxy_args).unwrap();
        assert_eq!(second, RegisterOutcome::AlreadyRegistered);
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), raw);
    }

    /// H2 acceptance: a pre-existing bare `["serve"]` entry that also carries
    /// an `env` block (e.g. a script-installed `IRONMEM_MCP_MODE=trusted`)
    /// must keep that `env` block after the automatic upgrade — the upgrade
    /// must mutate only `args`/`command`, never replace the whole object.
    #[test]
    fn claude_upgrade_preserves_existing_env_block() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        std::fs::write(
            &cfg,
            r#"{"mcpServers":{"ironmem":{"command":"/bin/ironmem","args":["serve"],"env":{"IRONMEM_MCP_MODE":"trusted"}}}}"#,
        )
        .unwrap();
        let proxy_args = test_proxy_args();

        let outcome = ensure_claude_registered(&cfg, "/bin/ironmem", &proxy_args).unwrap();
        assert_eq!(outcome, RegisterOutcome::Upgraded);

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            v["mcpServers"]["ironmem"]["args"],
            serde_json::json!(proxy_args)
        );
        assert_eq!(
            v["mcpServers"]["ironmem"]["env"]["IRONMEM_MCP_MODE"].as_str(),
            Some("trusted"),
            "env block must survive the automatic args upgrade"
        );
    }

    /// A hand-customized (or already-upgraded-with-different-args) entry must
    /// NEVER be silently rewritten — only the exact stale `["serve"]` shape
    /// is eligible for automatic upgrade.
    #[test]
    fn claude_does_not_touch_a_customized_args_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        std::fs::write(
            &cfg,
            r#"{"mcpServers":{"ironmem":{"command":"/bin/ironmem","args":["serve","--custom-flag"]}}}"#,
        )
        .unwrap();
        let raw_before = std::fs::read_to_string(&cfg).unwrap();

        let outcome = ensure_claude_registered(&cfg, "/bin/ironmem", &test_proxy_args()).unwrap();
        assert_eq!(outcome, RegisterOutcome::AlreadyRegistered);
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), raw_before);
    }

    #[test]
    fn codex_appends_block_then_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(&cfg, "# existing user config\nfoo = 1\n").unwrap();
        let proxy_args = test_proxy_args();

        let first = ensure_codex_registered(&cfg, "/bin/ironmem", &proxy_args).unwrap();
        assert_eq!(first, RegisterOutcome::Registered);

        let body = std::fs::read_to_string(&cfg).unwrap();
        assert!(body.contains("foo = 1"), "existing config preserved");
        assert!(body.contains("[mcp_servers.ironmem]"));
        assert!(body.contains("command = \"/bin/ironmem\""));
        assert!(body.contains(&format!("args = {}", toml_string_array(&proxy_args))));

        let second = ensure_codex_registered(&cfg, "/bin/ironmem", &proxy_args).unwrap();
        assert_eq!(second, RegisterOutcome::AlreadyRegistered);
        // Idempotent: the block appears exactly once, content unchanged.
        let body2 = std::fs::read_to_string(&cfg).unwrap();
        assert_eq!(body2.matches("[mcp_servers.ironmem]").count(), 1);
        assert_eq!(body2, body);
    }

    /// #190 Task 12 acceptance: a pre-existing bare `args = ["serve"]` line
    /// (this crate's own pre-#190 output) is upgraded in place.
    #[test]
    fn codex_upgrades_stale_bare_serve_entry_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(
            &cfg,
            "foo = 1\n\n[mcp_servers.ironmem]\ncommand = \"/bin/ironmem\"\nargs = [\"serve\"]\n\n[mcp_servers.ironmem.env]\nIRONMEM_MCP_MODE = \"trusted\"\n",
        )
        .unwrap();
        let proxy_args = test_proxy_args();

        let outcome = ensure_codex_registered(&cfg, "/bin/ironmem", &proxy_args).unwrap();
        assert_eq!(outcome, RegisterOutcome::Upgraded);

        let body = std::fs::read_to_string(&cfg).unwrap();
        assert!(body.contains("foo = 1"), "unrelated config preserved");
        assert!(body.contains(&format!("args = {}", toml_string_array(&proxy_args))));
        assert!(
            !body.contains(STALE_BARE_SERVE_TOML_LINE),
            "stale bare-serve line must be gone after upgrade"
        );
        assert_eq!(
            body.matches("[mcp_servers.ironmem]").count(),
            1,
            "upgrade must not duplicate the section"
        );

        // Idempotent: re-running after the upgrade makes no further changes.
        let second = ensure_codex_registered(&cfg, "/bin/ironmem", &proxy_args).unwrap();
        assert_eq!(second, RegisterOutcome::AlreadyRegistered);
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), body);
    }

    /// H3 acceptance: a stale bare-serve `[mcp_servers.ironmem]` block that
    /// comes AFTER an unrelated `[mcp_servers.other]` block (which also has
    /// `args = ["serve"]`) must only have ITS OWN line upgraded — the other
    /// section's args must be left untouched. Before the fix, a whole-file
    /// `existing.find(STALE_BARE_SERVE_TOML_LINE)` would match the FIRST
    /// occurrence in the file, which belongs to `other`, and corrupt it.
    #[test]
    fn codex_upgrade_is_scoped_to_the_ironmem_section_only() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(
            &cfg,
            "[mcp_servers.other]\ncommand = \"/bin/other\"\nargs = [\"serve\"]\n\n[mcp_servers.ironmem]\ncommand = \"/bin/ironmem\"\nargs = [\"serve\"]\n\n[mcp_servers.ironmem.env]\nIRONMEM_MCP_MODE = \"trusted\"\n",
        )
        .unwrap();
        let proxy_args = test_proxy_args();

        let outcome = ensure_codex_registered(&cfg, "/bin/ironmem", &proxy_args).unwrap();
        assert_eq!(outcome, RegisterOutcome::Upgraded);

        let body = std::fs::read_to_string(&cfg).unwrap();
        let other_section_end = body.find("[mcp_servers.ironmem]").unwrap();
        let other_section = &body[..other_section_end];
        assert!(
            other_section.contains("args = [\"serve\"]"),
            "unrelated section's args must be untouched, got:\n{body}"
        );
        let ironmem_section = &body[other_section_end..];
        assert!(
            ironmem_section.contains(&format!("args = {}", toml_string_array(&proxy_args))),
            "ironmem section's args must be upgraded, got:\n{body}"
        );
    }

    /// A hand-customized args line must never be silently rewritten.
    #[test]
    fn codex_does_not_touch_a_customized_args_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(
            &cfg,
            "[mcp_servers.ironmem]\ncommand = \"/bin/ironmem\"\nargs = [\"serve\", \"--custom-flag\"]\n",
        )
        .unwrap();
        let raw_before = std::fs::read_to_string(&cfg).unwrap();

        let outcome = ensure_codex_registered(&cfg, "/bin/ironmem", &test_proxy_args()).unwrap();
        assert_eq!(outcome, RegisterOutcome::AlreadyRegistered);
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), raw_before);
    }

    #[test]
    fn codex_creates_missing_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".codex").join("config.toml");
        let outcome = ensure_codex_registered(&cfg, "/bin/ironmem", &test_proxy_args()).unwrap();
        assert_eq!(outcome, RegisterOutcome::Registered);
        assert!(cfg.exists());
    }

    #[test]
    fn codex_escapes_exe_path_with_special_chars() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        // exe path contains a double-quote, which would break raw TOML interpolation.
        let exe = r#"/opt/iron"mem/ironmem"#;
        let outcome = ensure_codex_registered(&cfg, exe, &test_proxy_args()).unwrap();
        assert_eq!(outcome, RegisterOutcome::Registered);

        let body = std::fs::read_to_string(&cfg).unwrap();
        // The file must contain the properly escaped TOML basic-string sequence.
        assert!(
            body.contains(r#"command = "/opt/iron\"mem/ironmem""#),
            "expected escaped TOML command line, got:\n{body}"
        );
    }

    #[test]
    fn toml_basic_escape_handles_all_special_chars() {
        assert_eq!(toml_basic_escape(r#"a"b"#), r#"a\"b"#);
        assert_eq!(toml_basic_escape(r"a\b"), r"a\\b");
        assert_eq!(toml_basic_escape("a\nb"), r"a\nb");
        assert_eq!(toml_basic_escape("a\tb"), r"a\tb");
        assert_eq!(toml_basic_escape("/plain/path"), "/plain/path");
    }

    #[test]
    fn toml_string_array_escapes_each_element() {
        let items = vec!["serve".to_string(), r#"a"b"#.to_string()];
        assert_eq!(toml_string_array(&items), r#"["serve", "a\"b"]"#);
    }

    #[test]
    fn claude_rejects_non_object_root() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        std::fs::write(&cfg, "[]").unwrap();
        let err = ensure_claude_registered(&cfg, "/bin/ironmem", &test_proxy_args()).unwrap_err();
        assert!(err.to_string().contains("not a JSON object"), "got: {err}");
    }

    #[test]
    fn claude_rejects_non_object_mcpservers() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        std::fs::write(&cfg, r#"{"mcpServers": 5}"#).unwrap();
        let err = ensure_claude_registered(&cfg, "/bin/ironmem", &test_proxy_args()).unwrap_err();
        assert!(err.to_string().contains("is not an object"), "got: {err}");
    }

    #[test]
    fn claude_rejects_unparseable_config_and_leaves_it_untouched() {
        // The parse-failure branch is the only guard against clobbering a
        // settings file with a typo in it: the error must name the parse and
        // the bytes on disk must survive byte-for-byte.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        std::fs::write(&cfg, "{ \"mcpServers\": { \"other\": {} }, }").unwrap();
        let before = std::fs::read_to_string(&cfg).unwrap();
        let err = ensure_claude_registered(&cfg, "/bin/ironmem", &test_proxy_args()).unwrap_err();
        assert!(err.to_string().contains("parse"), "got: {err}");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
    }

    // ---- Fresh-file seed (Muse envelope MEASURED live; see fresh_file_seed) ----

    #[test]
    fn fresh_file_seed_is_muse_envelope_and_empty_elsewhere() {
        assert_eq!(
            fresh_file_seed("muse"),
            serde_json::json!({ "schema_version": 1 })
        );
        for id in ["claude", "gemini", "grok", "zeta"] {
            assert_eq!(fresh_file_seed(id), serde_json::json!({}), "{id}");
        }
    }

    #[test]
    fn muse_registration_seeds_measured_schema_version_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("muse").join("settings.json");
        let proxy_args = test_proxy_args();
        let outcome = ensure_json_mcpservers_registered(
            &cfg,
            "/bin/ironmem",
            &proxy_args,
            fresh_file_seed("muse"),
            fresh_entry_extras("muse"),
        )
        .unwrap();
        assert_eq!(outcome, RegisterOutcome::Registered);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v.get("schema_version").and_then(|s| s.as_i64()), Some(1));
        assert_eq!(
            v["mcpServers"]["ironmem"]["args"],
            serde_json::json!(proxy_args)
        );
    }

    #[test]
    fn seed_is_ignored_for_an_existing_file() {
        // An existing file keeps exactly its own keys: the seed never
        // injects schema_version into a file the user already has.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("settings.json");
        std::fs::write(&cfg, r#"{"mcpServers":{"other":{"command":"x"}}}"#).unwrap();
        let outcome = ensure_json_mcpservers_registered(
            &cfg,
            "/bin/ironmem",
            &test_proxy_args(),
            fresh_file_seed("muse"),
            fresh_entry_extras("muse"),
        )
        .unwrap();
        assert_eq!(outcome, RegisterOutcome::Registered);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(v.get("schema_version").is_none(), "seed must not leak: {v}");
        assert_eq!(v["mcpServers"]["other"]["command"].as_str(), Some("x"));
        assert!(v["mcpServers"]["ironmem"].is_object());
    }

    #[test]
    fn non_object_seed_is_refused_like_a_non_object_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("settings.json");
        let err = ensure_json_mcpservers_registered(
            &cfg,
            "/bin/ironmem",
            &test_proxy_args(),
            serde_json::json!([]),
            serde_json::Map::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a JSON object"), "got: {err}");
        assert!(!cfg.exists(), "nothing may be written for a refused seed");
    }

    // ---- Fresh-entry extras (Muse `mode` MEASURED from the binary's
    // McpServerSettings field list; see fresh_entry_extras) ----

    #[test]
    fn fresh_entry_extras_is_optional_mode_for_muse_and_empty_elsewhere() {
        let muse = fresh_entry_extras("muse");
        assert_eq!(muse.len(), 1, "{muse:?}");
        assert_eq!(muse.get("mode"), Some(&serde_json::json!("optional")));
        for id in ["claude", "gemini", "grok", "zeta"] {
            assert!(fresh_entry_extras(id).is_empty(), "{id}");
        }
    }

    #[test]
    fn muse_fresh_entry_is_registered_as_an_optional_server() {
        // A required (Muse's default) server that fails to start aborts the
        // whole Muse session; the entry ironmem creates must opt out of that.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("muse").join("settings.json");
        let proxy_args = test_proxy_args();
        let outcome = ensure_json_mcpservers_registered(
            &cfg,
            "/bin/ironmem",
            &proxy_args,
            fresh_file_seed("muse"),
            fresh_entry_extras("muse"),
        )
        .unwrap();
        assert_eq!(outcome, RegisterOutcome::Registered);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let entry = v["mcpServers"]["ironmem"].as_object().unwrap();
        assert_eq!(entry.get("mode"), Some(&serde_json::json!("optional")));
        // The extras never displace the canonical keys.
        assert_eq!(
            entry.get("command"),
            Some(&serde_json::json!("/bin/ironmem"))
        );
        assert_eq!(entry.get("args"), Some(&serde_json::json!(proxy_args)));
        assert_eq!(entry.len(), 3, "exactly command/args/mode: {entry:?}");
    }

    #[test]
    fn claude_fresh_entry_carries_no_mode_key() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        ensure_claude_registered(&cfg, "/bin/ironmem", &test_proxy_args()).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let entry = v["mcpServers"]["ironmem"].as_object().unwrap();
        assert!(entry.get("mode").is_none(), "{entry:?}");
        assert_eq!(entry.len(), 2, "exactly command/args: {entry:?}");
    }

    #[test]
    fn extras_are_not_applied_to_an_existing_entry_on_upgrade() {
        // The bare-["serve"] upgrade mutates only command/args; a Muse entry
        // the user wrote without `mode` keeps its own (required) semantics.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("settings.json");
        std::fs::write(
            &cfg,
            r#"{"schema_version":1,"mcpServers":{"ironmem":{"command":"/bin/ironmem","args":["serve"]}}}"#,
        )
        .unwrap();
        let outcome = ensure_json_mcpservers_registered(
            &cfg,
            "/bin/ironmem",
            &test_proxy_args(),
            fresh_file_seed("muse"),
            fresh_entry_extras("muse"),
        )
        .unwrap();
        assert_eq!(outcome, RegisterOutcome::Upgraded);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(
            v["mcpServers"]["ironmem"].get("mode").is_none(),
            "upgrade must not add keys the user did not write: {v}"
        );
    }
}

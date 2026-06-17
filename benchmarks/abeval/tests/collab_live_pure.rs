use std::path::Path;
use abeval::collab_live::{codex_exec_argv, claude_worker_argv, minimal_codex_config};

#[test]
fn codex_exec_argv_is_no_shell_and_isolated() {
    let (prog, args) = codex_exec_argv(Path::new("/tmp/wt"), "join sess-1");
    assert_eq!(prog, "codex");
    // -s danger-full-access, -C <worktree>, and the prompt as a single positional.
    assert!(args.windows(2).any(|w| w == ["-s", "danger-full-access"]));
    assert!(args.windows(2).any(|w| w == ["-C", "/tmp/wt"]));
    assert!(args.iter().any(|a| a == "join sess-1"));
    // never a shell.
    assert!(!args.iter().any(|a| a == "-c" || a.contains("sh ")));
}

#[test]
fn claude_worker_argv_carries_json_and_mcp_config() {
    let (prog, args) = claude_worker_argv(r#"{"mcpServers":{}}"#);
    assert_eq!(prog, "claude");
    assert!(args.windows(2).any(|w| w == ["--output-format", "json"]));
    assert!(args.iter().any(|a| a == "--mcp-config"));
    assert!(args.iter().any(|a| a == "-p"));
}

#[test]
fn minimal_codex_config_has_no_unparseable_app_keys() {
    let toml = minimal_codex_config();
    // Per memory feedback_codex_app_config_rewrite: keep config minimal so the
    // older CLI can parse it — no service_tier / relative-agent-paths.
    assert!(!toml.contains("service_tier"));
    assert!(!toml.contains("relative-agent-paths"));
}

//! The Claude Code hook doctor checks (#377): `hooks` and `hook_mining`.
//!
//! A plugin install gets its four hooks from `.claude-plugin/hooks/hooks.json`
//! and runs them through a wrapper that defaults to trusted mode. A non-plugin
//! install gets whatever `~/.claude/settings.json` holds, and hook commands
//! there default to read-only, which skips mining. Neither gap errors at run
//! time, so doctor is where it has to show up. Diagnose-only: never writes.

use std::path::Path;

use serde_json::Value;

use super::{Check, CheckStatus};
use crate::config::McpAccessMode;

/// The Claude Code event and `ironmem hook` name of each plugin hook, in the
/// order `.claude-plugin/hooks/hooks.json` lists them. A unit test pins it to
/// that file, and scripts/install-ironmem.sh's `CLAUDE_HOOK_EVENTS` mirrors it.
pub(super) const PLUGIN_HOOKS: [(&str, &str); 4] = [
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "user-prompt-submit"),
    ("Stop", "stop"),
    ("PreCompact", "precompact"),
];

/// The hooks whose run mines transcripts (and is skipped without write access).
const MINING_EVENTS: [&str; 2] = ["Stop", "PreCompact"];

const INSTALL_HINT: &str =
    "re-run scripts/install-ironmem.sh to wire the missing hooks, or install the ironmem Claude Code plugin";

const READ_ONLY_HINT: &str =
    "expected outside a plugin install: MCP writes and prompt recall still work. \
A cheaper way to mine without the plugin is tracked in #418. To mine now, prefix the Stop and \
PreCompact commands with IRONMEM_MCP_MODE=trusted; each run then loads the embedding model and \
rebuilds the vector index";

/// Where a hook's effective `IRONMEM_MCP_MODE` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeSource {
    /// `IRONMEM_MCP_MODE=...` in the hook command itself.
    Inline,
    /// `env.IRONMEM_MCP_MODE` in settings.json. UNVERIFIED that Claude Code
    /// passes this block to hook processes; `mined_rows` is the backstop.
    SettingsEnv,
    /// `IRONMEM_MCP_MODE` in doctor's own environment, used as a stand-in
    /// for the environment Claude Code passes to its hooks.
    Environment,
    /// The plugin wrapper's `${IRONMEM_MCP_MODE:-trusted}` default.
    PluginWrapper,
    /// Nothing set it: `Config` defaults to read-only.
    Default,
}

fn source_label(source: ModeSource) -> &'static str {
    match source {
        ModeSource::Inline => "set in the hook command",
        ModeSource::SettingsEnv => "set by env in settings.json",
        ModeSource::Environment => "inherited from IRONMEM_MCP_MODE in this shell",
        ModeSource::PluginWrapper => "the plugin wrapper's default",
        ModeSource::Default => "IRONMEM_MCP_MODE unset, so the default",
    }
}

fn mode_label(mode: McpAccessMode) -> &'static str {
    match mode {
        McpAccessMode::Trusted => "trusted",
        McpAccessMode::ReadOnly => "read-only",
        McpAccessMode::Restricted => "restricted",
    }
}

/// What `~/.claude/settings.json` says about the ironmem hooks.
#[derive(Debug)]
enum HookWiring {
    /// `~/.claude` does not exist: Claude Code is not in use here.
    NoClaude,
    /// `~/.claude` exists but `settings.json` does not.
    NoSettings,
    /// The file exists but cannot be read as a JSON object.
    Unreadable(String),
    /// The ironmem plugin is enabled; its hooks.json supplies the hooks.
    Plugin,
    /// Hooks registered by hand or by scripts/install-ironmem.sh.
    Manual {
        /// `(event, command)` for each plugin hook found, in PLUGIN_HOOKS order.
        wired: Vec<(&'static str, String)>,
        /// Plugin hook events with no matching command.
        missing: Vec<&'static str>,
        /// `env.IRONMEM_MCP_MODE` from settings.json, if set.
        settings_env_mode: Option<String>,
    },
}

/// Build the `hooks` and `hook_mining` checks for Claude Code. Always returns
/// exactly those two keys, in that order, so doctor's key set is stable.
/// `process_env_mode` is doctor's own `IRONMEM_MCP_MODE`, passed in so tests
/// don't mutate the process environment.
pub(super) fn claude_hook_checks(
    home: Option<&Path>,
    process_env_mode: Option<&str>,
) -> Vec<Check> {
    let Some(home) = home else {
        return vec![
            Check::new(
                "hooks",
                CheckStatus::Info,
                "Claude Code hooks: home directory unknown, skipped",
            ),
            Check::new(
                "hook_mining",
                CheckStatus::Info,
                "hook mining: skipped (home directory unknown)",
            ),
        ];
    };
    let claude_dir = home.join(".claude");
    let path = claude_dir.join("settings.json");
    match detect_hook_wiring(&claude_dir) {
        HookWiring::NoClaude => vec![
            Check::new(
                "hooks",
                CheckStatus::Info,
                format!("Claude Code hooks: {} not found, skipped", claude_dir.display()),
            ),
            Check::new("hook_mining", CheckStatus::Info, "hook mining: skipped (no Claude Code config)"),
        ],
        HookWiring::NoSettings => vec![
            Check::new(
                "hooks",
                CheckStatus::Warn,
                format!(
                    "Claude Code hooks: none wired ({} does not exist): prompt recall and transcript mining are off",
                    path.display()
                ),
            )
            .with_hint(INSTALL_HINT),
            no_miner_check(),
        ],
        HookWiring::Unreadable(e) => vec![
            Check::new(
                "hooks",
                CheckStatus::Warn,
                format!("Claude Code hooks: cannot read {}: {e}", path.display()),
            )
            .with_hint("fix the file by hand, then re-run scripts/install-ironmem.sh"),
            Check::new("hook_mining", CheckStatus::Info, "hook mining: skipped (settings.json unreadable)"),
        ],
        HookWiring::Plugin => vec![
            Check::new("hooks", CheckStatus::Ok, "Claude Code hooks: supplied by the enabled ironmem plugin"),
            Check::new(
                "hook_mining",
                CheckStatus::Ok,
                "Stop/PreCompact hooks run through the plugin wrapper (trusted by default): transcripts are mined",
            ),
        ],
        HookWiring::Manual { wired, missing, settings_env_mode } => vec![
            hooks_check(&path, &missing),
            mining_check(&wired, settings_env_mode.as_deref(), process_env_mode),
        ],
    }
}

fn detect_hook_wiring(claude_dir: &Path) -> HookWiring {
    if !claude_dir.is_dir() {
        return HookWiring::NoClaude;
    }
    let raw = match std::fs::read_to_string(claude_dir.join("settings.json")) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HookWiring::NoSettings,
        Err(e) => return HookWiring::Unreadable(e.to_string()),
    };
    let doc: Value = match serde_json::from_str(&raw) {
        Ok(doc) => doc,
        Err(e) => return HookWiring::Unreadable(format!("not valid JSON: {e}")),
    };
    if !doc.is_object() {
        return HookWiring::Unreadable("not a JSON object".to_string());
    }
    if plugin_enabled(&doc) {
        return HookWiring::Plugin;
    }
    let mut wired = Vec::new();
    let mut missing = Vec::new();
    for (event, hook) in PLUGIN_HOOKS {
        match commands_for(&doc, event)
            .into_iter()
            .find(|c| runs_hook(c, hook))
        {
            Some(cmd) => wired.push((event, cmd)),
            None => missing.push(event),
        }
    }
    let settings_env_mode = doc
        .pointer("/env/IRONMEM_MCP_MODE")
        .and_then(Value::as_str)
        .map(str::to_owned);
    HookWiring::Manual {
        wired,
        missing,
        settings_env_mode,
    }
}

/// True when `enabledPlugins` turns on a plugin named `ironmem` (keys are
/// `name@marketplace`).
fn plugin_enabled(doc: &Value) -> bool {
    doc.get("enabledPlugins")
        .and_then(Value::as_object)
        .is_some_and(|plugins| {
            plugins.iter().any(|(key, on)| {
                (key == "ironmem" || key.starts_with("ironmem@")) && on.as_bool() == Some(true)
            })
        })
}

/// Every `command` string under `hooks.<event>[].hooks[]`, skipping anything
/// not shaped like that rather than failing.
fn commands_for(doc: &Value, event: &str) -> Vec<String> {
    doc.get("hooks")
        .and_then(|h| h.get(event))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// True when `command` runs `ironmem hook <hook>`, directly, through the
/// plugin's `ironmem-mcp.sh` wrapper, or through its `ironmem-hook.sh` script.
/// Tokens are whitespace-split and matched on their basename, so any install
/// path counts.
fn runs_hook(command: &str, hook: &str) -> bool {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    tokens.iter().enumerate().any(|(i, token)| {
        let base = token.rsplit('/').next().unwrap_or(token);
        match base {
            "ironmem" | "ironmem-mcp.sh" => {
                tokens.get(i + 1) == Some(&"hook") && tokens.get(i + 2) == Some(&hook)
            }
            "ironmem-hook.sh" => tokens.get(i + 1) == Some(&hook),
            _ => false,
        }
    })
}

fn uses_plugin_wrapper(command: &str) -> bool {
    command.split_whitespace().any(|token| {
        let base = token.rsplit('/').next().unwrap_or(token);
        base == "ironmem-hook.sh" || base == "ironmem-mcp.sh"
    })
}

/// The mode a hook command will run in, and where that came from. Order:
/// inline `IRONMEM_MCP_MODE=` > settings.json `env` > the inherited
/// environment > the plugin wrapper's trusted default > read-only.
fn hook_mode(
    command: &str,
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> (Result<McpAccessMode, String>, ModeSource) {
    let inline = command
        .split_whitespace()
        .find_map(|token| token.strip_prefix("IRONMEM_MCP_MODE="))
        .map(|v| v.trim_matches(|c| c == '"' || c == '\''));
    let (raw, source) = match (inline, settings_env, process_env) {
        (Some(v), _, _) => (v, ModeSource::Inline),
        (None, Some(v), _) => (v, ModeSource::SettingsEnv),
        (None, None, Some(v)) => (v, ModeSource::Environment),
        (None, None, None) if uses_plugin_wrapper(command) => {
            return (Ok(McpAccessMode::Trusted), ModeSource::PluginWrapper)
        }
        (None, None, None) => return (Ok(McpAccessMode::ReadOnly), ModeSource::Default),
    };
    (McpAccessMode::parse(raw).map_err(|e| e.to_string()), source)
}

fn hooks_check(path: &Path, missing: &[&str]) -> Check {
    if missing.is_empty() {
        return Check::new(
            "hooks",
            CheckStatus::Ok,
            format!(
                "Claude Code hooks: all {} wired in {}",
                PLUGIN_HOOKS.len(),
                path.display()
            ),
        );
    }
    let effects: Vec<&str> = missing
        .iter()
        .map(|event| match *event {
            "SessionStart" => "no memory context at session start",
            "UserPromptSubmit" => "prompt recall is off",
            "Stop" => "no mining when a session stops",
            "PreCompact" => "no mining before compaction",
            _ => "unknown hook",
        })
        .collect();
    Check::new(
        "hooks",
        CheckStatus::Warn,
        format!(
            "Claude Code hooks: missing {} in {} ({})",
            missing.join(", "),
            path.display(),
            effects.join("; ")
        ),
    )
    .with_hint(INSTALL_HINT)
}

fn no_miner_check() -> Check {
    Check::new(
        "hook_mining",
        CheckStatus::Warn,
        "no Stop or PreCompact hook is wired: transcripts are not mined",
    )
    .with_hint(INSTALL_HINT)
}

fn mining_check(
    wired: &[(&'static str, String)],
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> Check {
    let miners: Vec<&(&'static str, String)> = wired
        .iter()
        .filter(|(event, _)| MINING_EVENTS.contains(event))
        .collect();
    if miners.is_empty() {
        return no_miner_check();
    }
    let mut cannot = Vec::new();
    for (event, command) in miners {
        match hook_mode(command, settings_env, process_env) {
            (Ok(McpAccessMode::Trusted), _) => {}
            (Ok(mode), source) => cannot.push(format!(
                "{event} runs {} ({})",
                mode_label(mode),
                source_label(source)
            )),
            (Err(e), source) => cannot.push(format!(
                "{event} fails to start: {e} ({})",
                source_label(source)
            )),
        }
    }
    if cannot.is_empty() {
        Check::new(
            "hook_mining",
            CheckStatus::Ok,
            "Stop/PreCompact hooks run trusted: transcripts are mined",
        )
    } else {
        Check::new(
            "hook_mining",
            CheckStatus::Warn,
            format!(
                "hooks cannot mine: {}. Without write access the hook skips mining, so new transcripts are not mined",
                cannot.join("; ")
            ),
        )
        .with_hint(READ_ONLY_HINT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with_settings(settings: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(dir.path().join(".claude").join("settings.json"), settings).unwrap();
        dir
    }

    fn settings_with(commands: &[(&str, &str)]) -> String {
        let mut hooks = serde_json::Map::new();
        for (event, cmd) in commands {
            hooks.insert(
                (*event).to_string(),
                serde_json::json!([{ "hooks": [{ "type": "command", "command": cmd }] }]),
            );
        }
        serde_json::json!({ "hooks": hooks }).to_string()
    }

    const ALL_FOUR_BARE: [(&str, &str); 4] = [
        (
            "SessionStart",
            "~/.ironrace/bin/ironmem hook session-start --harness claude-code",
        ),
        (
            "UserPromptSubmit",
            "~/.ironrace/bin/ironmem hook user-prompt-submit --harness claude-code",
        ),
        (
            "Stop",
            "~/.ironrace/bin/ironmem hook stop --harness claude-code",
        ),
        (
            "PreCompact",
            "~/.ironrace/bin/ironmem hook precompact --harness claude-code",
        ),
    ];

    #[test]
    fn plugin_hooks_table_matches_the_plugin_hooks_json() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.claude-plugin/hooks/hooks.json"
        );
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let hooks = doc["hooks"].as_object().unwrap();
        assert_eq!(
            hooks.len(),
            PLUGIN_HOOKS.len(),
            "event count drifted from hooks.json"
        );
        for (event, name) in PLUGIN_HOOKS {
            let cmd = hooks[event][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(
                runs_hook(cmd, name),
                "{event}: `{cmd}` should run hook {name}"
            );
        }
    }

    #[test]
    fn runs_hook_matches_direct_wrapper_and_plugin_script_forms() {
        assert!(runs_hook(
            "~/.ironrace/bin/ironmem hook stop --harness claude-code",
            "stop"
        ));
        assert!(runs_hook("ironmem hook stop", "stop"));
        assert!(runs_hook(
            "IRONMEM_MCP_MODE=trusted /x/ironmem hook precompact",
            "precompact"
        ));
        assert!(runs_hook(
            "/p/bin/ironmem-mcp.sh hook stop --harness claude-code",
            "stop"
        ));
        assert!(runs_hook(
            "bash ${CLAUDE_PLUGIN_ROOT}/hooks/ironmem-hook.sh session-start",
            "session-start"
        ));
        assert!(!runs_hook("~/.ironrace/bin/ironmem hook stopx", "stop"));
        assert!(!runs_hook("~/.claude/scripts/reply-rules.sh", "stop"));
        assert!(!runs_hook("ironmem serve --connect /s.sock", "stop"));
        assert!(!runs_hook("notironmem hook stop", "stop"));
    }

    #[test]
    fn hook_mode_precedence_inline_then_settings_env_then_process_env_then_default() {
        use McpAccessMode::*;
        let bare = "~/.ironrace/bin/ironmem hook stop";
        assert_eq!(
            hook_mode(bare, None, None),
            (Ok(ReadOnly), ModeSource::Default)
        );
        assert_eq!(
            hook_mode(bare, None, Some("trusted")),
            (Ok(Trusted), ModeSource::Environment)
        );
        assert_eq!(
            hook_mode(bare, Some("trusted"), Some("read-only")),
            (Ok(Trusted), ModeSource::SettingsEnv)
        );
        assert_eq!(
            hook_mode(
                "IRONMEM_MCP_MODE='read-only' ironmem hook stop",
                Some("trusted"),
                Some("trusted")
            ),
            (Ok(ReadOnly), ModeSource::Inline)
        );
        assert_eq!(
            hook_mode("bash /p/hooks/ironmem-hook.sh stop", None, None),
            (Ok(Trusted), ModeSource::PluginWrapper)
        );
        assert_eq!(
            hook_mode(
                "bash /p/hooks/ironmem-hook.sh stop",
                None,
                Some("read-only")
            ),
            (Ok(ReadOnly), ModeSource::Environment),
            "the wrapper only defaults; an inherited mode wins"
        );
        assert!(
            hook_mode("IRONMEM_MCP_MODE=bogus ironmem hook stop", None, None)
                .0
                .is_err()
        );
    }

    #[test]
    fn all_four_bare_hooks_are_wired_but_cannot_mine() {
        let home = home_with_settings(&settings_with(&ALL_FOUR_BARE));
        let checks = claude_hook_checks(Some(home.path()), None);
        assert_eq!(
            checks.iter().map(|c| c.name).collect::<Vec<_>>(),
            ["hooks", "hook_mining"]
        );
        assert_eq!(checks[0].status, CheckStatus::Ok, "{}", checks[0].summary);
        assert!(
            checks[0].summary.contains("all 4 wired"),
            "{}",
            checks[0].summary
        );
        assert_eq!(checks[1].status, CheckStatus::Warn);
        assert!(
            checks[1].summary.contains("Stop runs read-only"),
            "{}",
            checks[1].summary
        );
        assert!(
            checks[1].summary.contains("PreCompact runs read-only"),
            "{}",
            checks[1].summary
        );
        assert!(
            checks[1].summary.contains("not mined"),
            "{}",
            checks[1].summary
        );
        assert!(checks[1].hint.as_deref().unwrap().contains("#418"));
    }

    #[test]
    fn trusted_inline_hooks_mine() {
        let cmds = ALL_FOUR_BARE.map(|(e, c)| (e, format!("IRONMEM_MCP_MODE=trusted {c}")));
        let refs: Vec<(&str, &str)> = cmds.iter().map(|(e, c)| (*e, c.as_str())).collect();
        let home = home_with_settings(&settings_with(&refs));
        let checks = claude_hook_checks(Some(home.path()), None);
        assert_eq!(checks[1].status, CheckStatus::Ok, "{}", checks[1].summary);
        assert!(checks[1].summary.contains("transcripts are mined"));
    }

    #[test]
    fn missing_hooks_are_named_with_their_effect() {
        let home = home_with_settings(&settings_with(&[ALL_FOUR_BARE[2]]));
        let checks = claude_hook_checks(Some(home.path()), None);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        let s = &checks[0].summary;
        assert!(
            s.contains("missing SessionStart, UserPromptSubmit, PreCompact"),
            "{s}"
        );
        assert!(s.contains("prompt recall is off"), "{s}");
        assert!(checks[0]
            .hint
            .as_deref()
            .unwrap()
            .contains("install-ironmem.sh"));
    }

    #[test]
    fn no_miner_wired_warns() {
        let home = home_with_settings(&settings_with(&ALL_FOUR_BARE[..2]));
        let checks = claude_hook_checks(Some(home.path()), None);
        assert_eq!(checks[1].status, CheckStatus::Warn);
        assert!(checks[1]
            .summary
            .contains("no Stop or PreCompact hook is wired"));
    }

    #[test]
    fn enabled_plugin_supplies_the_hooks() {
        let home = home_with_settings(r#"{"enabledPlugins": {"ironmem@ironrace": true}}"#);
        let checks = claude_hook_checks(Some(home.path()), None);
        assert_eq!(checks[0].status, CheckStatus::Ok);
        assert!(checks[0].summary.contains("enabled ironmem plugin"));
        assert_eq!(checks[1].status, CheckStatus::Ok);
        // A disabled plugin entry is not a plugin install.
        let off = home_with_settings(r#"{"enabledPlugins": {"ironmem@ironrace": false}}"#);
        assert_eq!(
            claude_hook_checks(Some(off.path()), None)[0].status,
            CheckStatus::Warn
        );
    }

    #[test]
    fn absent_and_broken_settings() {
        // No ~/.claude at all: Claude Code isn't used here, so both are Info skips.
        let bare = tempfile::tempdir().unwrap();
        let c = claude_hook_checks(Some(bare.path()), None);
        assert_eq!(
            (c[0].status, c[1].status),
            (CheckStatus::Info, CheckStatus::Info)
        );
        // ~/.claude without settings.json: nothing is wired.
        let no_file = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(no_file.path().join(".claude")).unwrap();
        let c = claude_hook_checks(Some(no_file.path()), None);
        assert_eq!(
            (c[0].status, c[1].status),
            (CheckStatus::Warn, CheckStatus::Warn)
        );
        // Malformed JSON: hooks warns, mining is skipped as Info.
        let broken = home_with_settings("{not json");
        let c = claude_hook_checks(Some(broken.path()), None);
        assert_eq!(
            (c[0].status, c[1].status),
            (CheckStatus::Warn, CheckStatus::Info)
        );
        // Unknown home.
        let c = claude_hook_checks(None, None);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].status, CheckStatus::Info);
    }

    #[test]
    fn hooks_with_odd_shapes_do_not_panic() {
        let home = home_with_settings(
            r#"{"hooks": {"Stop": {"not": "an array"}, "PreCompact": [{"hooks": "x"}, 3]}}"#,
        );
        let checks = claude_hook_checks(Some(home.path()), None);
        assert_eq!(checks[0].status, CheckStatus::Warn);
    }
}

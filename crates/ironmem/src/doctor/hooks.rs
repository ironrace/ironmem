//! The Claude Code hook doctor checks (#377): `hooks` and `hook_mining`.
//!
//! A plugin install gets its four hooks from `.claude-plugin/hooks/hooks.json`
//! and runs them through a wrapper that defaults to trusted mode. A non-plugin
//! install gets whatever `~/.claude/settings.json` holds, and hook commands
//! there default to read-only, which skips mining. Neither gap errors at run
//! time, so doctor is where it has to show up. Diagnose-only: never writes.

use std::path::Path;

use serde_json::Value;

use super::hook_command::{
    best_result, hook_mode, mode_label, runs_hook, source_label, ModeError, ModeResult, ModeSource,
};
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

const INSTALL_HINT: &str = "re-run scripts/install-ironmem.sh to wire the missing hooks, \
or install the ironmem Claude Code plugin";

const READ_ONLY_HINT: &str = "expected outside a plugin install: MCP writes and prompt recall \
still work. A cheaper way to mine without the plugin is tracked in #418. To mine now, prefix the \
Stop and PreCompact commands with IRONMEM_MCP_MODE=trusted; each run then loads the embedding \
model and rebuilds the vector index";

const INVALID_MODE_HINT: &str = "set IRONMEM_MCP_MODE to trusted, read-only or restricted, \
or remove it; a hook with any other value exits at startup";

const DYNAMIC_MODE_HINT: &str = "set a literal IRONMEM_MCP_MODE value (trusted, read-only or \
restricted) in the hook command so doctor can check it";

const DUPLICATE_HINT: &str = "remove the hand-wired ironmem entries from settings.json; \
the enabled plugin already supplies these hooks";

const HOME_UNKNOWN: &str = "hook mining: skipped (home directory unknown)";

/// The command shape of a plugin hook, used to work out the mode it runs in.
const PLUGIN_HOOK_COMMAND: &str = "bash ${CLAUDE_PLUGIN_ROOT}/hooks/ironmem-hook.sh";

/// Plugin hook events that settings.json wires, with every command matching each.
type WiredHooks = Vec<(&'static str, Vec<String>)>;

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
    Plugin {
        /// Plugin hook events that settings.json also wires by hand (they
        /// would run twice).
        hand_wired: Vec<&'static str>,
        /// `env.IRONMEM_MCP_MODE` from settings.json, if set.
        settings_env_mode: Option<String>,
    },
    /// Hooks registered by hand or by scripts/install-ironmem.sh.
    Manual {
        /// The plugin hooks found, in PLUGIN_HOOKS order.
        wired: WiredHooks,
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
        return skipped(
            "Claude Code hooks: home directory unknown, skipped",
            HOME_UNKNOWN,
        );
    };
    let claude_dir = home.join(".claude");
    let path = claude_dir.join("settings.json");
    match detect_hook_wiring(&claude_dir) {
        HookWiring::NoClaude => skipped(
            &format!(
                "Claude Code hooks: {} not found, skipped",
                claude_dir.display()
            ),
            "hook mining: skipped (no Claude Code config)",
        ),
        HookWiring::NoSettings => vec![
            Check::new(
                "hooks",
                CheckStatus::Warn,
                format!(
                    "Claude Code hooks: none wired ({} does not exist): \
                     prompt recall and transcript mining are off",
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
            Check::new(
                "hook_mining",
                CheckStatus::Info,
                "hook mining: skipped (settings.json unreadable)",
            ),
        ],
        HookWiring::Plugin {
            hand_wired,
            settings_env_mode,
        } => {
            let wired: WiredHooks = PLUGIN_HOOKS
                .iter()
                .map(|(event, name)| (*event, vec![format!("{PLUGIN_HOOK_COMMAND} {name}")]))
                .collect();
            let failures = startup_failures(&wired, settings_env_mode.as_deref(), process_env_mode);
            vec![
                plugin_hooks_check(&hand_wired, &failures),
                mining_check(&wired, settings_env_mode.as_deref(), process_env_mode),
            ]
        }
        HookWiring::Manual {
            wired,
            missing,
            settings_env_mode,
        } => {
            let failures = startup_failures(&wired, settings_env_mode.as_deref(), process_env_mode);
            vec![
                hooks_check(&path, &missing, &failures),
                mining_check(&wired, settings_env_mode.as_deref(), process_env_mode),
            ]
        }
    }
}

fn skipped(hooks_summary: &str, mining_summary: &str) -> Vec<Check> {
    vec![
        Check::new("hooks", CheckStatus::Info, hooks_summary),
        Check::new("hook_mining", CheckStatus::Info, mining_summary),
    ]
}

fn plugin_hook_name(event: &str) -> &'static str {
    PLUGIN_HOOKS
        .iter()
        .find(|(e, _)| *e == event)
        .map_or("", |(_, name)| name)
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
    let mut wired = Vec::new();
    let mut missing = Vec::new();
    for (event, hook) in PLUGIN_HOOKS {
        let matching: Vec<String> = commands_for(&doc, event)
            .into_iter()
            .filter(|c| runs_hook(c, hook))
            .collect();
        if matching.is_empty() {
            missing.push(event);
        } else {
            wired.push((event, matching));
        }
    }
    let settings_env_mode = doc
        .pointer("/env/IRONMEM_MCP_MODE")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if plugin_enabled(&doc) {
        let hand_wired = wired.iter().map(|(event, _)| *event).collect();
        return HookWiring::Plugin {
            hand_wired,
            settings_env_mode,
        };
    }
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

/// The one mode result reported for `event`: each matching command's
/// [`hook_mode`], reduced by [`best_result`] (a trusted run that doesn't
/// depend on doctor's shell, else any trusted run, else the first command in
/// settings order that doesn't fail to start, else the first command).
fn event_mode(
    event: &str,
    commands: &[String],
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> ModeResult {
    let hook = plugin_hook_name(event);
    let results: Vec<ModeResult> = commands
        .iter()
        .map(|c| hook_mode(c, hook, settings_env, process_env))
        .collect();
    best_result(&results)
}

/// What is lost when `event`'s hook does not run.
fn event_effect(event: &str) -> &'static str {
    match event {
        "SessionStart" => "no memory context at session start",
        "UserPromptSubmit" => "prompt recall is off",
        "Stop" => "no mining when a session stops",
        "PreCompact" => "no mining before compaction",
        _ => "unknown hook",
    }
}

/// `event` exits at startup because `Config::load` rejects `error`'s mode.
fn startup_failure(event: &str, error: &str, source: ModeSource) -> String {
    format!(
        "{event} fails to start: {error} ({}), so {}",
        source_label(source),
        event_effect(event)
    )
}

/// Non-mining hooks (SessionStart, UserPromptSubmit) whose mode is invalid:
/// `Config::load` fails, so they exit at startup. Each entry names its own
/// effect: no memory context at session start for SessionStart, prompt recall
/// off for UserPromptSubmit.
fn startup_failures(
    wired: &WiredHooks,
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> Vec<String> {
    wired
        .iter()
        .filter(|(event, _)| !MINING_EVENTS.contains(event))
        .filter_map(|(event, commands)| {
            match event_mode(event, commands, settings_env, process_env) {
                (Err(ModeError::Invalid(e)), source) => Some(startup_failure(event, &e, source)),
                _ => None,
            }
        })
        .collect()
}

fn plugin_hooks_check(hand_wired: &[&str], failures: &[String]) -> Check {
    if hand_wired.is_empty() && failures.is_empty() {
        return Check::new(
            "hooks",
            CheckStatus::Ok,
            "Claude Code hooks: supplied by the enabled ironmem plugin",
        );
    }
    let mut problems = Vec::new();
    let mut hints = Vec::new();
    if !hand_wired.is_empty() {
        problems.push(format!(
            "the enabled ironmem plugin supplies them and settings.json wires {} by hand \
             too, so those hooks run twice",
            hand_wired.join(", ")
        ));
        hints.push(DUPLICATE_HINT);
    }
    if !failures.is_empty() {
        problems.push(failures.join("; "));
        hints.push(INVALID_MODE_HINT);
    }
    Check::new(
        "hooks",
        CheckStatus::Warn,
        format!("Claude Code hooks: {}", problems.join("; ")),
    )
    .with_hint(hints.join("; "))
}

fn hooks_check(path: &Path, missing: &[&str], failures: &[String]) -> Check {
    if missing.is_empty() && failures.is_empty() {
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
    let mut problems = Vec::new();
    let mut hints = Vec::new();
    if !missing.is_empty() {
        let effects: Vec<&str> = missing.iter().map(|event| event_effect(event)).collect();
        problems.push(format!(
            "missing {} in {} ({})",
            missing.join(", "),
            path.display(),
            effects.join("; ")
        ));
        hints.push(INSTALL_HINT);
    }
    if !failures.is_empty() {
        problems.push(failures.join("; "));
        hints.push(INVALID_MODE_HINT);
    }
    Check::new(
        "hooks",
        CheckStatus::Warn,
        format!("Claude Code hooks: {}", problems.join("; ")),
    )
    .with_hint(hints.join("; "))
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
    wired: &WiredHooks,
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> Check {
    let miners: Vec<&(&'static str, Vec<String>)> = wired
        .iter()
        .filter(|(event, _)| MINING_EVENTS.contains(event))
        .collect();
    if miners.is_empty() {
        return no_miner_check();
    }
    let mut cannot = Vec::new();
    let mut failing = Vec::new();
    let mut unknown = Vec::new();
    let mut from_shell = false;
    let names: Vec<&str> = miners.iter().map(|(event, _)| *event).collect();
    for (event, commands) in miners {
        match event_mode(event, commands, settings_env, process_env) {
            (Ok(McpAccessMode::Trusted), source) => {
                from_shell |= source == ModeSource::Environment;
            }
            (Ok(mode), source) => cannot.push(format!(
                "{event} runs {} ({})",
                mode_label(mode),
                source_label(source)
            )),
            (Err(ModeError::Invalid(e)), source) => {
                failing.push(startup_failure(event, &e, source));
            }
            (Err(ModeError::Unknown(raw)), _) => unknown.push(format!(
                "{event} mode cannot be determined statically (IRONMEM_MCP_MODE={raw})"
            )),
        }
    }
    if !failing.is_empty() {
        let mut parts = failing;
        parts.extend(unknown);
        parts.extend(cannot);
        return Check::new(
            "hook_mining",
            CheckStatus::Warn,
            format!(
                "hooks cannot mine: {}. A hook with an invalid IRONMEM_MCP_MODE exits at startup",
                parts.join("; ")
            ),
        )
        .with_hint(INVALID_MODE_HINT);
    }
    if !unknown.is_empty() {
        let mut parts = unknown;
        parts.extend(cannot);
        return Check::new(
            "hook_mining",
            CheckStatus::Warn,
            format!("hooks may not mine: {}", parts.join("; ")),
        )
        .with_hint(DYNAMIC_MODE_HINT);
    }
    if !cannot.is_empty() {
        return Check::new(
            "hook_mining",
            CheckStatus::Warn,
            format!(
                "hooks cannot mine: {}. Without write access the hook skips mining, so new \
                 transcripts are not mined",
                cannot.join("; ")
            ),
        )
        .with_hint(READ_ONLY_HINT);
    }
    let subject = match names.as_slice() {
        [one] => format!("{one} hook runs"),
        many => format!("{} hooks run", many.join(" and ")),
    };
    let summary = if from_shell {
        format!(
            "{subject} trusted (inherited from IRONMEM_MCP_MODE in this shell; \
             Claude Code must be launched with it): transcripts are mined"
        )
    } else {
        format!("{subject} trusted: transcripts are mined")
    };
    Check::new("hook_mining", CheckStatus::Ok, summary)
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
            assert_eq!(cmd, format!("{PLUGIN_HOOK_COMMAND} {name}"), "{event}");
        }
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
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        assert_eq!(checks[1].status, CheckStatus::Warn, "{}", checks[1].summary);
        assert!(checks[1]
            .summary
            .contains("no Stop or PreCompact hook is wired"));
    }

    const PLUGIN_ON: &str = r#"{"enabledPlugins": {"ironmem@ironrace": true}"#;

    #[test]
    fn plugin_mining_respects_settings_env_and_process_env() {
        let ro = home_with_settings(&format!(
            r#"{PLUGIN_ON}, "env": {{"IRONMEM_MCP_MODE": "read-only"}}}}"#
        ));
        let c = claude_hook_checks(Some(ro.path()), None);
        assert_eq!(c[0].status, CheckStatus::Ok);
        assert_eq!(c[1].status, CheckStatus::Warn, "{}", c[1].summary);
        assert!(
            c[1].summary.contains("Stop runs read-only"),
            "{}",
            c[1].summary
        );
        assert!(
            c[1].summary.contains("set by env in settings.json"),
            "{}",
            c[1].summary
        );

        let plain = home_with_settings(&format!("{PLUGIN_ON}}}"));
        let c = claude_hook_checks(Some(plain.path()), Some("read-only"));
        assert_eq!(c[1].status, CheckStatus::Warn, "{}", c[1].summary);
        assert!(
            c[1].summary.contains("inherited from IRONMEM_MCP_MODE"),
            "{}",
            c[1].summary
        );

        let c = claude_hook_checks(Some(plain.path()), None);
        assert_eq!(c[1].status, CheckStatus::Ok, "{}", c[1].summary);
    }

    #[test]
    fn invalid_mode_is_a_startup_failure_not_a_read_only_story() {
        let inline = ALL_FOUR_BARE.map(|(e, c)| (e, format!("IRONMEM_MCP_MODE=bogus {c}")));
        let refs: Vec<(&str, &str)> = inline.iter().map(|(e, c)| (*e, c.as_str())).collect();
        let home = home_with_settings(&settings_with(&refs));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(c[1].status, CheckStatus::Warn);
        assert!(c[1].summary.contains("fails to start"), "{}", c[1].summary);
        // Stop and PreCompact never recall, so only their mining effect is named.
        assert!(
            c[1].summary.contains("no mining when a session stops"),
            "{}",
            c[1].summary
        );
        assert!(
            c[1].summary.contains("no mining before compaction"),
            "{}",
            c[1].summary
        );
        assert!(!c[1].summary.contains("recall"), "{}", c[1].summary);
        assert!(!c[1].summary.contains("skips mining"), "{}", c[1].summary);
        let hint = c[1].hint.as_deref().unwrap();
        assert!(hint.contains("trusted, read-only or restricted"), "{hint}");
        assert!(!hint.contains("prompt recall still work"), "{hint}");

        let mut doc: Value = serde_json::from_str(&settings_with(&ALL_FOUR_BARE)).unwrap();
        doc["env"] = serde_json::json!({"IRONMEM_MCP_MODE": "bogus"});
        let home = home_with_settings(&doc.to_string());
        let c = claude_hook_checks(Some(home.path()), None);
        assert!(c[1].summary.contains("fails to start"), "{}", c[1].summary);
        assert!(
            c[1].summary.contains("set by env in settings.json"),
            "{}",
            c[1].summary
        );
        assert!(c[1]
            .hint
            .as_deref()
            .unwrap()
            .contains("trusted, read-only or restricted"));
    }

    #[test]
    fn invalid_and_read_only_miners_are_reported_separately() {
        let doc = settings_with(&[
            ("Stop", "IRONMEM_MCP_MODE=bogus ironmem hook stop"),
            (
                "PreCompact",
                "IRONMEM_MCP_MODE=read-only ironmem hook precompact",
            ),
        ]);
        let home = home_with_settings(&doc);
        let c = claude_hook_checks(Some(home.path()), None);
        let s = &c[1].summary;
        assert!(s.contains("Stop fails to start"), "{s}");
        assert!(s.contains("PreCompact runs read-only"), "{s}");
        assert!(!s.contains("Stop runs read-only"), "{s}");
        assert!(c[1]
            .hint
            .as_deref()
            .unwrap()
            .contains("trusted, read-only or restricted"));
    }

    #[test]
    fn trusted_only_from_this_shell_says_so() {
        let home = home_with_settings(&settings_with(&ALL_FOUR_BARE));
        let c = claude_hook_checks(Some(home.path()), Some("trusted"));
        assert_eq!(c[1].status, CheckStatus::Ok, "{}", c[1].summary);
        assert!(c[1]
            .summary
            .contains("inherited from IRONMEM_MCP_MODE in this shell"));
        assert!(c[1]
            .summary
            .contains("Claude Code must be launched with it"));
    }

    #[test]
    fn plugin_plus_hand_wired_hooks_run_twice() {
        let mut doc: Value = serde_json::from_str(&settings_with(&ALL_FOUR_BARE[2..])).unwrap();
        doc["enabledPlugins"] = serde_json::json!({"ironmem@ironrace": true});
        let home = home_with_settings(&doc.to_string());
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(c[0].status, CheckStatus::Warn, "{}", c[0].summary);
        assert!(c[0].summary.contains("run twice"), "{}", c[0].summary);
        assert!(
            c[0].summary.contains("Stop, PreCompact"),
            "{}",
            c[0].summary
        );
        assert!(c[0]
            .hint
            .as_deref()
            .unwrap()
            .contains("remove the hand-wired"));
    }

    #[test]
    fn restricted_mode_cannot_mine() {
        let cmd = "IRONMEM_MCP_MODE=restricted ironmem hook stop";
        let home = home_with_settings(&settings_with(&[("Stop", cmd)]));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(c[1].status, CheckStatus::Warn);
        assert!(
            c[1].summary.contains("Stop runs restricted"),
            "{}",
            c[1].summary
        );
    }

    fn mining_of(settings: &str, process_env: Option<&str>) -> Check {
        let home = home_with_settings(settings);
        claude_hook_checks(Some(home.path()), process_env).remove(1)
    }

    #[test]
    fn any_trusted_command_for_an_event_mines() {
        let restricted_precompact = "IRONMEM_MCP_MODE=restricted ironmem hook precompact";
        let doc = serde_json::json!({"hooks": {
            "Stop": [{"hooks": [
                {"type": "command", "command": "ironmem hook stop"},
                {"type": "command", "command": "IRONMEM_MCP_MODE=trusted ironmem hook stop"}
            ]}],
            "PreCompact": [{"hooks": [
                {"type": "command", "command": "ironmem hook precompact"},
                {"type": "command", "command": restricted_precompact}
            ]}]
        }});
        let c = mining_of(&doc.to_string(), None);
        assert_eq!(c.status, CheckStatus::Warn, "{}", c.summary);
        assert!(
            !c.summary.contains("Stop runs"),
            "Stop has a trusted command: {}",
            c.summary
        );
        assert!(
            c.summary.contains("PreCompact runs read-only"),
            "the first command is reported: {}",
            c.summary
        );
        assert!(!c.summary.contains("restricted"), "{}", c.summary);
    }

    #[test]
    fn empty_mode_follows_the_wrapper_default_or_config_load() {
        let plugin = format!(r#"{PLUGIN_ON}, "env": {{"IRONMEM_MCP_MODE": ""}}}}"#);
        let c = mining_of(&plugin, None);
        assert_eq!(
            c.status,
            CheckStatus::Ok,
            "wrapper treats empty as unset: {}",
            c.summary
        );
        // Config::load parses an empty IRONMEM_MCP_MODE and rejects it.
        let mut doc: Value = serde_json::from_str(&settings_with(&ALL_FOUR_BARE)).unwrap();
        doc["env"] = serde_json::json!({"IRONMEM_MCP_MODE": ""});
        let c = mining_of(&doc.to_string(), None);
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.summary.contains("fails to start"), "{}", c.summary);
    }

    #[test]
    fn dynamic_mode_value_cannot_be_judged() {
        let cmd = "IRONMEM_MCP_MODE=$MODE ironmem hook stop";
        let c = mining_of(&settings_with(&[("Stop", cmd)]), None);
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(
            c.summary.contains("cannot be determined statically"),
            "{}",
            c.summary
        );
        assert!(!c.summary.contains("fails to start"), "{}", c.summary);
        assert!(c.hint.as_deref().unwrap().contains("literal"));
        let quoted = "IRONMEM_MCP_MODE=\"$(cat /m)\" ironmem hook stop";
        let c = mining_of(&settings_with(&[("Stop", quoted)]), None);
        assert!(
            c.summary.contains("cannot be determined statically"),
            "{}",
            c.summary
        );
    }

    #[test]
    fn plugin_with_trusted_shell_does_not_blame_the_shell() {
        let c = mining_of(&format!("{PLUGIN_ON}}}"), Some("trusted"));
        assert_eq!(c.status, CheckStatus::Ok, "{}", c.summary);
        assert!(!c.summary.contains("must be launched"), "{}", c.summary);
    }

    #[test]
    fn invalid_mode_on_a_non_mining_hook_is_reported() {
        let mut cmds: Vec<(&str, String)> = ALL_FOUR_BARE
            .iter()
            .map(|(e, c)| (*e, (*c).to_string()))
            .collect();
        cmds[1].1 = format!("IRONMEM_MCP_MODE=bogus {}", cmds[1].1);
        let refs: Vec<(&str, &str)> = cmds.iter().map(|(e, c)| (*e, c.as_str())).collect();
        let home = home_with_settings(&settings_with(&refs));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(c[0].status, CheckStatus::Warn, "{}", c[0].summary);
        assert!(
            c[0].summary.contains("UserPromptSubmit fails to start"),
            "{}",
            c[0].summary
        );
        assert!(
            !c[0].summary.contains("SessionStart fails"),
            "{}",
            c[0].summary
        );
        assert!(c[0]
            .hint
            .as_deref()
            .unwrap()
            .contains("trusted, read-only or restricted"));

        let mut doc: Value = serde_json::from_str(&settings_with(&ALL_FOUR_BARE[..2])).unwrap();
        doc["env"] = serde_json::json!({"IRONMEM_MCP_MODE": "bogus"});
        let c = claude_hook_checks(Some(home_with_settings(&doc.to_string()).path()), None);
        assert_eq!(c[0].status, CheckStatus::Warn);
        assert!(
            c[0].summary.contains("SessionStart fails to start"),
            "{}",
            c[0].summary
        );
        assert!(
            c[0].summary.contains("UserPromptSubmit fails to start"),
            "{}",
            c[0].summary
        );
        assert!(
            c[0].summary.contains("missing Stop, PreCompact"),
            "{}",
            c[0].summary
        );
    }

    fn all_four_with(event: &str, prefix: &str) -> String {
        let cmds: Vec<(&str, String)> = ALL_FOUR_BARE
            .iter()
            .map(|(e, c)| {
                let c = if *e == event {
                    format!("{prefix} {c}")
                } else {
                    (*c).to_string()
                };
                (*e, c)
            })
            .collect();
        let refs: Vec<(&str, &str)> = cmds.iter().map(|(e, c)| (*e, c.as_str())).collect();
        settings_with(&refs)
    }

    #[test]
    fn an_invalid_session_start_names_its_own_effect_not_recall() {
        let home = home_with_settings(&all_four_with("SessionStart", "IRONMEM_MCP_MODE=bogus"));
        let c = claude_hook_checks(Some(home.path()), None);
        let s = &c[0].summary;
        assert_eq!(c[0].status, CheckStatus::Warn, "{s}");
        assert!(s.contains("SessionStart fails to start"), "{s}");
        assert!(s.contains("no memory context at session start"), "{s}");
        assert!(!s.contains("prompt recall is off"), "{s}");
    }

    #[test]
    fn an_invalid_user_prompt_submit_turns_recall_off() {
        let home = home_with_settings(&all_four_with("UserPromptSubmit", "IRONMEM_MCP_MODE=bogus"));
        let c = claude_hook_checks(Some(home.path()), None);
        let s = &c[0].summary;
        assert_eq!(c[0].status, CheckStatus::Warn, "{s}");
        assert!(s.contains("UserPromptSubmit fails to start"), "{s}");
        assert!(s.contains("prompt recall is off"), "{s}");
        assert!(!s.contains("no memory context at session start"), "{s}");
    }

    #[test]
    fn a_valid_command_is_reported_over_an_invalid_one_for_the_same_event() {
        let doc = serde_json::json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "IRONMEM_MCP_MODE=bogus ironmem hook stop"},
            {"type": "command", "command": "ironmem hook stop"}
        ]}]}});
        let c = mining_of(&doc.to_string(), None);
        assert_eq!(c.status, CheckStatus::Warn, "{}", c.summary);
        assert!(c.summary.contains("Stop runs read-only"), "{}", c.summary);
        assert!(!c.summary.contains("fails to start"), "{}", c.summary);
    }

    #[test]
    fn a_trusted_command_that_needs_no_shell_beats_one_that_does() {
        let doc = serde_json::json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "ironmem hook stop"},
            {"type": "command", "command": "IRONMEM_MCP_MODE=trusted ironmem hook stop"}
        ]}]}});
        let c = mining_of(&doc.to_string(), Some("trusted"));
        assert_eq!(c.status, CheckStatus::Ok, "{}", c.summary);
        assert!(!c.summary.contains("must be launched"), "{}", c.summary);
    }

    #[test]
    fn plugin_with_an_invalid_settings_env_mode_fails_every_hook() {
        let home = home_with_settings(&format!(
            r#"{PLUGIN_ON}, "env": {{"IRONMEM_MCP_MODE": "bogus"}}}}"#
        ));
        let c = claude_hook_checks(Some(home.path()), None);
        let s = &c[0].summary;
        assert_eq!(c[0].status, CheckStatus::Warn, "{s}");
        assert!(s.contains("SessionStart fails to start"), "{s}");
        assert!(s.contains("UserPromptSubmit fails to start"), "{s}");
        assert!(s.contains("prompt recall is off"), "{s}");
        assert!(c[0]
            .hint
            .as_deref()
            .unwrap()
            .contains("trusted, read-only or restricted"));
    }

    #[test]
    fn the_trusted_summary_names_only_the_wired_miners() {
        let cmd = "IRONMEM_MCP_MODE=trusted ironmem hook stop";
        let c = mining_of(&settings_with(&[("Stop", cmd)]), None);
        assert_eq!(c.status, CheckStatus::Ok, "{}", c.summary);
        assert!(
            c.summary.starts_with("Stop hook runs trusted"),
            "{}",
            c.summary
        );
        assert!(!c.summary.contains("PreCompact"), "{}", c.summary);

        let both = ALL_FOUR_BARE.map(|(e, c)| (e, format!("IRONMEM_MCP_MODE=trusted {c}")));
        let refs: Vec<(&str, &str)> = both.iter().map(|(e, c)| (*e, c.as_str())).collect();
        let c = mining_of(&settings_with(&refs), None);
        assert!(
            c.summary
                .starts_with("Stop and PreCompact hooks run trusted"),
            "{}",
            c.summary
        );
    }

    #[test]
    fn a_later_trusted_assignment_does_not_make_the_hook_mine() {
        let later = "ironmem hook stop; IRONMEM_MCP_MODE=trusted true";
        let c = mining_of(&settings_with(&[("Stop", later)]), None);
        assert_eq!(c.status, CheckStatus::Warn, "{}", c.summary);
        assert!(c.summary.contains("Stop runs read-only"), "{}", c.summary);
    }
}

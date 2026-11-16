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
    best_result, hook_mode_with_raw, mode_label, runs_hook, source_label, ModeError, ModeSource,
    RawModeResult,
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

/// The hint for bare manual miners that run read-only, naming those `events`.
fn read_only_hint(events: &[&str]) -> String {
    let commands = if events.len() == 1 {
        "command"
    } else {
        "commands"
    };
    format!(
        "expected outside a plugin install: MCP writes and prompt recall still work. A cheaper \
         way to mine without the plugin is tracked in #418. To mine now, prefix the {} {commands} \
         with IRONMEM_MCP_MODE=trusted; each run then loads the embedding model and rebuilds the \
         vector index",
        events.join(" and ")
    )
}

/// Separates several hints on one line; no hint text contains it.
const HINT_JOINER: &str = " | ";

const PLUGIN_OVERRIDE_HINT: &str = "remove or unset the IRONMEM_MCP_MODE override (settings.json \
env or your shell); the plugin wrapper defaults to trusted";

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
        /// Plugin hooks that settings.json also wires by hand (they would
        /// run twice), with their commands.
        hand_wired: WiredHooks,
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
            let env = settings_env_mode.as_deref();
            let mut failures = startup_failures(&wired, env, process_env_mode);
            failures.extend(hand_wired_failures(&hand_wired, env, process_env_mode));
            let duplicated: Vec<&str> = hand_wired.iter().map(|(event, _)| *event).collect();
            // A hand-wired copy runs as well as the plugin's, so it can mine too.
            let candidates: WiredHooks = wired
                .iter()
                .map(|(event, commands)| {
                    let extra = hand_wired.iter().filter(|(e, _)| e == event);
                    let all = commands.iter().chain(extra.flat_map(|(_, c)| c)).cloned();
                    (*event, all.collect())
                })
                .collect();
            vec![
                plugin_hooks_check(&duplicated, &failures),
                mining_check(&candidates, env, process_env_mode, true),
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
                mining_check(
                    &wired,
                    settings_env_mode.as_deref(),
                    process_env_mode,
                    false,
                ),
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
        return HookWiring::Plugin {
            hand_wired: wired,
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

/// The one mode result reported for `event`, with its raw value: each
/// matching command's [`hook_mode_with_raw`], reduced by [`best_result`]
/// (trusted without doctor's shell, then trusted via the shell, then unknown,
/// then read-only or restricted, then invalid; settings order breaks ties).
fn event_mode(
    event: &str,
    commands: &[String],
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> RawModeResult {
    let hook = plugin_hook_name(event);
    let results = commands
        .iter()
        .map(|c| hook_mode_with_raw(c, hook, settings_env, process_env))
        .collect();
    best_result(results)
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

/// Why `Config::load` rejects the raw mode value `raw`, with it quoted.
fn invalid_reason(raw: &str) -> String {
    format!("IRONMEM_MCP_MODE={raw:?} is not trusted, read-only or restricted")
}

/// `event` exits at startup because `Config::load` rejects its raw mode `raw`.
fn startup_failure(event: &str, raw: &str, source: ModeSource) -> String {
    format!(
        "{event} fails to start: {} ({}), so {}",
        invalid_reason(raw),
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
                ((Err(ModeError::Invalid(e)), source), _) => {
                    Some(startup_failure(event, &e, source))
                }
                _ => None,
            }
        })
        .collect()
}

/// Hand-wired copies of plugin hooks whose mode is invalid. The plugin's own
/// copy still starts, so no effect is named and the copy is not said to fail
/// "also": only the copy fails at startup.
fn hand_wired_failures(
    hand_wired: &WiredHooks,
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> Vec<String> {
    hand_wired
        .iter()
        .filter_map(|(event, commands)| {
            match event_mode(event, commands, settings_env, process_env) {
                ((Err(ModeError::Invalid(raw)), source), _) => Some(format!(
                    "the hand-wired {event} fails to start: {} ({})",
                    invalid_reason(&raw),
                    source_label(source)
                )),
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
    .with_hint(hints.join(HINT_JOINER))
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
    .with_hint(hints.join(HINT_JOINER))
}

fn no_miner_check() -> Check {
    Check::new(
        "hook_mining",
        CheckStatus::Warn,
        "no Stop or PreCompact hook is wired: transcripts are not mined",
    )
    .with_hint(INSTALL_HINT)
}

/// Why a wired miner does not mine. Raw values are the `IRONMEM_MCP_MODE`
/// text as the file or shell has it, so hints quote it, not a normalised label.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NoMineCause {
    /// The mode is invalid, so the hook exits at startup.
    Invalid,
    /// The mode is built by the shell, so doctor can't judge it.
    Dynamic,
    /// A plugin hook whose wrapper default is overridden by settings.json
    /// `env` or the shell.
    PluginOverride,
    /// The hook command sets this raw mode itself.
    Inline(String),
    /// A manual install's settings.json `env` sets this raw mode.
    SettingsEnv(String),
    /// A manual install inherits this raw mode from the shell.
    Shell(String),
    /// A bare manual command: nothing sets the mode, so it is read-only.
    Default,
}

/// One hint's cause and the miners it applies to, in entry order.
struct CauseGroup {
    cause: NoMineCause,
    events: Vec<&'static str>,
}

impl NoMineCause {
    fn of(source: ModeSource, raw: Option<String>, plugin: bool) -> Self {
        let raw = raw.unwrap_or_default();
        match source {
            ModeSource::SettingsEnv | ModeSource::Environment if plugin => Self::PluginOverride,
            ModeSource::Inline => Self::Inline(raw),
            ModeSource::SettingsEnv => Self::SettingsEnv(raw),
            ModeSource::Environment => Self::Shell(raw),
            ModeSource::PluginWrapper | ModeSource::Default => Self::Default,
        }
    }

    fn hint(&self, events: &[&str]) -> String {
        match self {
            Self::Invalid => INVALID_MODE_HINT.to_owned(),
            Self::Dynamic => DYNAMIC_MODE_HINT.to_owned(),
            Self::PluginOverride => PLUGIN_OVERRIDE_HINT.to_owned(),
            Self::Inline(raw) => {
                let sets = if events.len() == 1 {
                    "command itself sets"
                } else {
                    "commands themselves set"
                };
                format!(
                    "the {} {sets} IRONMEM_MCP_MODE={raw:?}; change it to trusted to mine",
                    events.join(" and ")
                )
            }
            Self::SettingsEnv(raw) => format!(
                "settings.json sets env.IRONMEM_MCP_MODE={raw:?}; change it to trusted to mine \
                 (removing it leaves bare hook commands read-only)"
            ),
            Self::Shell(raw) => format!(
                "Claude Code inherits IRONMEM_MCP_MODE={raw:?} from the shell that launches it; \
                 launch it with IRONMEM_MCP_MODE=trusted to mine"
            ),
            Self::Default => read_only_hint(events),
        }
    }
}

/// Add `event` to the group for `cause`, or start one; groups stay in the
/// order their first entry appears.
fn add_cause(groups: &mut Vec<CauseGroup>, cause: NoMineCause, event: &'static str) {
    match groups.iter_mut().find(|g| g.cause == cause) {
        Some(group) => group.events.push(event),
        None => groups.push(CauseGroup {
            cause,
            events: vec![event],
        }),
    }
}

/// `Stop`, `Stop and PreCompact`, with the matching verb.
fn events_with_verb(events: &[&str]) -> String {
    let verb = if events.len() == 1 { "is" } else { "are" };
    format!("{} {verb}", events.join(" and "))
}

/// A headline that holds in every mix of mined, not-mined and unknown miners.
fn mining_headline(mined: &[&str], not_mined: &[&str], unknown: &[&str]) -> String {
    if not_mined.is_empty() && unknown.is_empty() {
        return format!("{} mined", events_with_verb(mined));
    }
    let mut parts = Vec::new();
    if mined.is_empty() {
        parts.push(if unknown.is_empty() {
            "no wired hook mines".to_owned()
        } else {
            "no wired hook is known to mine".to_owned()
        });
    } else {
        parts.push(format!("{} mined", events_with_verb(mined)));
        if !not_mined.is_empty() {
            parts.push(format!("{} not", events_with_verb(not_mined)));
        }
    }
    if !unknown.is_empty() {
        parts.push(format!("{} may not be", unknown.join(" and ")));
    }
    parts.join("; ")
}

/// The `hook_mining` check: a headline, then one `<Event>: <mode> (<source>)
/// -> <outcome>` entry per wired miner, joined with `; ` because doctor
/// renders each summary as one line. Ok only when every wired miner mines;
/// the hint holds one entry per distinct [`NoMineCause`], in the order of the
/// entries, joined with [`HINT_JOINER`]. `plugin` says the miners run on the
/// plugin path (the plugin's hooks plus any hand-wired copies).
fn mining_check(
    wired: &WiredHooks,
    settings_env: Option<&str>,
    process_env: Option<&str>,
    plugin: bool,
) -> Check {
    let miners: Vec<&(&'static str, Vec<String>)> = wired
        .iter()
        .filter(|(event, _)| MINING_EVENTS.contains(event))
        .collect();
    if miners.is_empty() {
        return no_miner_check();
    }
    let (mut mined, mut not_mined, mut unknown) = (Vec::new(), Vec::new(), Vec::new());
    let mut lines = Vec::new();
    let mut causes: Vec<CauseGroup> = Vec::new();
    for (event, commands) in miners {
        let ((result, source), raw) = event_mode(event, commands, settings_env, process_env);
        let label = source_label(source);
        lines.push(match result {
            Ok(McpAccessMode::Trusted) => {
                mined.push(*event);
                if source == ModeSource::Environment {
                    format!(
                        "{event}: trusted (inherited from this shell; Claude Code must be \
                         launched with it) -> mined"
                    )
                } else {
                    format!("{event}: trusted ({label}) -> mined")
                }
            }
            Ok(mode) => {
                not_mined.push(*event);
                add_cause(&mut causes, NoMineCause::of(source, raw, plugin), event);
                format!("{event}: {} ({label}) -> not mined", mode_label(mode))
            }
            Err(ModeError::Invalid(raw)) => {
                not_mined.push(*event);
                add_cause(&mut causes, NoMineCause::Invalid, event);
                format!(
                    "{event}: {raw:?} ({label}) -> fails to start: {}",
                    invalid_reason(&raw)
                )
            }
            Err(ModeError::Unknown(raw)) => {
                unknown.push(*event);
                add_cause(&mut causes, NoMineCause::Dynamic, event);
                format!("{event}: {raw} ({label}) -> mode cannot be determined statically")
            }
        });
    }
    let status = if causes.is_empty() {
        CheckStatus::Ok
    } else {
        CheckStatus::Warn
    };
    let summary = format!(
        "{} — {}",
        mining_headline(&mined, &not_mined, &unknown),
        lines.join("; ")
    );
    let check = Check::new("hook_mining", status, summary);
    if causes.is_empty() {
        return check;
    }
    let hints: Vec<String> = causes.iter().map(|g| g.cause.hint(&g.events)).collect();
    check.with_hint(hints.join(HINT_JOINER))
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
            checks[1].summary.starts_with("no wired hook mines"),
            "{}",
            checks[1].summary
        );
        assert!(
            checks[1]
                .summary
                .contains("Stop: read-only (IRONMEM_MCP_MODE unset, so the default) -> not mined"),
            "{}",
            checks[1].summary
        );
        assert!(
            checks[1].summary.contains(
                "PreCompact: read-only (IRONMEM_MCP_MODE unset, so the default) -> not mined"
            ),
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
        assert!(
            checks[1]
                .summary
                .starts_with("Stop and PreCompact are mined"),
            "{}",
            checks[1].summary
        );
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
            c[1].summary
                .contains("Stop: read-only (set by env in settings.json) -> not mined"),
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
        // Stop and PreCompact never recall, so recall is not mentioned.
        assert!(
            c[1].summary
                .contains(r#"Stop: "bogus" (set in the hook command) -> fails to start"#),
            "{}",
            c[1].summary
        );
        assert!(
            c[1].summary
                .contains(r#"PreCompact: "bogus" (set in the hook command) -> fails to start"#),
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
        assert!(
            s.contains(r#"Stop: "bogus" (set in the hook command) -> fails to start"#),
            "{s}"
        );
        assert!(
            s.contains("PreCompact: read-only (set in the hook command) -> not mined"),
            "{s}"
        );
        assert!(!s.contains("Stop: read-only"), "{s}");
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
            .contains("Stop: trusted (inherited from this shell;"));
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
            c[1].summary
                .contains("Stop: restricted (set in the hook command) -> not mined"),
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
            c.summary
                .contains("Stop: trusted (set in the hook command) -> mined"),
            "Stop has a trusted command: {}",
            c.summary
        );
        assert!(
            c.summary.contains(
                "PreCompact: read-only (IRONMEM_MCP_MODE unset, so the default) -> not mined"
            ),
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
        assert!(
            c.summary
                .contains("Stop: read-only (IRONMEM_MCP_MODE unset, so the default) -> not mined"),
            "{}",
            c.summary
        );
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
        assert!(c.summary.starts_with("Stop is mined"), "{}", c.summary);
        assert!(!c.summary.contains("PreCompact"), "{}", c.summary);

        let both = ALL_FOUR_BARE.map(|(e, c)| (e, format!("IRONMEM_MCP_MODE=trusted {c}")));
        let refs: Vec<(&str, &str)> = both.iter().map(|(e, c)| (*e, c.as_str())).collect();
        let c = mining_of(&settings_with(&refs), None);
        assert!(
            c.summary.starts_with("Stop and PreCompact are mined"),
            "{}",
            c.summary
        );
    }

    fn miners(stop: &str, precompact: &str) -> String {
        settings_with(&[("Stop", stop), ("PreCompact", precompact)])
    }

    fn plugin_mining(env_mode: Option<&str>, shell: Option<&str>) -> Check {
        let env = env_mode.map_or(String::new(), |m| {
            format!(r#", "env": {{"IRONMEM_MCP_MODE": "{m}"}}"#)
        });
        mining_of(&format!("{PLUGIN_ON}{env}}}"), shell)
    }

    /// The bare-read-only hint for `commands`, spelled out independently of
    /// `read_only_hint` so the test pins the text.
    fn read_only_text(commands: &str) -> String {
        format!(
            "expected outside a plugin install: MCP writes and prompt recall still work. \
             A cheaper way to mine without the plugin is tracked in #418. To mine now, \
             prefix the {commands} with IRONMEM_MCP_MODE=trusted; each run then loads \
             the embedding model and rebuilds the vector index"
        )
    }

    const TRUSTED_STOP: &str = "IRONMEM_MCP_MODE=trusted ironmem hook stop";
    const TRUSTED_PRECOMPACT: &str = "IRONMEM_MCP_MODE=trusted ironmem hook precompact";

    #[test]
    fn mixed_read_only_stop_and_trusted_precompact() {
        let c = mining_of(&miners("ironmem hook stop", TRUSTED_PRECOMPACT), None);
        assert_eq!(c.status, CheckStatus::Warn, "{}", c.summary);
        assert!(
            c.summary.starts_with("PreCompact is mined; Stop is not"),
            "{}",
            c.summary
        );
        assert!(
            c.summary
                .contains("PreCompact: trusted (set in the hook command) -> mined"),
            "{}",
            c.summary
        );
        assert!(!c.summary.contains("new transcripts are not mined"));
        assert_eq!(
            c.hint.as_deref(),
            Some(read_only_text("Stop command").as_str())
        );
    }

    #[test]
    fn mixed_invalid_stop_and_trusted_precompact() {
        let c = mining_of(
            &miners(
                "IRONMEM_MCP_MODE=bogus ironmem hook stop",
                TRUSTED_PRECOMPACT,
            ),
            None,
        );
        assert_eq!(c.status, CheckStatus::Warn, "{}", c.summary);
        assert!(
            c.summary.starts_with("PreCompact is mined; Stop is not"),
            "{}",
            c.summary
        );
        assert!(
            c.summary
                .contains(r#"Stop: "bogus" (set in the hook command) -> fails to start"#),
            "{}",
            c.summary
        );
        assert_eq!(c.hint.as_deref(), Some(INVALID_MODE_HINT));
    }

    #[test]
    fn both_read_only_miners() {
        let c = mining_of(
            &miners("ironmem hook stop", "ironmem hook precompact"),
            None,
        );
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(
            c.summary.starts_with("no wired hook mines"),
            "{}",
            c.summary
        );
        assert_eq!(
            c.hint.as_deref(),
            Some(read_only_text("Stop and PreCompact commands").as_str())
        );
    }

    #[test]
    fn both_trusted_miners() {
        let c = mining_of(&miners(TRUSTED_STOP, TRUSTED_PRECOMPACT), None);
        assert_eq!(c.status, CheckStatus::Ok, "{}", c.summary);
        assert!(
            c.summary.starts_with("Stop and PreCompact are mined"),
            "{}",
            c.summary
        );
        assert_eq!(c.hint, None);
    }

    #[test]
    fn plugin_with_a_settings_env_override_says_remove_it() {
        let c = plugin_mining(Some("read-only"), None);
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(
            c.summary.starts_with("no wired hook mines"),
            "{}",
            c.summary
        );
        assert!(
            c.summary
                .contains("Stop: read-only (set by env in settings.json) -> not mined"),
            "{}",
            c.summary
        );
        assert_eq!(c.hint.as_deref(), Some(PLUGIN_OVERRIDE_HINT));
        assert!(!c.hint.as_deref().unwrap().contains("prefix"));
    }

    #[test]
    fn plugin_with_a_shell_override_says_remove_it() {
        let c = plugin_mining(None, Some("restricted"));
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(
            c.summary.starts_with("no wired hook mines"),
            "{}",
            c.summary
        );
        assert!(
            c.summary.contains(
                "PreCompact: restricted (inherited from IRONMEM_MCP_MODE in this shell) \
                 -> not mined"
            ),
            "{}",
            c.summary
        );
        assert_eq!(c.hint.as_deref(), Some(PLUGIN_OVERRIDE_HINT));
    }

    #[test]
    fn an_inline_read_only_mode_is_not_called_expected() {
        let stop = "IRONMEM_MCP_MODE=read-only ironmem hook stop";
        let c = mining_of(&miners(stop, TRUSTED_PRECOMPACT), None);
        assert!(
            c.summary.starts_with("PreCompact is mined; Stop is not"),
            "{}",
            c.summary
        );
        let hint = c.hint.as_deref().unwrap();
        assert_eq!(
            hint,
            concat!(
                r#"the Stop command itself sets IRONMEM_MCP_MODE="read-only"; "#,
                "change it to trusted to mine"
            )
        );
        assert!(!hint.contains("expected"), "{hint}");
        assert!(!hint.contains("prefix"), "{hint}");
    }

    #[test]
    fn only_the_shell_trusted_miner_carries_the_shell_note() {
        let c = mining_of(
            &miners(TRUSTED_STOP, "ironmem hook precompact"),
            Some("trusted"),
        );
        assert_eq!(c.status, CheckStatus::Ok, "{}", c.summary);
        assert!(
            c.summary.starts_with("Stop and PreCompact are mined"),
            "{}",
            c.summary
        );
        assert!(
            c.summary
                .contains("Stop: trusted (set in the hook command) -> mined; PreCompact:"),
            "{}",
            c.summary
        );
        assert!(
            c.summary.contains(
                "PreCompact: trusted (inherited from this shell; Claude Code must be \
                 launched with it) -> mined"
            ),
            "{}",
            c.summary
        );
        assert_eq!(c.summary.matches("must be launched").count(), 1);
        assert_eq!(c.hint, None);
    }

    #[test]
    fn a_manual_settings_env_read_only_points_at_settings_json() {
        let mut doc: Value =
            serde_json::from_str(&miners("ironmem hook stop", "ironmem hook precompact")).unwrap();
        doc["env"] = serde_json::json!({"IRONMEM_MCP_MODE": "read-only"});
        let c = mining_of(&doc.to_string(), None);
        assert!(
            c.summary.starts_with("no wired hook mines"),
            "{}",
            c.summary
        );
        let hint = c.hint.as_deref().unwrap();
        assert!(hint.contains("env.IRONMEM_MCP_MODE"), "{hint}");
        assert!(hint.contains("settings.json"), "{hint}");
        assert!(!hint.contains("prefix"), "{hint}");
        assert!(!hint.contains("expected"), "{hint}");
    }

    #[test]
    fn a_manual_shell_read_only_points_at_the_shell() {
        let c = mining_of(
            &miners("ironmem hook stop", "ironmem hook precompact"),
            Some("read-only"),
        );
        let hint = c.hint.as_deref().unwrap();
        assert!(hint.contains("launch"), "{hint}");
        assert!(!hint.contains("prefix"), "{hint}");
    }

    #[test]
    fn distinct_causes_give_each_hint_once_in_entry_order() {
        let doc = serde_json::json!({"hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "ironmem hook stop"}]}],
            "PreCompact": [{"hooks": [{"type": "command",
                "command": "IRONMEM_MCP_MODE=$M ironmem hook precompact"}]}]
        }});
        let c = mining_of(&doc.to_string(), None);
        assert!(
            c.summary
                .starts_with("no wired hook is known to mine; PreCompact may not be"),
            "{}",
            c.summary
        );
        assert!(
            c.summary
                .contains("PreCompact: $M (set in the hook command) -> mode cannot be"),
            "{}",
            c.summary
        );
        assert_eq!(
            c.hint.as_deref(),
            Some(format!("{} | {DYNAMIC_MODE_HINT}", read_only_text("Stop command")).as_str())
        );
    }

    #[test]
    fn a_hand_wired_copy_with_an_invalid_mode_is_reported_with_the_duplicate() {
        let mut doc: Value = serde_json::from_str(&settings_with(&[(
            "UserPromptSubmit",
            "IRONMEM_MCP_MODE=bogus ironmem hook user-prompt-submit",
        )]))
        .unwrap();
        doc["enabledPlugins"] = serde_json::json!({"ironmem@ironrace": true});
        let home = home_with_settings(&doc.to_string());
        let c = claude_hook_checks(Some(home.path()), None);
        let s = &c[0].summary;
        assert_eq!(c[0].status, CheckStatus::Warn, "{s}");
        assert!(s.contains("run twice"), "{s}");
        assert!(
            s.contains("the hand-wired UserPromptSubmit fails to start"),
            "{s}"
        );
        assert!(!s.contains("also"), "the plugin's copy starts fine: {s}");
        let hint = c[0].hint.as_deref().unwrap();
        assert_eq!(hint, format!("{DUPLICATE_HINT} | {INVALID_MODE_HINT}"));
    }

    #[test]
    fn an_empty_mode_is_quoted_in_every_invalid_message() {
        let home = home_with_settings(&settings_with(&ALL_FOUR_BARE));
        let c = claude_hook_checks(Some(home.path()), Some(""));
        for check in &c {
            assert!(check.summary.contains(r#""""#), "{}", check.summary);
            assert!(!check.summary.contains("got  "), "{}", check.summary);
        }
        assert!(
            c[0].summary
                .contains(r#"IRONMEM_MCP_MODE="" is not trusted, read-only or restricted"#),
            "{}",
            c[0].summary
        );
        assert!(
            c[1].summary.contains(
                r#"Stop: "" (inherited from IRONMEM_MCP_MODE in this shell) -> fails to start"#
            ),
            "{}",
            c[1].summary
        );
    }

    #[test]
    fn hand_wired_trusted_miners_count_on_the_plugin_path() {
        let mut doc: Value =
            serde_json::from_str(&miners(TRUSTED_STOP, TRUSTED_PRECOMPACT)).unwrap();
        doc["enabledPlugins"] = serde_json::json!({"ironmem@ironrace": true});
        doc["env"] = serde_json::json!({"IRONMEM_MCP_MODE": "read-only"});
        let c = mining_of(&doc.to_string(), None);
        assert_eq!(c.status, CheckStatus::Ok, "{}", c.summary);
        assert!(
            c.summary.starts_with("Stop and PreCompact are mined"),
            "{}",
            c.summary
        );
        doc["env"] = serde_json::json!({});
        let c = mining_of(&doc.to_string(), Some("read-only"));
        assert_eq!(c.status, CheckStatus::Ok, "{}", c.summary);
    }

    #[test]
    fn an_unknown_mode_outranks_a_read_only_one_in_either_order() {
        let dynamic = "IRONMEM_MCP_MODE=$M ironmem hook stop";
        for pair in [
            ["ironmem hook stop", dynamic],
            [dynamic, "ironmem hook stop"],
        ] {
            let doc = serde_json::json!({"hooks": {"Stop": [{"hooks": [
                {"type": "command", "command": pair[0]},
                {"type": "command", "command": pair[1]}
            ]}]}});
            let c = mining_of(&doc.to_string(), None);
            assert!(
                c.summary
                    .starts_with("no wired hook is known to mine; Stop may not be"),
                "{pair:?}: {}",
                c.summary
            );
            assert_eq!(c.hint.as_deref(), Some(DYNAMIC_MODE_HINT), "{pair:?}");
        }
    }

    #[test]
    fn inline_and_settings_env_hints_quote_the_raw_value() {
        let stop = "IRONMEM_MCP_MODE=readonly ironmem hook stop";
        let pre = "IRONMEM_MCP_MODE=READ-ONLY ironmem hook precompact";
        let c = mining_of(&miners(stop, pre), None);
        assert_eq!(
            c.hint.as_deref(),
            Some(concat!(
                r#"the Stop command itself sets IRONMEM_MCP_MODE="readonly"; "#,
                "change it to trusted to mine | ",
                r#"the PreCompact command itself sets IRONMEM_MCP_MODE="READ-ONLY"; "#,
                "change it to trusted to mine"
            ))
        );
        let same = "IRONMEM_MCP_MODE=restricted ironmem hook";
        let c = mining_of(
            &miners(&format!("{same} stop"), &format!("{same} precompact")),
            None,
        );
        assert_eq!(
            c.hint.as_deref(),
            Some(concat!(
                "the Stop and PreCompact commands themselves set ",
                r#"IRONMEM_MCP_MODE="restricted"; change it to trusted to mine"#
            ))
        );

        let mut doc: Value =
            serde_json::from_str(&miners("ironmem hook stop", "ironmem hook precompact")).unwrap();
        doc["env"] = serde_json::json!({"IRONMEM_MCP_MODE": "ReadOnly"});
        let c = mining_of(&doc.to_string(), None);
        let hint = c.hint.as_deref().unwrap();
        assert!(
            hint.contains(r#"env.IRONMEM_MCP_MODE="ReadOnly""#),
            "{hint}"
        );
    }

    #[test]
    fn hints_follow_the_entry_order() {
        let pre = "IRONMEM_MCP_MODE=bogus ironmem hook precompact";
        let c = mining_of(&miners("ironmem hook stop", pre), None);
        let hint = c.hint.as_deref().unwrap();
        assert!(
            hint.starts_with("expected outside a plugin install"),
            "Stop's hint comes first: {hint}"
        );
        assert!(hint.ends_with(&format!(" | {INVALID_MODE_HINT}")), "{hint}");
    }

    #[test]
    fn a_later_trusted_assignment_does_not_make_the_hook_mine() {
        let later = "ironmem hook stop; IRONMEM_MCP_MODE=trusted true";
        let c = mining_of(&settings_with(&[("Stop", later)]), None);
        assert_eq!(c.status, CheckStatus::Warn, "{}", c.summary);
        assert!(
            c.summary
                .contains("Stop: read-only (IRONMEM_MCP_MODE unset, so the default) -> not mined"),
            "{}",
            c.summary
        );
    }
}

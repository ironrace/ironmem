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
    best_result, hook_mode_with_raw, hook_modes_with_raw, mode_label, runs_hook, source_label,
    ModeError, ModeSource, RawModeResult,
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
        /// Plugin hooks that settings.json also wires by hand (they run
        /// twice unless the plugin's copy fails at startup), with their
        /// commands.
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
            // A hand-wired copy runs as well as the plugin's, so it can start,
            // recall and mine in the plugin copy's place.
            let candidates: WiredHooks = wired
                .iter()
                .map(|(event, commands)| {
                    let extra = hand_wired.iter().filter(|(e, _)| e == event);
                    let all = commands.iter().chain(extra.flat_map(|(_, c)| c)).cloned();
                    (*event, all.collect())
                })
                .collect();
            let split = split_hand_wired(&wired, &hand_wired, env, process_env_mode);
            // A `Maybe` here needs a dynamic hand-wired copy (the plugin's copy
            // never has one), which `split_hand_wired` already reports.
            let (failures, _) = startup_failures(&candidates, env, process_env_mode);
            let mut mining = mining_check(&candidates, env, process_env_mode, true);
            if mined_only_by_hand_wired(&wired, &candidates, env, process_env_mode) {
                add_hint(&mut mining, PLUGIN_OVERRIDE_HINT);
            }
            vec![plugin_hooks_check(&split, &failures), mining]
        }
        HookWiring::Manual {
            wired,
            missing,
            settings_env_mode,
        } => {
            let (failures, may_fail) =
                startup_failures(&wired, settings_env_mode.as_deref(), process_env_mode);
            vec![
                hooks_check(&path, &missing, &failures, &may_fail),
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

/// True when some plugin miner mines only through a hand-wired copy: the
/// plugin's own command doesn't run trusted (an override wins over the
/// wrapper's default), so removing the copies, as the `hooks` hint says,
/// would stop mining.
fn mined_only_by_hand_wired(
    wired: &WiredHooks,
    candidates: &WiredHooks,
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> bool {
    let trusted = |hooks: &WiredHooks, event: &str| {
        hooks
            .iter()
            .filter(|(e, _)| *e == event)
            .any(|(e, commands)| {
                let ((result, _), _) = event_mode(e, commands, settings_env, process_env);
                result == Ok(McpAccessMode::Trusted)
            })
    };
    MINING_EVENTS
        .iter()
        .any(|event| !trusted(wired, event) && trusted(candidates, event))
}

/// Append `hint` to `check`'s hint unless it is already there.
fn add_hint(check: &mut Check, hint: &str) {
    check.hint = match check.hint.take() {
        Some(h) if h.split(HINT_JOINER).any(|part| part == hint) => Some(h),
        Some(h) => Some(format!("{h}{HINT_JOINER}{hint}")),
        None => Some(hint.to_owned()),
    };
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

/// `event` may exit at startup: its mode `raw` is built by the shell.
fn possible_startup_failure(event: &str, raw: &str, source: ModeSource) -> String {
    format!(
        "{event}'s mode cannot be determined statically (IRONMEM_MCP_MODE={raw}, {}), so it \
         may fail to start, and then {}",
        source_label(source),
        event_effect(event)
    )
}

/// The startup outcome of the non-mining hooks (SessionStart,
/// UserPromptSubmit), by [`starts`]: entries for those that surely fail
/// (`No`: `Config::load` rejects every run's mode, so they exit at startup),
/// then entries for those that may fail (`Maybe`: no run is surely valid and
/// one's mode is built by the shell). Each entry names its own effect: no
/// memory context at session start for SessionStart, prompt recall off for
/// UserPromptSubmit. Miners are left to `hook_mining`, which reports both
/// outcomes for them.
fn startup_failures(
    wired: &WiredHooks,
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> (Vec<String>, Vec<String>) {
    let (mut fail, mut may_fail) = (Vec::new(), Vec::new());
    for (event, commands) in wired {
        if MINING_EVENTS.contains(event) {
            continue;
        }
        match starts(event, commands, settings_env, process_env) {
            Starts::No(raw, source) => fail.push(startup_failure(event, &raw, source)),
            Starts::Maybe(raw, source) => {
                may_fail.push(possible_startup_failure(event, &raw, source))
            }
            Starts::Yes => {}
        }
    }
    (fail, may_fail)
}

/// Whether any of an event's copies starts at all.
#[derive(Debug, PartialEq, Eq)]
enum Starts {
    /// Some run of the hook has a valid mode.
    Yes,
    /// No run surely starts, but one's mode is built by the shell: this raw
    /// value, from this source.
    Maybe(String, ModeSource),
    /// Every run's mode is invalid; the first one's raw value and source.
    No(String, ModeSource),
}

/// Whether `commands`, the copies of `event`, start: [`Starts::Yes`] when
/// any run of the hook has a valid mode, across commands and within one, so a
/// dynamic or invalid run never hides a working one.
fn starts(
    event: &str,
    commands: &[String],
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> Starts {
    let hook = plugin_hook_name(event);
    let results: Vec<_> = commands
        .iter()
        .flat_map(|c| hook_modes_with_raw(c, hook, settings_env, process_env))
        .map(|(result, _)| result)
        .collect();
    // No runs at all reads as the read-only default, which starts.
    if results.is_empty() || results.iter().any(|(r, _)| r.is_ok()) {
        return Starts::Yes;
    }
    let unknown = results.iter().find_map(|(r, source)| match r {
        Err(ModeError::Unknown(raw)) => Some(Starts::Maybe(raw.clone(), *source)),
        _ => None,
    });
    let invalid = results.iter().find_map(|(r, source)| match r {
        Err(ModeError::Invalid(raw)) => Some(Starts::No(raw.clone(), *source)),
        _ => None,
    });
    unknown.or(invalid).unwrap_or(Starts::Yes)
}

/// How settings.json's hand-wired copies of plugin hooks sit beside the
/// plugin's own copies.
struct HandWired {
    /// Events where both copies start, so the hook runs twice.
    duplicated: Vec<&'static str>,
    /// One entry for each other hand-wired event, in PLUGIN_HOOKS (hooks.json)
    /// order, the order `detect_hook_wiring` collects them in.
    entries: Vec<String>,
    /// The hints those entries need, in entry order (repeats allowed).
    hints: Vec<&'static str>,
}

/// Sort the hand-wired plugin hooks by whether each copy starts. A hook runs
/// twice only when both copies surely start. Otherwise the entry says which
/// copy runs (or may run). A broken or dynamic hand-wired copy beside a
/// working plugin copy gets only the remove-the-copies hint: the plugin covers
/// the event, and fixing the copy's mode would make the hook run twice (or,
/// for an empty settings.json `env` value, which the wrapper reads as trusted,
/// could make the plugin's copies read-only). A working hand-wired copy beside
/// a failing plugin copy never gets that hint, because removing it would turn
/// the hook off. When every copy fails, `startup_failures` (or `hook_mining`,
/// for a miner) names the effect.
fn split_hand_wired(
    wired: &WiredHooks,
    hand_wired: &WiredHooks,
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> HandWired {
    let mut out = HandWired {
        duplicated: Vec::new(),
        entries: Vec::new(),
        hints: Vec::new(),
    };
    for (event, commands) in hand_wired {
        let plugin_copy = wired.iter().find(|(e, _)| e == event).map(|(_, c)| c);
        // Never `None`: `wired` holds all of PLUGIN_HOOKS and `hand_wired`
        // only events from it. Doctor stays total, so no `expect`.
        let plugin =
            plugin_copy.map_or(Starts::Yes, |c| starts(event, c, settings_env, process_env));
        // `plugin` is never `Starts::Maybe` (pinned by
        // `a_plugin_copy_never_has_a_dynamic_mode`), so the `_` arms below
        // stand for `Starts::Yes`.
        let copy = starts(event, commands, settings_env, process_env);
        let (entry, hints): (String, &[&'static str]) = match (plugin, copy) {
            (Starts::No(raw, source), Starts::Yes) => (
                format!(
                    "the plugin's {event} fails to start: {} ({}), so only the hand-wired copy \
                     runs",
                    invalid_reason(&raw),
                    source_label(source)
                ),
                &[INVALID_MODE_HINT],
            ),
            (Starts::No(raw, source), Starts::Maybe(copy_raw, copy_source)) => (
                format!(
                    "the plugin's {event} fails to start: {} ({}), so only the hand-wired copy \
                     may run: its mode cannot be determined statically \
                     (IRONMEM_MCP_MODE={copy_raw}, {})",
                    invalid_reason(&raw),
                    source_label(source),
                    source_label(copy_source)
                ),
                &[INVALID_MODE_HINT, DYNAMIC_MODE_HINT],
            ),
            (Starts::No(..), Starts::No(raw, source)) => (
                format!(
                    "the hand-wired {event} fails to start: {} ({})",
                    invalid_reason(&raw),
                    source_label(source)
                ),
                &[INVALID_MODE_HINT],
            ),
            (_, Starts::Yes) => {
                out.duplicated.push(*event);
                continue;
            }
            (_, Starts::Maybe(raw, source)) => (
                format!(
                    "the hand-wired {event}'s mode cannot be determined statically \
                     (IRONMEM_MCP_MODE={raw}, {}), so the plugin's copy runs and the \
                     hand-wired copy may run too",
                    source_label(source)
                ),
                &[DUPLICATE_HINT],
            ),
            (_, Starts::No(raw, source)) => (
                format!(
                    "the hand-wired {event} fails to start: {} ({}), so only the plugin's copy \
                     runs",
                    invalid_reason(&raw),
                    source_label(source)
                ),
                &[DUPLICATE_HINT],
            ),
        };
        out.entries.push(entry);
        out.hints.extend_from_slice(hints);
    }
    out
}

/// The plugin path's `hooks` check: the run-twice events from `hand_wired`,
/// then its other entries, then `failures` (the startup failures). Hints
/// follow that order, each given once.
fn plugin_hooks_check(hand_wired: &HandWired, failures: &[String]) -> Check {
    let HandWired {
        duplicated,
        entries,
        hints: entry_hints,
    } = hand_wired;
    if duplicated.is_empty() && entries.is_empty() && failures.is_empty() {
        return Check::new(
            "hooks",
            CheckStatus::Ok,
            "Claude Code hooks: supplied by the enabled ironmem plugin",
        );
    }
    let mut problems = Vec::new();
    let mut hints = Vec::new();
    if !duplicated.is_empty() {
        problems.push(format!(
            "the enabled ironmem plugin supplies them and settings.json wires {} by hand \
             too, so those hooks run twice",
            duplicated.join(", ")
        ));
        hints.push(DUPLICATE_HINT);
    }
    problems.extend(entries.iter().cloned());
    hints.extend(entry_hints.iter().copied());
    if !failures.is_empty() {
        problems.push(failures.join("; "));
        hints.push(INVALID_MODE_HINT);
    }
    let mut unique: Vec<&str> = Vec::new();
    for hint in hints {
        if !unique.contains(&hint) {
            unique.push(hint);
        }
    }
    Check::new(
        "hooks",
        CheckStatus::Warn,
        format!("Claude Code hooks: {}", problems.join("; ")),
    )
    .with_hint(unique.join(HINT_JOINER))
}

/// The manual install's `hooks` check: missing hooks, then hooks that fail to
/// start (`failures`), then hooks that may (`may_fail`), each with one hint.
fn hooks_check(path: &Path, missing: &[&str], failures: &[String], may_fail: &[String]) -> Check {
    if missing.is_empty() && failures.is_empty() && may_fail.is_empty() {
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
    // Warn, as hook_mining does for a mode it cannot determine.
    if !may_fail.is_empty() {
        problems.push(may_fail.join("; "));
        hints.push(DYNAMIC_MODE_HINT);
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
                let it = if events.len() == 1 { "it" } else { "them" };
                format!(
                    "the {} {sets} IRONMEM_MCP_MODE={raw:?}; change {it} to trusted to mine",
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
            Some("ReadOnly"),
        );
        let hint = c.hint.as_deref().unwrap();
        assert!(hint.contains("launch"), "{hint}");
        assert!(
            hint.contains(r#"inherits IRONMEM_MCP_MODE="ReadOnly""#),
            "{hint}"
        );
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
    fn a_hand_wired_copy_with_an_invalid_mode_leaves_only_the_plugin_copy() {
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
        // Only the plugin's copy starts, so the hook does not run twice.
        assert_eq!(
            *s,
            "Claude Code hooks: the hand-wired UserPromptSubmit fails to start: \
             IRONMEM_MCP_MODE=\"bogus\" is not trusted, read-only or restricted (set in the \
             hook command), so only the plugin's copy runs"
        );
        assert!(!s.contains("run twice"), "{s}");
        assert!(!s.contains("also"), "the plugin's copy starts fine: {s}");
        // The broken copy can go: the plugin covers the event. Fixing its mode
        // instead would make the hook run twice, so no fix-the-mode hint.
        assert_eq!(c[0].hint.as_deref(), Some(DUPLICATE_HINT));
    }

    #[test]
    fn an_empty_settings_env_mode_breaks_only_a_bare_hand_wired_copy() {
        // The wrapper reads "" as unset (trusted); Config::load rejects it.
        let home = home_with_settings(&plugin_with(
            "",
            &[("UserPromptSubmit", "ironmem hook user-prompt-submit")],
        ));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(c[0].status, CheckStatus::Warn, "{}", c[0].summary);
        assert_eq!(
            c[0].summary,
            "Claude Code hooks: the hand-wired UserPromptSubmit fails to start: \
             IRONMEM_MCP_MODE=\"\" is not trusted, read-only or restricted (set by env in \
             settings.json), so only the plugin's copy runs"
        );
        // Setting env to read-only, as the fix-the-mode hint would allow,
        // would stop the plugin's copies mining, so only the remove hint.
        assert_eq!(c[0].hint.as_deref(), Some(DUPLICATE_HINT));
        assert_eq!(c[1].status, CheckStatus::Ok, "{}", c[1].summary);
        assert_eq!(c[1].hint, None);
    }

    #[test]
    fn a_dynamic_hand_wired_copy_beside_a_running_plugin_copy_may_run_too() {
        let mut doc: Value = serde_json::from_str(&settings_with(&[(
            "UserPromptSubmit",
            "IRONMEM_MCP_MODE=$M ironmem hook user-prompt-submit",
        )]))
        .unwrap();
        doc["enabledPlugins"] = serde_json::json!({"ironmem@ironrace": true});
        let home = home_with_settings(&doc.to_string());
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(c[0].status, CheckStatus::Warn, "{}", c[0].summary);
        assert_eq!(
            c[0].summary,
            "Claude Code hooks: the hand-wired UserPromptSubmit's mode cannot be determined \
             statically (IRONMEM_MCP_MODE=$M, set in the hook command), so the plugin's copy \
             runs and the hand-wired copy may run too"
        );
        assert_eq!(c[0].hint.as_deref(), Some(DUPLICATE_HINT));
    }

    #[test]
    fn a_dynamic_hand_wired_copy_beside_a_failing_plugin_copy_may_cover_it() {
        let home = home_with_settings(&plugin_with(
            "bogus",
            &[(
                "UserPromptSubmit",
                "IRONMEM_MCP_MODE=$M ironmem hook user-prompt-submit",
            )],
        ));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(
            c[0].summary,
            format!(
                "Claude Code hooks: the plugin's UserPromptSubmit fails to start: {BOGUS_REASON} \
                 (set by env in settings.json), so only the hand-wired copy may run: its mode \
                 cannot be determined statically (IRONMEM_MCP_MODE=$M, set in the hook \
                 command); SessionStart fails to start: {BOGUS_REASON} (set by env in \
                 settings.json), so no memory context at session start"
            )
        );
        assert!(
            !c[0].summary.contains("only the hand-wired copy runs"),
            "{}",
            c[0].summary
        );
        assert_eq!(
            c[0].hint.as_deref(),
            Some(format!("{INVALID_MODE_HINT} | {DYNAMIC_MODE_HINT}").as_str())
        );
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
                r#"IRONMEM_MCP_MODE="restricted"; change them to trusted to mine"#
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
    fn plugin_startup_check_counts_a_valid_hand_wired_copy() {
        let mut doc: Value = serde_json::from_str(&settings_with(&[(
            "UserPromptSubmit",
            "IRONMEM_MCP_MODE=trusted ironmem hook user-prompt-submit",
        )]))
        .unwrap();
        doc["enabledPlugins"] = serde_json::json!({"ironmem@ironrace": true});
        doc["env"] = serde_json::json!({"IRONMEM_MCP_MODE": "bogus"});
        let home = home_with_settings(&doc.to_string());
        let s = &claude_hook_checks(Some(home.path()), None)[0].summary;
        // Only the plugin's copy of UserPromptSubmit fails; the hand-wired copy
        // runs, so the event is not reported as failed: recall is not reported
        // as off.
        assert_eq!(
            *s,
            format!(
                "Claude Code hooks: {}; SessionStart fails to start: {BOGUS_REASON} (set by \
                 env in settings.json), so no memory context at session start",
                only_hand_wired_runs("UserPromptSubmit")
            )
        );
        assert!(!s.contains("prompt recall is off"), "{s}");
        // The plugin's SessionStart has no valid copy, so it still fails.
        assert!(s.contains("SessionStart fails to start"), "{s}");
    }

    #[test]
    fn a_plugin_copy_never_has_a_dynamic_mode() {
        // split_hand_wired relies on this: env values are literal and the
        // plugin's command sets no mode, so its `Starts::Maybe` case never runs.
        for (event, name) in PLUGIN_HOOKS {
            let plugin = [format!("{PLUGIN_HOOK_COMMAND} {name}")];
            assert_eq!(
                starts(event, &plugin, Some("$M"), None),
                Starts::No("$M".to_owned(), ModeSource::SettingsEnv),
                "{event}"
            );
            assert_eq!(
                starts(event, &plugin, None, Some("$(cat /m)")),
                Starts::No("$(cat /m)".to_owned(), ModeSource::Environment),
                "{event}"
            );
        }
    }

    const UPS: &str = "/x/ironmem hook user-prompt-submit";

    /// One command that runs UserPromptSubmit twice, as `first; second`
    /// and as `second; first`.
    fn both_orders(first: &str, second: &str) -> [String; 2] {
        [
            format!("{first} {UPS}; {second} {UPS}"),
            format!("{second} {UPS}; {first} {UPS}"),
        ]
    }

    /// Plugin settings without an env mode, plus a hand-wired UserPromptSubmit.
    fn plugin_and_ups(command: &str) -> String {
        let mut doc: Value =
            serde_json::from_str(&settings_with(&[("UserPromptSubmit", command)])).unwrap();
        doc["enabledPlugins"] = serde_json::json!({"ironmem@ironrace": true});
        doc.to_string()
    }

    const UPS_RUNS_TWICE: &str = "Claude Code hooks: the enabled ironmem plugin supplies them \
         and settings.json wires UserPromptSubmit by hand too, so those hooks run twice";

    fn assert_runs_twice(command: &str) {
        let home = home_with_settings(&plugin_and_ups(command));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(c[0].summary, UPS_RUNS_TWICE, "{command}");
        assert_eq!(c[0].hint.as_deref(), Some(DUPLICATE_HINT), "{command}");
    }

    fn assert_only_hand_wired_runs(command: &str) {
        let home = home_with_settings(&plugin_with("bogus", &[("UserPromptSubmit", command)]));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(
            c[0].summary,
            format!(
                "Claude Code hooks: {}; SessionStart fails to start: {BOGUS_REASON} (set by \
                 env in settings.json), so no memory context at session start",
                only_hand_wired_runs("UserPromptSubmit")
            ),
            "{command}"
        );
        assert_eq!(c[0].hint.as_deref(), Some(INVALID_MODE_HINT), "{command}");
    }

    #[test]
    fn a_hand_wired_command_with_an_invalid_and_a_valid_run_starts_beside_the_plugin() {
        for cmd in both_orders("IRONMEM_MCP_MODE=bogus", "IRONMEM_MCP_MODE=trusted") {
            assert_runs_twice(&cmd);
        }
    }

    #[test]
    fn a_hand_wired_command_with_an_invalid_and_a_valid_run_replaces_a_failing_plugin() {
        for cmd in both_orders("IRONMEM_MCP_MODE=bogus", "IRONMEM_MCP_MODE=trusted") {
            assert_only_hand_wired_runs(&cmd);
        }
    }

    #[test]
    fn a_hand_wired_command_with_a_dynamic_and_a_valid_run_starts_beside_the_plugin() {
        for cmd in both_orders("IRONMEM_MCP_MODE=$M", "IRONMEM_MCP_MODE=read-only") {
            assert_runs_twice(&cmd);
        }
    }

    #[test]
    fn a_hand_wired_command_with_a_dynamic_and_a_valid_run_replaces_a_failing_plugin() {
        for cmd in both_orders("IRONMEM_MCP_MODE=$M", "IRONMEM_MCP_MODE=read-only") {
            assert_only_hand_wired_runs(&cmd);
        }
    }

    /// A manual install with all four hooks bare except UserPromptSubmit,
    /// which runs `command`.
    fn manual_with_ups(command: &str) -> String {
        let cmds: Vec<(&str, &str)> = ALL_FOUR_BARE
            .iter()
            .map(|(e, c)| {
                (
                    *e,
                    if *e == "UserPromptSubmit" {
                        command
                    } else {
                        *c
                    },
                )
            })
            .collect();
        settings_with(&cmds)
    }

    #[test]
    fn a_command_with_any_valid_run_is_not_a_startup_failure() {
        for (first, second) in [
            ("IRONMEM_MCP_MODE=$M", "IRONMEM_MCP_MODE=read-only"),
            ("IRONMEM_MCP_MODE=bogus", "IRONMEM_MCP_MODE=read-only"),
        ] {
            for cmd in both_orders(first, second) {
                let home = home_with_settings(&manual_with_ups(&cmd));
                let c = claude_hook_checks(Some(home.path()), None);
                assert_eq!(c[0].status, CheckStatus::Ok, "{cmd}: {}", c[0].summary);
                assert_eq!(c[0].summary, all_four_wired(home.path()), "{cmd}");
                assert_eq!(c[0].hint, None, "{cmd}");
            }
        }
    }

    /// The Ok `hooks` summary for a manual install under `home`.
    fn all_four_wired(home: &Path) -> String {
        let path = home.join(".claude").join("settings.json");
        format!("Claude Code hooks: all 4 wired in {}", path.display())
    }

    #[test]
    fn a_command_with_no_valid_run_keeps_the_existing_outcomes() {
        // Every run invalid: fails to start, quoting the first run's value.
        let cmd = format!("IRONMEM_MCP_MODE=bogus {UPS}; IRONMEM_MCP_MODE=other {UPS}");
        let home = home_with_settings(&manual_with_ups(&cmd));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(
            c[0].summary,
            "Claude Code hooks: UserPromptSubmit fails to start: IRONMEM_MCP_MODE=\"bogus\" is \
             not trusted, read-only or restricted (set in the hook command), so prompt recall \
             is off"
        );
        assert_eq!(c[0].hint.as_deref(), Some(INVALID_MODE_HINT));
        // A dynamic run may start or fail, so neither "fails" nor Ok.
        for cmd in both_orders("IRONMEM_MCP_MODE=$M", "IRONMEM_MCP_MODE=bogus") {
            let home = home_with_settings(&manual_with_ups(&cmd));
            let c = claude_hook_checks(Some(home.path()), None);
            assert_eq!(c[0].status, CheckStatus::Warn, "{cmd}: {}", c[0].summary);
            assert_eq!(
                c[0].summary,
                "Claude Code hooks: UserPromptSubmit's mode cannot be determined statically \
                 (IRONMEM_MCP_MODE=$M, set in the hook command), so it may fail to start, and \
                 then prompt recall is off",
                "{cmd}"
            );
            assert_eq!(c[0].hint.as_deref(), Some(DYNAMIC_MODE_HINT), "{cmd}");
        }
    }

    #[test]
    fn a_dynamic_manual_session_start_may_fail_to_start() {
        let home = home_with_settings(&all_four_with("SessionStart", "IRONMEM_MCP_MODE=$M"));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(c[0].status, CheckStatus::Warn, "{}", c[0].summary);
        assert_eq!(
            c[0].summary,
            "Claude Code hooks: SessionStart's mode cannot be determined statically \
             (IRONMEM_MCP_MODE=$M, set in the hook command), so it may fail to start, and then \
             no memory context at session start"
        );
        assert_eq!(c[0].hint.as_deref(), Some(DYNAMIC_MODE_HINT));
    }

    #[test]
    fn manual_hook_problems_are_grouped_by_cause_with_one_hint_each() {
        let home = home_with_settings(&settings_with(&[
            (
                "SessionStart",
                "IRONMEM_MCP_MODE=bogus ironmem hook session-start",
            ),
            (
                "UserPromptSubmit",
                "IRONMEM_MCP_MODE=$M ironmem hook user-prompt-submit",
            ),
            ("Stop", "ironmem hook stop"),
        ]));
        let c = claude_hook_checks(Some(home.path()), None);
        let path = home.path().join(".claude").join("settings.json");
        assert_eq!(
            c[0].summary,
            format!(
                "Claude Code hooks: missing PreCompact in {} (no mining before compaction); \
                 SessionStart fails to start: IRONMEM_MCP_MODE=\"bogus\" is not trusted, \
                 read-only or restricted (set in the hook command), so no memory context at \
                 session start; UserPromptSubmit's mode cannot be determined statically \
                 (IRONMEM_MCP_MODE=$M, set in the hook command), so it may fail to start, and \
                 then prompt recall is off",
                path.display()
            )
        );
        assert_eq!(
            c[0].hint.as_deref(),
            Some(format!("{INSTALL_HINT} | {INVALID_MODE_HINT} | {DYNAMIC_MODE_HINT}").as_str())
        );
    }

    #[test]
    fn a_dynamic_manual_miner_is_left_to_hook_mining() {
        // Miner startup outcomes (invalid or dynamic) belong to hook_mining.
        let home = home_with_settings(&all_four_with("Stop", "IRONMEM_MCP_MODE=$M"));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(c[0].status, CheckStatus::Ok, "{}", c[0].summary);
        assert_eq!(c[0].summary, all_four_wired(home.path()));
        assert_eq!(c[0].hint, None);
        assert!(
            c[1].summary
                .contains("Stop: $M (set in the hook command) -> mode cannot be determined"),
            "{}",
            c[1].summary
        );
    }

    #[test]
    fn a_dynamic_run_beside_a_read_only_one_may_mine() {
        for cmd in [
            "IRONMEM_MCP_MODE=$M ironmem hook stop; IRONMEM_MCP_MODE=read-only ironmem hook stop",
            "IRONMEM_MCP_MODE=read-only ironmem hook stop; IRONMEM_MCP_MODE=$M ironmem hook stop",
        ] {
            let c = mining_of(&settings_with(&[("Stop", cmd)]), None);
            assert_eq!(c.status, CheckStatus::Warn, "{cmd}");
            assert_eq!(
                c.summary,
                "no wired hook is known to mine; Stop may not be — Stop: $M (set in the hook \
                 command) -> mode cannot be determined statically",
                "{cmd}"
            );
            assert_eq!(c.hint.as_deref(), Some(DYNAMIC_MODE_HINT), "{cmd}");
        }
    }

    /// Plugin settings with `env.IRONMEM_MCP_MODE` set to `env_mode` and
    /// `hand_wired` copies of plugin hooks.
    fn plugin_with(env_mode: &str, hand_wired: &[(&str, &str)]) -> String {
        let mut doc: Value = serde_json::from_str(&settings_with(hand_wired)).unwrap();
        doc["enabledPlugins"] = serde_json::json!({"ironmem@ironrace": true});
        doc["env"] = serde_json::json!({"IRONMEM_MCP_MODE": env_mode});
        doc.to_string()
    }

    const BOGUS_REASON: &str =
        r#"IRONMEM_MCP_MODE="bogus" is not trusted, read-only or restricted"#;

    /// The entry for a plugin copy that fails at startup on `env.IRONMEM_MCP_MODE`
    /// = "bogus" while a hand-wired copy of `event` runs.
    fn only_hand_wired_runs(event: &str) -> String {
        format!(
            "the plugin's {event} fails to start: {BOGUS_REASON} (set by env in \
             settings.json), so only the hand-wired copy runs"
        )
    }

    #[test]
    fn a_failing_plugin_copy_with_a_valid_hand_wired_copy_does_not_run_twice() {
        let home = home_with_settings(&plugin_with(
            "bogus",
            &[
                (
                    "SessionStart",
                    "IRONMEM_MCP_MODE=trusted /x/ironmem hook session-start",
                ),
                (
                    "UserPromptSubmit",
                    "IRONMEM_MCP_MODE=trusted /x/ironmem hook user-prompt-submit",
                ),
            ],
        ));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(c[0].status, CheckStatus::Warn, "{}", c[0].summary);
        assert_eq!(
            c[0].summary,
            format!(
                "Claude Code hooks: {}; {}",
                only_hand_wired_runs("SessionStart"),
                only_hand_wired_runs("UserPromptSubmit")
            )
        );
        assert!(!c[0].summary.contains("run twice"), "{}", c[0].summary);
        // Removing the hand-wired copies would turn context and recall off.
        assert_eq!(c[0].hint.as_deref(), Some(INVALID_MODE_HINT));
    }

    #[test]
    fn a_failing_plugin_miner_with_a_trusted_hand_wired_copy_agrees_across_checks() {
        let home = home_with_settings(&plugin_with("bogus", &[("Stop", TRUSTED_STOP)]));
        let c = claude_hook_checks(Some(home.path()), None);
        let fails = |event: &str, effect: &str| {
            format!(
                "{event} fails to start: {BOGUS_REASON} (set by env in settings.json), \
                 so {effect}"
            )
        };
        assert_eq!(
            c[0].summary,
            format!(
                "Claude Code hooks: {}; {}; {}",
                only_hand_wired_runs("Stop"),
                fails("SessionStart", "no memory context at session start"),
                fails("UserPromptSubmit", "prompt recall is off"),
            )
        );
        assert_eq!(c[0].hint.as_deref(), Some(INVALID_MODE_HINT));
        // hook_mining agrees: the hand-wired Stop mines and must stay, and
        // removing the override lets the plugin's own PreCompact mine too.
        assert_eq!(
            c[1].summary,
            format!(
                "Stop is mined; PreCompact is not — Stop: trusted (set in the hook command) \
                 -> mined; PreCompact: \"bogus\" (set by env in settings.json) -> fails to \
                 start: {BOGUS_REASON}"
            )
        );
        assert_eq!(
            c[1].hint.as_deref(),
            Some(format!("{INVALID_MODE_HINT} | {PLUGIN_OVERRIDE_HINT}").as_str())
        );
        assert!(!c[1].hint.as_deref().unwrap().contains(DUPLICATE_HINT));
    }

    #[test]
    fn a_failing_plugin_copy_and_a_failing_hand_wired_copy_do_not_run_twice() {
        let home = home_with_settings(&plugin_with(
            "bogus",
            &[("UserPromptSubmit", "ironmem hook user-prompt-submit")],
        ));
        let c = claude_hook_checks(Some(home.path()), None);
        let s = &c[0].summary;
        assert_eq!(
            *s,
            format!(
                "Claude Code hooks: the hand-wired UserPromptSubmit fails to start: \
                 {BOGUS_REASON} (set by env in settings.json); SessionStart fails to start: \
                 {BOGUS_REASON} (set by env in settings.json), so no memory context at session \
                 start; UserPromptSubmit fails to start: {BOGUS_REASON} (set by env in \
                 settings.json), so prompt recall is off"
            )
        );
        assert!(!s.contains("run twice"), "{s}");
        assert_eq!(c[0].hint.as_deref(), Some(INVALID_MODE_HINT));
    }

    #[test]
    fn hand_wired_copies_that_alone_mine_get_the_override_hint() {
        let mut doc: Value =
            serde_json::from_str(&miners(TRUSTED_STOP, TRUSTED_PRECOMPACT)).unwrap();
        doc["enabledPlugins"] = serde_json::json!({"ironmem@ironrace": true});
        let c = mining_of(&doc.to_string(), Some("read-only"));
        assert_eq!(c.status, CheckStatus::Ok, "{}", c.summary);
        assert_eq!(c.hint.as_deref(), Some(PLUGIN_OVERRIDE_HINT));
        // Without the override the plugin's copies mine by themselves.
        let c = mining_of(&doc.to_string(), None);
        assert_eq!(c.hint, None);
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

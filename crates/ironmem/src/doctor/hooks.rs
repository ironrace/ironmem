//! The Claude Code hook doctor checks (#377): `hooks` and `hook_mining`.
//!
//! A plugin install gets its four hooks from `.claude-plugin/hooks/hooks.json`
//! and runs them through a wrapper that defaults to trusted mode. A non-plugin
//! install gets whatever `~/.claude/settings.json` holds, and hook commands
//! there default to read-only, which skips mining. Neither gap errors at run
//! time, so doctor is where it has to show up. Diagnose-only: never writes.
//! Besides reading settings.json it looks at the file system in one place:
//! [`program_problem`] checks that a wired hook's literal program path (an
//! absolute path or `~/...`) is an executable file.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::hook_command::{
    best_result, source_label, HookCommand, ModeEnv, ModeError, ModeSource, ProgramProblem,
    RunForm, RunMode,
};
use super::{Check, CheckStatus};

/// The key of the check on whether the hooks are wired and start.
pub(super) const HOOKS_KEY: &str = "hooks";
/// The key of the check on whether the wired miners mine.
pub(super) const HOOK_MINING_KEY: &str = "hook_mining";

/// One hook the ironmem plugin wires.
#[derive(Debug)]
pub(super) struct PluginHook {
    /// The Claude Code event that runs it.
    pub(super) event: &'static str,
    /// Its `ironmem hook <name>`.
    pub(super) name: &'static str,
    /// A run with write access mines the workspace's changed files. A unit
    /// test pins this to the hook's own rule, `crate::hook::hook_mines_workspace`.
    pub(super) mines: bool,
    /// What is lost when it does not run (it is missing or exits at startup),
    /// as a full clause. It follows "so" or "and if it does," in the startup
    /// entries (`startup_failure`, `possible_startup_failure`) and "and if it
    /// fails too," in `split_hand_wired`'s hand-wired entries, and is listed
    /// in parentheses, joined by "; ", for missing hooks.
    pub(super) lost: &'static str,
}

/// The plugin hooks. A unit test pins their events and each one's command to
/// `.claude-plugin/hooks/hooks.json`, but not their order; the order here is
/// the one doctor reports them in.
/// scripts/install-ironmem.sh's `CLAUDE_HOOK_EVENTS` mirrors it.
pub(super) static PLUGIN_HOOKS: [PluginHook; 4] = [
    PluginHook {
        event: "SessionStart",
        name: "session-start",
        mines: false,
        lost: "no memory context is loaded at session start",
    },
    PluginHook {
        event: "UserPromptSubmit",
        name: "user-prompt-submit",
        mines: false,
        lost: "prompt recall is off",
    },
    PluginHook {
        event: "Stop",
        name: "stop",
        mines: true,
        lost: "workspace files are not mined when a session stops",
    },
    PluginHook {
        event: "PreCompact",
        name: "precompact",
        mines: true,
        lost: "workspace files are not mined before compaction",
    },
];

/// The events of the plugin hooks that mine, joined with `joiner`, e.g.
/// "Stop and PreCompact".
pub(super) fn miner_events(joiner: &str) -> String {
    PLUGIN_HOOKS
        .iter()
        .filter(|h| h.mines)
        .map(|h| h.event)
        .collect::<Vec<_>>()
        .join(joiner)
}

/// The hint for a `hooks` line that names hooks missing (or no settings.json).
const INSTALL_HINT: &str = "re-run scripts/install-ironmem.sh to wire the missing hooks, or \
install the ironmem Claude Code plugin, whose hooks also mine; the installer wires them \
read-only, which brings back session context and prompt recall, but a hook it wires mines only \
once its command sets IRONMEM_MCP_MODE=trusted (see hook_mining)";

/// Beside [`INSTALL_HINT`] for missing hooks: the installer's matcher reads
/// more command shapes than doctor's (see `hook_command::runs_hook`).
const READ_SHAPE_HINT: &str = "if the installer says a missing hook is already wired, doctor \
could not read its command: make ironmem the command's program, alone or after env, exec, \
command, nohup or bash -c, not behind if, { }, timeout, time, nice or another wrapper";

/// The hint for a `hook_mining` line that finds no miner wired.
const NO_MINER_HINT: &str = "install the ironmem Claude Code plugin, whose hooks mine, or \
re-run scripts/install-ironmem.sh to wire the missing hooks and then set \
IRONMEM_MCP_MODE=trusted in their commands; the installer wires them read-only, which does not \
mine";

/// The hint for bare manual miners that run read-only, naming those `events`.
/// `form` says where their runs sit: an exec-form entry's command (`args`
/// set) cannot take an assignment prefix, as no shell reads it, and a run in
/// a `-c` string of such an entry is fixed inside that string.
fn read_only_hint(events: &[&str], form: RunForm) -> String {
    let events_text = events.join(" and ");
    let one = events.len() == 1;
    let fix = match (form, one) {
        (RunForm::Shell, true) => {
            format!("prefix the {events_text} command with IRONMEM_MCP_MODE=trusted")
        }
        (RunForm::Shell, false) => {
            format!("prefix the {events_text} commands with IRONMEM_MCP_MODE=trusted")
        }
        (RunForm::Exec, true) => format!(
            "in the {events_text} entry (exec form, args set) make env the command and put \
             IRONMEM_MCP_MODE=trusted, then the program, first in args"
        ),
        (RunForm::Exec, false) => format!(
            "in the {events_text} entries (exec form, args set) make env the command and put \
             IRONMEM_MCP_MODE=trusted, then the program, first in args"
        ),
        (RunForm::ExecString, true) => format!(
            "in the {events_text} entry (exec form, args set) put IRONMEM_MCP_MODE=trusted \
             before the ironmem command inside its -c string"
        ),
        (RunForm::ExecString, false) => format!(
            "in the {events_text} entries (exec form, args set) put IRONMEM_MCP_MODE=trusted \
             before the ironmem command inside each -c string"
        ),
    };
    format!(
        "expected outside a plugin install: MCP writes and prompt recall still work. A cheaper \
         way to mine without the plugin is tracked in #418. To mine now, {fix}; each run then \
         loads the embedding model and rebuilds the vector index"
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

const MIS_SHAPED_HINT: &str = "fix hooks in settings.json by hand (it must be an object, and \
each hooks.<event> an array of hook groups), then re-run scripts/install-ironmem.sh, which \
refuses the file until then";

const DISABLE_ALL_HOOKS_HINT: &str = "remove disableAllHooks from settings.json or set it to false";

const MISSING_PROGRAM_HINT: &str = "correct the program path in that settings.json entry, or \
remove the entry and re-run scripts/install-ironmem.sh to wire it again (the installer leaves an \
entry that is already wired as it is)";

const HOME_UNKNOWN: &str = "hook mining: skipped (home directory unknown)";

/// The command shape of a plugin hook, used to work out the mode it runs in.
const PLUGIN_HOOK_COMMAND: &str = "bash ${CLAUDE_PLUGIN_ROOT}/hooks/ironmem-hook.sh";

/// What the hook checks read besides settings.json's commands.
#[derive(Debug, Clone, Copy)]
struct Context<'a> {
    env: ModeEnv<'a>,
    /// The home directory a `~/` program path expands to.
    home: &'a Path,
}

/// Plugin hooks that settings.json wires, with every command matching each.
type WiredHooks = Vec<(&'static PluginHook, Vec<HookCommand>)>;

/// What `~/.claude/settings.json` says about the ironmem hooks.
#[derive(Debug)]
enum HookWiring {
    /// `~/.claude` does not exist: Claude Code is not in use here.
    NoClaude,
    /// `~/.claude` exists but `settings.json` does not.
    NoSettings,
    /// The file exists but cannot be read as a JSON object.
    Unreadable(String),
    /// The file is a JSON object, but its `hooks` is not absent, null, or an
    /// object whose values are all arrays (the shape the installer accepts),
    /// for this reason.
    MisShaped(String),
    /// settings.json sets `disableAllHooks: true`, so Claude Code runs no
    /// ironmem hook, the plugin's included, unless a project's settings set it
    /// back to false (managed-policy hooks still run; doctor does not read
    /// project or managed settings).
    AllHooksDisabled,
    /// The ironmem plugin is enabled; its hooks.json supplies the hooks.
    Plugin {
        /// Plugin hooks that settings.json also wires by hand, with their
        /// commands; `split_hand_wired` works out which copies run.
        hand_wired: WiredHooks,
        /// `env.IRONMEM_MCP_MODE` from settings.json, if set.
        settings_env_mode: Option<String>,
    },
    /// Hooks registered by hand or by scripts/install-ironmem.sh.
    Manual {
        /// The plugin hooks found, in PLUGIN_HOOKS order.
        wired: WiredHooks,
        /// Plugin hooks with no matching command.
        missing: Vec<&'static PluginHook>,
        /// `env.IRONMEM_MCP_MODE` from settings.json, if set.
        settings_env_mode: Option<String>,
    },
}

/// Build the `hooks` and `hook_mining` checks for Claude Code: exactly those
/// two keys, in that order, so doctor's key set is stable.
/// `process_env_mode` is doctor's own `IRONMEM_MCP_MODE`, passed in so tests
/// don't mutate the process environment.
pub(super) fn claude_hook_checks(
    home: Option<&Path>,
    process_env_mode: Option<&str>,
) -> [Check; 2] {
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
        HookWiring::NoSettings => [
            Check::new(
                HOOKS_KEY,
                CheckStatus::Warn,
                format!(
                    "Claude Code hooks: none wired ({} does not exist): \
                     prompt recall is off and workspace files are not mined",
                    path.display()
                ),
            )
            .with_hint(INSTALL_HINT),
            no_miner_check(),
        ],
        HookWiring::Unreadable(e) => [
            Check::new(
                HOOKS_KEY,
                CheckStatus::Warn,
                format!("Claude Code hooks: cannot read {}: {e}", path.display()),
            )
            .with_hint("fix the file by hand, then re-run scripts/install-ironmem.sh"),
            Check::new(
                HOOK_MINING_KEY,
                CheckStatus::Info,
                "hook mining: skipped (settings.json unreadable)",
            ),
        ],
        HookWiring::MisShaped(reason) => [
            Check::new(
                HOOKS_KEY,
                CheckStatus::Warn,
                format!(
                    "Claude Code hooks: cannot read the hooks in {}: {reason}",
                    path.display()
                ),
            )
            .with_hint(MIS_SHAPED_HINT),
            Check::new(
                HOOK_MINING_KEY,
                CheckStatus::Info,
                "hook mining: skipped (the hooks in settings.json cannot be read)",
            ),
        ],
        HookWiring::AllHooksDisabled => [
            Check::new(
                HOOKS_KEY,
                CheckStatus::Warn,
                format!(
                    "Claude Code hooks: {} sets disableAllHooks to true, so Claude Code runs no \
                     ironmem hook unless a project's settings set it back to false: no memory \
                     context is loaded at session start, prompt recall is off and no hook mines \
                     workspace files",
                    path.display()
                ),
            )
            .with_hint(DISABLE_ALL_HOOKS_HINT),
            Check::new(
                HOOK_MINING_KEY,
                CheckStatus::Info,
                "hook mining: skipped (disableAllHooks is true in settings.json; see the hooks line)",
            ),
        ],
        HookWiring::Plugin {
            hand_wired,
            settings_env_mode,
        } => {
            let wired: WiredHooks = PLUGIN_HOOKS
                .iter()
                .map(|hook| {
                    let command = format!("{PLUGIN_HOOK_COMMAND} {}", hook.name);
                    (hook, vec![HookCommand::Shell(command)])
                })
                .collect();
            let ctx = Context {
                env: ModeEnv {
                    settings: settings_env_mode.as_deref(),
                    process: process_env_mode,
                },
                home,
            };
            // A hand-wired copy runs as well as the plugin's, so it can start,
            // recall and mine in the plugin copy's place.
            let candidates: WiredHooks = wired
                .iter()
                .map(|(hook, commands)| {
                    let extra = hand_wired.iter().filter(|(h, _)| h.event == hook.event);
                    let all = commands.iter().chain(extra.flat_map(|(_, c)| c)).cloned();
                    (*hook, all.collect())
                })
                .collect();
            let split = split_hand_wired(&wired, &hand_wired, ctx);
            // A `Maybe` here needs a dynamic hand-wired copy (the plugin's copy
            // never has one), which `split_hand_wired` already reports.
            let (failures, _) = startup_failures(&candidates, ctx);
            let mut mining = mining_check(&candidates, ctx, true);
            if mined_only_by_hand_wired(&wired, &candidates, ctx) {
                add_hint(&mut mining, PLUGIN_OVERRIDE_HINT);
            }
            [plugin_hooks_check(&split, &failures), mining]
        }
        HookWiring::Manual {
            wired,
            missing,
            settings_env_mode,
        } => {
            let ctx = Context {
                env: ModeEnv {
                    settings: settings_env_mode.as_deref(),
                    process: process_env_mode,
                },
                home,
            };
            let (failures, may_fail) = startup_failures(&wired, ctx);
            [
                hooks_check(&path, &missing, &failures, &may_fail),
                mining_check(&wired, ctx, false),
            ]
        }
    }
}

/// True when the run's mode lets the hook mine: the hook's own gate.
fn can_mine(run: &RunMode) -> bool {
    run.mode.as_ref().is_ok_and(|mode| mode.allows_writes())
}

/// True when some plugin miner mines only through a hand-wired copy: the
/// plugin's own command can't mine (an override wins over the wrapper's
/// default), so removing the copies, as the `hooks` hint says, would stop
/// mining.
fn mined_only_by_hand_wired(wired: &WiredHooks, candidates: &WiredHooks, ctx: Context<'_>) -> bool {
    let mines = |hooks: &WiredHooks, event: &str| {
        hooks
            .iter()
            .filter(|(h, _)| h.event == event)
            .any(|(h, commands)| can_mine(&event_mode(h, commands, ctx)))
    };
    PLUGIN_HOOKS
        .iter()
        .filter(|h| h.mines)
        .any(|h| !mines(wired, h.event) && mines(candidates, h.event))
}

/// Append `hint` to `check`'s hint unless it is already there.
fn add_hint(check: &mut Check, hint: &str) {
    check.hint = match check.hint.take() {
        Some(h) if h.split(HINT_JOINER).any(|part| part == hint) => Some(h),
        Some(h) => Some(format!("{h}{HINT_JOINER}{hint}")),
        None => Some(hint.to_owned()),
    };
}

fn skipped(hooks_summary: &str, mining_summary: &str) -> [Check; 2] {
    [
        Check::new(HOOKS_KEY, CheckStatus::Info, hooks_summary),
        Check::new(HOOK_MINING_KEY, CheckStatus::Info, mining_summary),
    ]
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
    if let Some(reason) = hooks_shape_problem(&doc) {
        return HookWiring::MisShaped(reason);
    }
    // Claude Code's documented switch: `"disableAllHooks": true` turns every
    // hook off; any other value is ignored. Doctor reads only
    // ~/.claude/settings.json, so a project or managed settings file that
    // sets it back to false (or sets it at all) is out of scope here.
    if doc.get("disableAllHooks") == Some(&Value::Bool(true)) {
        return HookWiring::AllHooksDisabled;
    }
    let mut wired = Vec::new();
    let mut missing = Vec::new();
    for hook in &PLUGIN_HOOKS {
        let matching: Vec<HookCommand> = commands_for(&doc, hook.event)
            .into_iter()
            .filter(|c| c.runs_hook(hook.name))
            .collect();
        if matching.is_empty() {
            missing.push(hook);
        } else {
            wired.push((hook, matching));
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

/// Why `doc.hooks` is not a shape doctor and the installer read, if it is
/// not: readable is absent, null, or an object whose values are all arrays
/// (scripts/install-ironmem.sh's rule). Junk inside an event's array is not
/// a shape problem; [`commands_for`] skips it, as the installer does.
fn hooks_shape_problem(doc: &Value) -> Option<String> {
    let hooks = match doc.get("hooks") {
        None | Some(Value::Null) => return None,
        Some(Value::Object(hooks)) => hooks,
        Some(_) => return Some("hooks is not an object".to_owned()),
    };
    let mut bad: Vec<String> = hooks
        .iter()
        .filter(|(_, v)| !v.is_array())
        .map(|(event, _)| format!("hooks.{event}"))
        .collect();
    bad.sort();
    match bad.len() {
        0 => None,
        1 => Some(format!("{} is not an array", bad[0])),
        _ => Some(format!("{} are not arrays", bad.join(" and "))),
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

/// Every hook entry's command under `hooks.<event>[].hooks[]`, skipping
/// anything not shaped like that rather than failing.
fn commands_for(doc: &Value, event: &str) -> Vec<HookCommand> {
    doc.get("hooks")
        .and_then(|h| h.get(event))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(hook_command)
        .collect()
}

/// One hook entry's command: shell form when it has no `args`, exec form
/// when `args` is an array of strings (Claude Code's hooks docs: "A command
/// hook runs as exec form when `args` is set, and shell form when `args` is
/// omitted"). An entry whose `command` is not a string, or whose `args` is
/// set to anything else (`null` included), is skipped, as other junk is; the
/// installer's `_claude_hook_is_wired` skips the same entries.
fn hook_command(hook: &Value) -> Option<HookCommand> {
    let command = hook.get("command")?.as_str()?.to_owned();
    match hook.get("args") {
        None => Some(HookCommand::Shell(command)),
        Some(Value::Array(args)) => args
            .iter()
            .map(|arg| arg.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .map(|args| HookCommand::Exec {
                program: command,
                args,
            }),
        Some(_) => None,
    }
}

/// Every run of `hook` in `commands`, in order, with a run whose literal
/// program path is not a runnable file marked [`ModeError::NoProgram`].
fn runs(hook: &PluginHook, commands: &[HookCommand], ctx: Context<'_>) -> Vec<RunMode> {
    commands
        .iter()
        .flat_map(|c| c.hook_modes(hook.name, ctx.env))
        .map(|mut run| {
            if let Some(program) = run.program.clone() {
                if let Some(problem) = program_problem(&program, ctx.home) {
                    run.mode = Err(ModeError::NoProgram(program, problem));
                }
            }
            run
        })
        .collect()
}

/// What is wrong with `program`, a literal path as the command has it (`~/`
/// expands to `home`), or `None` when it is an executable file or doctor
/// cannot tell (an error other than not-found, such as a permission error).
/// On unix any execute bit counts; elsewhere any file does.
fn program_problem(program: &str, home: &Path) -> Option<ProgramProblem> {
    let path = match program.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(program),
    };
    let meta = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(ProgramProblem::Missing),
        Err(_) => return None,
    };
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = true;
    (!(meta.is_file() && executable)).then_some(ProgramProblem::NotExecutable)
}

/// The one mode result reported for `hook`, with its raw value: every run of
/// it in its matching commands, reduced by [`best_result`] (can mine without
/// doctor's shell, then via the shell, then unknown, then cannot mine, then
/// does not start; settings order breaks ties).
fn event_mode(hook: &PluginHook, commands: &[HookCommand], ctx: Context<'_>) -> RunMode {
    best_result(runs(hook, commands, ctx))
}

/// Why `Config::load` rejects the raw mode value `raw`, with it quoted.
fn invalid_reason(raw: &str) -> String {
    format!("IRONMEM_MCP_MODE={raw:?} is not trusted, read-only or restricted")
}

/// Why every run of a hook fails to start.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// `Config::load` rejects this raw mode, from this source.
    Invalid(String, ModeSource),
    /// The program is this literal path, which is not a runnable file.
    NoProgram(String, ProgramProblem),
}

impl Failure {
    /// The clause after a hook's name, e.g. "fails to start: ... (set in the
    /// hook command)" or "is wired to /old/bin/ironmem, which does not exist".
    fn clause(&self) -> String {
        match self {
            Self::Invalid(raw, source) => format!(
                "fails to start: {} ({})",
                invalid_reason(raw),
                source_label(*source)
            ),
            Self::NoProgram(path, problem) => {
                format!("is wired to {path}, which {}", problem.describe())
            }
        }
    }

    /// The hint that fixes it.
    fn hint(&self) -> &'static str {
        match self {
            Self::Invalid(..) => INVALID_MODE_HINT,
            Self::NoProgram(..) => MISSING_PROGRAM_HINT,
        }
    }
}

/// `hook` does not start, for `failure`, so its [`PluginHook::lost`].
fn startup_failure(hook: &PluginHook, failure: &Failure) -> String {
    format!("{} {}, so {}", hook.event, failure.clause(), hook.lost)
}

/// `hook` may exit at startup: its mode `raw` is built by the shell.
fn possible_startup_failure(hook: &PluginHook, raw: &str, source: ModeSource) -> String {
    format!(
        "{}'s mode cannot be determined statically (IRONMEM_MCP_MODE={raw}, {}), so it \
         may fail to start, and if it does, {}",
        hook.event,
        source_label(source),
        hook.lost
    )
}

/// The startup outcome of the hooks that do not mine (SessionStart,
/// UserPromptSubmit), by [`starts`]: entries for those that surely fail
/// (`No`: no run starts, for an invalid mode or a missing program), each with
/// the hint that fixes it, then entries for those that may fail (`Maybe`: no
/// run surely starts and one's mode is built by the shell). Each entry ends
/// with its hook's [`PluginHook::lost`]. Miners are left to `hook_mining`,
/// which reports both outcomes for them.
fn startup_failures(
    wired: &WiredHooks,
    ctx: Context<'_>,
) -> (Vec<(String, &'static str)>, Vec<String>) {
    let (mut fail, mut may_fail) = (Vec::new(), Vec::new());
    for (hook, commands) in wired {
        if hook.mines {
            continue;
        }
        match starts(hook, commands, ctx) {
            Starts::No(failure) => fail.push((startup_failure(hook, &failure), failure.hint())),
            Starts::Maybe(raw, source) => {
                may_fail.push(possible_startup_failure(hook, &raw, source))
            }
            Starts::Yes => {}
        }
    }
    (fail, may_fail)
}

/// Push `hint` unless `hints` has it already.
fn push_unique(hints: &mut Vec<&'static str>, hint: &'static str) {
    if !hints.contains(&hint) {
        hints.push(hint);
    }
}

/// Whether any of an event's copies starts at all.
#[derive(Debug, PartialEq, Eq)]
enum Starts {
    /// Some run of the hook has a valid mode and a program that exists.
    Yes,
    /// No run surely starts, but one's mode is built by the shell: this raw
    /// value, from this source.
    Maybe(String, ModeSource),
    /// No run starts; why the first one does not.
    No(Failure),
}

/// Whether `commands`, the copies of `hook`, start: [`Starts::Yes`] when
/// any run of the hook has a valid mode and a program that exists, across
/// commands and within one, so a dynamic or failing run never hides a working
/// one.
fn starts(hook: &PluginHook, commands: &[HookCommand], ctx: Context<'_>) -> Starts {
    let runs = runs(hook, commands, ctx);
    // No runs at all reads as the unset default, which starts.
    if runs.is_empty() || runs.iter().any(|r| r.mode.is_ok()) {
        return Starts::Yes;
    }
    let unknown = runs.iter().find_map(|r| match &r.mode {
        Err(ModeError::Unknown(raw)) => Some(Starts::Maybe(raw.clone(), r.source)),
        _ => None,
    });
    let failed = runs.iter().find_map(|r| match &r.mode {
        Err(ModeError::Invalid(raw)) => Some(Starts::No(Failure::Invalid(raw.clone(), r.source))),
        Err(ModeError::NoProgram(path, problem)) => {
            Some(Starts::No(Failure::NoProgram(path.clone(), *problem)))
        }
        _ => None,
    });
    unknown.or(failed).unwrap_or(Starts::Yes)
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
fn split_hand_wired(wired: &WiredHooks, hand_wired: &WiredHooks, ctx: Context<'_>) -> HandWired {
    let mut out = HandWired {
        duplicated: Vec::new(),
        entries: Vec::new(),
        hints: Vec::new(),
    };
    for (hook, commands) in hand_wired {
        let event = hook.event;
        let plugin_copy = wired.iter().find(|(h, _)| h.event == event).map(|(_, c)| c);
        // Never `None`: `wired` holds all of PLUGIN_HOOKS and `hand_wired`
        // only hooks from it. Doctor stays total, so no `expect`.
        let plugin = plugin_copy.map_or(Starts::Yes, |c| starts(hook, c, ctx));
        // `plugin` is never `Starts::Maybe` (pinned by
        // `a_plugin_copy_never_has_a_dynamic_mode`), so the `_` arms below
        // stand for `Starts::Yes`.
        let copy = starts(hook, commands, ctx);
        let (entry, hints): (String, Vec<&'static str>) = match (plugin, copy) {
            (Starts::No(failure), Starts::Yes) => (
                format!(
                    "the plugin's {event} {}, so only the hand-wired copy runs",
                    failure.clause()
                ),
                vec![failure.hint()],
            ),
            (Starts::No(failure), Starts::Maybe(copy_raw, copy_source)) => (
                format!(
                    "the plugin's {event} {}, so only the hand-wired copy may run: its mode \
                     cannot be determined statically (IRONMEM_MCP_MODE={copy_raw}, {}), and if \
                     it fails too, {}",
                    failure.clause(),
                    source_label(copy_source),
                    hook.lost
                ),
                vec![failure.hint(), DYNAMIC_MODE_HINT],
            ),
            (Starts::No(..), Starts::No(failure)) => (
                format!("the hand-wired {event} {}", failure.clause()),
                vec![failure.hint()],
            ),
            (_, Starts::Yes) => {
                out.duplicated.push(event);
                continue;
            }
            (_, Starts::Maybe(raw, source)) => (
                format!(
                    "the hand-wired {event}'s mode cannot be determined statically \
                     (IRONMEM_MCP_MODE={raw}, {}), so the plugin's copy runs and the \
                     hand-wired copy may run too",
                    source_label(source)
                ),
                vec![DUPLICATE_HINT],
            ),
            (_, Starts::No(failure)) => (
                format!(
                    "the hand-wired {event} {}, so only the plugin's copy runs",
                    failure.clause()
                ),
                vec![DUPLICATE_HINT],
            ),
        };
        out.entries.push(entry);
        out.hints.extend(hints);
    }
    out
}

/// The plugin path's `hooks` check: the run-twice events from `hand_wired`,
/// then its other entries, then `failures` (the startup failures). Hints
/// follow that order, each given once.
fn plugin_hooks_check(hand_wired: &HandWired, failures: &[(String, &'static str)]) -> Check {
    let HandWired {
        duplicated,
        entries,
        hints: entry_hints,
    } = hand_wired;
    if duplicated.is_empty() && entries.is_empty() && failures.is_empty() {
        return Check::new(
            HOOKS_KEY,
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
        let texts: Vec<&str> = failures.iter().map(|(text, _)| text.as_str()).collect();
        problems.push(texts.join("; "));
        hints.extend(failures.iter().map(|(_, hint)| *hint));
    }
    let mut unique: Vec<&'static str> = Vec::new();
    for hint in hints {
        push_unique(&mut unique, hint);
    }
    Check::new(
        HOOKS_KEY,
        CheckStatus::Warn,
        format!("Claude Code hooks: {}", problems.join("; ")),
    )
    .with_hint(unique.join(HINT_JOINER))
}

/// The manual install's `hooks` check: missing hooks, then hooks that fail to
/// start (`failures`), then hooks that may (`may_fail`), each with one hint.
fn hooks_check(
    path: &Path,
    missing: &[&PluginHook],
    failures: &[(String, &'static str)],
    may_fail: &[String],
) -> Check {
    if missing.is_empty() && failures.is_empty() && may_fail.is_empty() {
        return Check::new(
            HOOKS_KEY,
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
        let events: Vec<&str> = missing.iter().map(|h| h.event).collect();
        let effects: Vec<&str> = missing.iter().map(|h| h.lost).collect();
        problems.push(format!(
            "missing {} in {} ({})",
            events.join(", "),
            path.display(),
            effects.join("; ")
        ));
        push_unique(&mut hints, INSTALL_HINT);
        push_unique(&mut hints, READ_SHAPE_HINT);
    }
    if !failures.is_empty() {
        let texts: Vec<&str> = failures.iter().map(|(text, _)| text.as_str()).collect();
        problems.push(texts.join("; "));
        for (_, hint) in failures {
            push_unique(&mut hints, hint);
        }
    }
    // Warn, as hook_mining does for a mode it cannot determine.
    if !may_fail.is_empty() {
        problems.push(may_fail.join("; "));
        push_unique(&mut hints, DYNAMIC_MODE_HINT);
    }
    Check::new(
        HOOKS_KEY,
        CheckStatus::Warn,
        format!("Claude Code hooks: {}", problems.join("; ")),
    )
    .with_hint(hints.join(HINT_JOINER))
}

fn no_miner_check() -> Check {
    Check::new(
        HOOK_MINING_KEY,
        CheckStatus::Warn,
        format!(
            "no {} hook is wired: workspace files are not mined",
            miner_events(" or ")
        ),
    )
    .with_hint(NO_MINER_HINT)
}

/// Why a wired miner does not mine. Raw values are the `IRONMEM_MCP_MODE`
/// text as the file or shell has it, so hints quote it, not a normalised label.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NoMineCause {
    /// The mode is invalid, so the hook exits at startup.
    Invalid,
    /// The program path is not a runnable file, so the hook does not start.
    NoProgram,
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
    /// Its [`RunForm`] says where a fix puts the assignment.
    Default(RunForm),
}

/// One hint's cause and the miners it applies to, in entry order.
struct CauseGroup {
    cause: NoMineCause,
    events: Vec<&'static str>,
}

impl NoMineCause {
    fn of(source: ModeSource, raw: Option<String>, plugin: bool, form: RunForm) -> Self {
        let raw = raw.unwrap_or_default();
        match source {
            ModeSource::SettingsEnv | ModeSource::Environment if plugin => Self::PluginOverride,
            ModeSource::Inline => Self::Inline(raw),
            ModeSource::SettingsEnv => Self::SettingsEnv(raw),
            ModeSource::Environment => Self::Shell(raw),
            ModeSource::PluginWrapper | ModeSource::Default => Self::Default(form),
        }
    }

    fn hint(&self, events: &[&str]) -> String {
        match self {
            Self::Invalid => INVALID_MODE_HINT.to_owned(),
            Self::NoProgram => MISSING_PROGRAM_HINT.to_owned(),
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
                "doctor ran with IRONMEM_MCP_MODE={raw:?}, and a Claude Code launched from this \
                 shell inherits it; launch Claude Code with IRONMEM_MCP_MODE=trusted to mine"
            ),
            Self::Default(form) => read_only_hint(events, *form),
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

/// A headline that holds in every mix of mined, not-mined (including fails to
/// start) and unknown miners, naming each group. The not-mined group comes
/// last: "<Event> is not" right after "is mined", "<Event> is not mined" after
/// "may not be mined".
fn mining_headline(mined: &[&str], not_mined: &[&str], unknown: &[&str]) -> String {
    if mined.is_empty() && unknown.is_empty() {
        return "no wired hook mines".to_owned();
    }
    let mut parts = Vec::new();
    if mined.is_empty() {
        parts.push("no wired hook is known to mine".to_owned());
    } else {
        parts.push(format!("{} mined", events_with_verb(mined)));
    }
    if !unknown.is_empty() {
        parts.push(format!("{} may not be mined", unknown.join(" and ")));
    }
    if !not_mined.is_empty() {
        // After "may not be mined", a bare "is not" reads ambiguously.
        let tail = if unknown.is_empty() {
            "not"
        } else {
            "not mined"
        };
        parts.push(format!("{} {tail}", events_with_verb(not_mined)));
    }
    parts.join("; ")
}

/// The `hook_mining` check: a headline, then one `<Event>: <mode> (<source>)
/// -> <outcome>` entry per wired miner, joined with `; ` because doctor
/// renders each summary as one line. Ok only when every wired miner mines
/// (its mode passes [`can_mine`], the hook's own gate); the hint holds one
/// entry per distinct [`NoMineCause`], in the order of the entries, joined
/// with [`HINT_JOINER`]. `plugin` says the miners run on the plugin path (the
/// plugin's hooks plus any hand-wired copies).
fn mining_check(wired: &WiredHooks, ctx: Context<'_>, plugin: bool) -> Check {
    let miners: Vec<&(&'static PluginHook, Vec<HookCommand>)> =
        wired.iter().filter(|(hook, _)| hook.mines).collect();
    if miners.is_empty() {
        return no_miner_check();
    }
    let (mut mined, mut not_mined, mut unknown) = (Vec::new(), Vec::new(), Vec::new());
    let mut lines = Vec::new();
    let mut causes: Vec<CauseGroup> = Vec::new();
    for (hook, commands) in miners {
        let event = hook.event;
        let run = event_mode(hook, commands, ctx);
        let label = source_label(run.source);
        let mines = can_mine(&run);
        lines.push(match run.mode {
            Ok(mode) if mines => {
                mined.push(event);
                if run.source == ModeSource::Environment {
                    format!(
                        "{event}: {} (inherited from this shell; Claude Code must be \
                         launched with it) -> mined",
                        mode.as_str()
                    )
                } else {
                    format!("{event}: {} ({label}) -> mined", mode.as_str())
                }
            }
            Ok(mode) => {
                not_mined.push(event);
                add_cause(
                    &mut causes,
                    NoMineCause::of(run.source, run.raw, plugin, run.form),
                    event,
                );
                format!("{event}: {} ({label}) -> not mined", mode.as_str())
            }
            Err(ModeError::Invalid(raw)) => {
                not_mined.push(event);
                add_cause(&mut causes, NoMineCause::Invalid, event);
                format!(
                    "{event}: {raw:?} ({label}) -> fails to start: {}",
                    invalid_reason(&raw)
                )
            }
            Err(ModeError::NoProgram(path, problem)) => {
                not_mined.push(event);
                add_cause(&mut causes, NoMineCause::NoProgram, event);
                format!(
                    "{event}: wired to {path}, which {} -> does not run",
                    problem.describe()
                )
            }
            Err(ModeError::Unknown(raw)) => {
                unknown.push(event);
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
    let check = Check::new(HOOK_MINING_KEY, status, summary);
    if causes.is_empty() {
        return check;
    }
    let hints: Vec<String> = causes.iter().map(|g| g.cause.hint(&g.events)).collect();
    check.with_hint(hints.join(HINT_JOINER))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create an executable file at `path`, with its parent directories.
    fn make_executable(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// A home whose `~/.claude/settings.json` holds `settings` and whose
    /// `~/.ironrace/bin/ironmem` is an executable file, so the `~/` commands
    /// below name a program that exists.
    fn home_with_settings(settings: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        make_executable(&dir.path().join(".ironrace/bin/ironmem"));
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
        for hook in &PLUGIN_HOOKS {
            let (event, name) = (hook.event, hook.name);
            let cmd = hooks[event][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(
                HookCommand::Shell(cmd.to_owned()).runs_hook(name),
                "{event}: `{cmd}` should run hook {name}"
            );
            assert_eq!(cmd, format!("{PLUGIN_HOOK_COMMAND} {name}"), "{event}");
        }
    }

    #[test]
    fn plugin_hook_mines_flags_follow_the_hooks_own_rule() {
        for hook in &PLUGIN_HOOKS {
            assert_eq!(
                hook.mines,
                crate::hook::hook_mines_workspace(hook.name),
                "{}",
                hook.event
            );
        }
        assert_eq!(miner_events(" and "), "Stop and PreCompact");
    }

    #[test]
    fn the_plugin_wrapper_defaults_to_the_mode_doctor_assumes() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.claude-plugin");
        let wrapper = std::fs::read_to_string(format!("{root}/bin/ironmem-mcp.sh")).unwrap();
        let lines: Vec<&str> = wrapper
            .lines()
            .filter(|l| l.trim_start().starts_with("export IRONMEM_MCP_MODE="))
            .collect();
        assert_eq!(lines.len(), 1, "{lines:?}");
        let default = lines[0]
            .trim()
            .strip_prefix(r#"export IRONMEM_MCP_MODE="${IRONMEM_MCP_MODE:-"#)
            .and_then(|rest| rest.strip_suffix(r#"}""#))
            .unwrap_or_else(|| panic!("unexpected wrapper line: {}", lines[0]));
        assert_eq!(
            crate::config::McpAccessMode::parse(default).unwrap(),
            super::super::hook_command::WRAPPER_DEFAULT
        );
        // The plugin's hook script runs its hooks through that wrapper.
        let hook_script = std::fs::read_to_string(format!("{root}/hooks/ironmem-hook.sh")).unwrap();
        assert!(
            hook_script.contains(r#""$PLUGIN_ROOT/bin/ironmem-mcp.sh" hook "$HOOK_NAME""#),
            "{hook_script}"
        );
    }

    /// Every [`McpAccessMode`]; the match fails to compile when one is added.
    fn every_mode() -> [crate::config::McpAccessMode; 3] {
        use crate::config::McpAccessMode::*;
        let all = [Trusted, ReadOnly, Restricted];
        for mode in all {
            match mode {
                Trusted | ReadOnly | Restricted => {}
            }
        }
        all
    }

    #[test]
    fn a_miner_is_mined_exactly_when_its_mode_allows_writes() {
        for mode in every_mode() {
            let raw = mode.as_str();
            let stop = format!("IRONMEM_MCP_MODE={raw} ironmem hook stop");
            let c = mining_of(&settings_with(&[("Stop", stop.as_str())]), None);
            let mined = format!("Stop: {raw} (set in the hook command) -> mined");
            let not_mined = format!("Stop: {raw} (set in the hook command) -> not mined");
            if mode.allows_writes() {
                assert_eq!(c.status, CheckStatus::Ok, "{raw}: {}", c.summary);
                assert!(c.summary.contains(&mined), "{raw}: {}", c.summary);
            } else {
                assert_eq!(c.status, CheckStatus::Warn, "{raw}: {}", c.summary);
                assert!(c.summary.contains(&not_mined), "{raw}: {}", c.summary);
            }
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

    /// The commands scripts/install-ironmem.sh writes for an install path the
    /// shell would split: the path single-quoted, an embedded `'` as `'\''`.
    #[test]
    fn single_quoted_install_paths_are_wired_and_their_mode_is_read() {
        let bins = tempfile::tempdir().unwrap();
        let root = bins.path().display();
        make_executable(&bins.path().join("Jo Smith/.ironrace/bin/ironmem"));
        make_executable(&bins.path().join("Jo Smith/it's here/bin/ironmem"));
        for program in [
            format!("'{root}/Jo Smith/.ironrace/bin/ironmem'"),
            format!(r"'{root}/Jo Smith/it'\''s here/bin/ironmem'"),
        ] {
            let cmds: Vec<(&str, String)> = PLUGIN_HOOKS
                .iter()
                .map(|h| {
                    let n = h.name;
                    (h.event, format!("{program} hook {n} --harness claude-code"))
                })
                .collect();
            let refs: Vec<(&str, &str)> = cmds.iter().map(|(e, c)| (*e, c.as_str())).collect();
            let home = home_with_settings(&settings_with(&refs));
            let checks = claude_hook_checks(Some(home.path()), None);
            assert_eq!(checks[0].status, CheckStatus::Ok, "{}", checks[0].summary);
            assert!(
                checks[0].summary.contains("all 4 wired"),
                "{}",
                checks[0].summary
            );
            assert!(
                checks[1].summary.contains(
                    "Stop: read-only (IRONMEM_MCP_MODE unset, so the default) -> not mined"
                ),
                "{program}: {}",
                checks[1].summary
            );

            let trusted: Vec<(&str, String)> = cmds
                .iter()
                .map(|(e, c)| (*e, format!("IRONMEM_MCP_MODE=trusted {c}")))
                .collect();
            let refs: Vec<(&str, &str)> = trusted.iter().map(|(e, c)| (*e, c.as_str())).collect();
            let home = home_with_settings(&settings_with(&refs));
            let checks = claude_hook_checks(Some(home.path()), None);
            assert_eq!(
                checks[1].status,
                CheckStatus::Ok,
                "{program}: {}",
                checks[1].summary
            );
            assert!(
                checks[1]
                    .summary
                    .contains("Stop: trusted (set in the hook command) -> mined"),
                "{program}: {}",
                checks[1].summary
            );
        }
        // Unquoted, the shell runs `/Users/Jo`, so the hook is missing.
        let unquoted = "/Users/Jo Smith/.ironrace/bin/ironmem hook stop --harness claude-code";
        let home = home_with_settings(&settings_with(&[("Stop", unquoted)]));
        let checks = claude_hook_checks(Some(home.path()), None);
        assert!(
            checks[0]
                .summary
                .contains("missing SessionStart, UserPromptSubmit, Stop, PreCompact"),
            "{}",
            checks[0].summary
        );
    }

    #[test]
    fn missing_hooks_are_named_with_their_effect() {
        let home = home_with_settings(&settings_with(&[ALL_FOUR_BARE[2]]));
        let checks = claude_hook_checks(Some(home.path()), None);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        let s = &checks[0].summary;
        let path = home.path().join(".claude").join("settings.json");
        assert_eq!(
            *s,
            format!(
                "Claude Code hooks: missing SessionStart, UserPromptSubmit, PreCompact in {} \
                 (no memory context is loaded at session start; prompt recall is off; \
                 workspace files are not mined before compaction)",
                path.display()
            )
        );
        assert!(checks[0]
            .hint
            .as_deref()
            .unwrap()
            .contains("install-ironmem.sh"));
    }

    #[test]
    fn install_hints_say_what_the_installer_brings_back() {
        // No settings.json: the hooks line points at the installer or the
        // plugin; the mining line says the installer alone does not mine.
        let no_file = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(no_file.path().join(".claude")).unwrap();
        let c = claude_hook_checks(Some(no_file.path()), None);
        assert_eq!(
            c[0].hint.as_deref(),
            Some(
                "re-run scripts/install-ironmem.sh to wire the missing hooks, or install the \
                 ironmem Claude Code plugin, whose hooks also mine; the installer wires them \
                 read-only, which brings back session context and prompt recall, but a hook it \
                 wires mines only once its command sets IRONMEM_MCP_MODE=trusted (see \
                 hook_mining)"
            )
        );
        assert_eq!(
            c[1].hint.as_deref(),
            Some(
                "install the ironmem Claude Code plugin, whose hooks mine, or re-run \
                 scripts/install-ironmem.sh to wire the missing hooks and then set \
                 IRONMEM_MCP_MODE=trusted in their commands; the installer wires them read-only, \
                 which does not mine"
            )
        );
        // A missing hook adds what to do when the installer calls it wired.
        let home = home_with_settings(&settings_with(&ALL_FOUR_BARE[..3]));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(
            c[0].hint.as_deref(),
            Some(
                format!(
                    "{INSTALL_HINT} | if the installer says a missing hook is already wired, \
                     doctor could not read its command: make ironmem the command's program, \
                     alone or after env, exec, command, nohup or bash -c, not behind if, {{ }}, \
                     timeout, time, nice or another wrapper"
                )
                .as_str()
            )
        );
    }

    #[test]
    fn no_miner_wired_warns() {
        let home = home_with_settings(&settings_with(&ALL_FOUR_BARE[..2]));
        let checks = claude_hook_checks(Some(home.path()), None);
        assert_eq!(checks[1].status, CheckStatus::Warn);
        assert_eq!(
            checks[1].summary,
            "no Stop or PreCompact hook is wired: workspace files are not mined"
        );
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
    fn only_an_ironmem_plugin_key_set_to_true_enables_the_plugin() {
        // The same answers as scripts/install-ironmem.sh's enabledPlugins jq.
        for (plugins, enabled) in [
            (r#"{"ironmem@ironrace": true}"#, true),
            (r#"{"ironmem": true}"#, true),
            (r#"{"ironmem-extras@x": true}"#, false),
            (r#"{"notironmem@x": true}"#, false),
            (r#"{"ironmem@x": "true"}"#, false),
        ] {
            let doc: Value =
                serde_json::from_str(&format!(r#"{{"enabledPlugins": {plugins}}}"#)).unwrap();
            assert_eq!(plugin_enabled(&doc), enabled, "{plugins}");
        }
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
        assert!(
            c[0].summary.ends_with(
                "does not exist): prompt recall is off and workspace files are not mined"
            ),
            "{}",
            c[0].summary
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
        // An event value that is not an array: the installer refuses the
        // file, so doctor says it cannot read the hooks, not that they are
        // missing (whose hint, re-run the installer, could not fix it).
        let home = home_with_settings(
            r#"{"hooks": {"Stop": {"not": "an array"}, "PreCompact": [{"hooks": "x"}, 3]}}"#,
        );
        let checks = claude_hook_checks(Some(home.path()), None);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        assert_eq!(checks[1].status, CheckStatus::Info, "{}", checks[1].summary);
        assert_eq!(
            checks[1].summary,
            "hook mining: skipped (the hooks in settings.json cannot be read)"
        );
        // Junk nested inside an event's array is skipped silently, as the
        // installer does: the hooks read as missing.
        let home = home_with_settings(
            r#"{"hooks": {"PreCompact": [{"hooks": "x"}, 3, {"hooks": [{"command": 5}, "s"]}]}}"#,
        );
        let checks = claude_hook_checks(Some(home.path()), None);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        assert!(
            checks[0]
                .summary
                .contains("missing SessionStart, UserPromptSubmit, Stop, PreCompact"),
            "{}",
            checks[0].summary
        );
        assert_eq!(checks[1].status, CheckStatus::Warn, "{}", checks[1].summary);
        assert!(checks[1]
            .summary
            .contains("no Stop or PreCompact hook is wired"));
    }

    #[test]
    fn a_mis_shaped_hooks_block_names_the_offending_key() {
        for (settings, reason) in [
            (r#"{"hooks": {"Stop": {}}}"#, "hooks.Stop is not an array"),
            (r#"{"hooks": {"Stop": null}}"#, "hooks.Stop is not an array"),
            (
                r#"{"hooks": {"Stop": {}, "PreCompact": 3, "SessionStart": []}}"#,
                "hooks.PreCompact and hooks.Stop are not arrays",
            ),
            (r#"{"hooks": []}"#, "hooks is not an object"),
            (r#"{"hooks": "x"}"#, "hooks is not an object"),
            // scripts/install-ironmem.sh refuses both `false`s too
            // (test_hooks_false_is_refused_as_a_shape_problem), so the hint's
            // "refuses the file until then" holds.
            (r#"{"hooks": false}"#, "hooks is not an object"),
            (
                r#"{"hooks": {"Stop": false}}"#,
                "hooks.Stop is not an array",
            ),
        ] {
            let home = home_with_settings(settings);
            let c = claude_hook_checks(Some(home.path()), None);
            let path = home.path().join(".claude").join("settings.json");
            assert_eq!(c[0].status, CheckStatus::Warn, "{settings}");
            assert_eq!(
                c[0].summary,
                format!(
                    "Claude Code hooks: cannot read the hooks in {}: {reason}",
                    path.display()
                ),
                "{settings}"
            );
            assert_eq!(c[0].hint.as_deref(), Some(MIS_SHAPED_HINT), "{settings}");
            assert_eq!(c[1].status, CheckStatus::Info, "{settings}");
            assert!(c[1].summary.contains("skipped"), "{settings}");
        }
        // Absent or null hooks are readable: nothing is wired.
        for settings in ["{}", r#"{"hooks": null}"#] {
            let home = home_with_settings(settings);
            let c = claude_hook_checks(Some(home.path()), None);
            assert!(
                c[0].summary.contains("missing SessionStart"),
                "{settings}: {}",
                c[0].summary
            );
        }
    }

    #[test]
    fn disable_all_hooks_turns_every_hook_off() {
        let mut manual: Value = serde_json::from_str(&settings_with(&ALL_FOUR_BARE)).unwrap();
        manual["disableAllHooks"] = Value::Bool(true);
        let plugin = r#"{"enabledPlugins": {"ironmem@ironrace": true}, "disableAllHooks": true}"#;
        for settings in [manual.to_string(), plugin.to_owned()] {
            let home = home_with_settings(&settings);
            let c = claude_hook_checks(Some(home.path()), None);
            let path = home.path().join(".claude").join("settings.json");
            assert_eq!(c[0].status, CheckStatus::Warn, "{settings}");
            assert_eq!(
                c[0].summary,
                format!(
                    "Claude Code hooks: {} sets disableAllHooks to true, so Claude Code runs no \
                     ironmem hook unless a project's settings set it back to false: no memory \
                     context is loaded at session start, prompt recall is off and no hook mines \
                     workspace files",
                    path.display()
                ),
                "{settings}"
            );
            assert_eq!(c[0].hint.as_deref(), Some(DISABLE_ALL_HOOKS_HINT));
            assert_eq!(c[1].status, CheckStatus::Info, "{settings}");
            assert_eq!(
                c[1].summary,
                "hook mining: skipped (disableAllHooks is true in settings.json; see the hooks line)"
            );
            assert_eq!(c[1].hint, None);
        }
    }

    #[test]
    fn a_non_boolean_or_false_disable_all_hooks_is_ignored() {
        for value in [r#""true""#, "1", "false", "null"] {
            let mut doc: Value = serde_json::from_str(&settings_with(&ALL_FOUR_BARE)).unwrap();
            doc["disableAllHooks"] = serde_json::from_str(value).unwrap();
            let home = home_with_settings(&doc.to_string());
            let c = claude_hook_checks(Some(home.path()), None);
            assert_eq!(c[0].summary, all_four_wired(home.path()), "{value}");
        }
    }

    #[test]
    fn a_settings_file_that_cannot_be_read_wins_over_disable_all_hooks() {
        let home = home_with_settings(r#"{"disableAllHooks": true, "hooks": {"Stop": {}}}"#);
        let c = claude_hook_checks(Some(home.path()), None);
        assert!(
            c[0].summary.contains("cannot read the hooks in"),
            "{}",
            c[0].summary
        );
        let home = home_with_settings(r#"{"disableAllHooks": true,"#);
        let c = claude_hook_checks(Some(home.path()), None);
        assert!(c[0].summary.contains("not valid JSON"), "{}", c[0].summary);
    }

    /// `program` as a hook command's first word for `hook`, with `prefix`.
    fn wired_to(prefix: &str, program: &str, hook: &str) -> String {
        format!("{prefix}{program} hook {hook} --harness claude-code")
    }

    #[test]
    fn a_miner_wired_to_a_program_that_does_not_exist_is_not_mined() {
        let gone = tempfile::tempdir().unwrap().path().join("old/bin/ironmem");
        let gone = gone.display().to_string();
        let stop = wired_to("IRONMEM_MCP_MODE=trusted ", &gone, "stop");
        let home = home_with_settings(&all_four_with_command("Stop", &stop));
        let c = claude_hook_checks(Some(home.path()), None);
        // Stop's startup belongs to hook_mining, as for an invalid mode.
        assert_eq!(c[0].summary, all_four_wired(home.path()));
        assert_eq!(c[1].status, CheckStatus::Warn, "{}", c[1].summary);
        assert_eq!(
            c[1].summary,
            format!(
                "no wired hook mines — Stop: wired to {gone}, which does not exist -> does not \
                 run; PreCompact: read-only (IRONMEM_MCP_MODE unset, so the default) -> not mined"
            )
        );
        assert_eq!(
            c[1].hint.as_deref(),
            Some(
                format!(
                    "{MISSING_PROGRAM_HINT} | {}",
                    read_only_text("PreCompact command")
                )
                .as_str()
            )
        );
    }

    #[test]
    fn a_tilde_program_is_looked_up_under_home() {
        let stop = wired_to("IRONMEM_MCP_MODE=trusted ", "~/gone/ironmem", "stop");
        let c = mining_of(&settings_with(&[("Stop", stop.as_str())]), None);
        assert_eq!(
            c.summary,
            "no wired hook mines — Stop: wired to ~/gone/ironmem, which does not exist -> does \
             not run"
        );
        assert_eq!(c.hint.as_deref(), Some(MISSING_PROGRAM_HINT));
        // ~/.ironrace/bin/ironmem exists under the test home, so it mines.
        let stop = wired_to(
            "IRONMEM_MCP_MODE=trusted ",
            "~/.ironrace/bin/ironmem",
            "stop",
        );
        let c = mining_of(&settings_with(&[("Stop", stop.as_str())]), None);
        assert_eq!(c.status, CheckStatus::Ok, "{}", c.summary);
    }

    #[test]
    fn a_non_miner_wired_to_a_missing_program_fails_to_start_on_hooks() {
        let gone = tempfile::tempdir().unwrap().path().join("old/bin/ironmem");
        let gone = gone.display().to_string();
        let cmd = wired_to("", &gone, "session-start");
        let home = home_with_settings(&all_four_with_command("SessionStart", &cmd));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(c[0].status, CheckStatus::Warn, "{}", c[0].summary);
        assert_eq!(
            c[0].summary,
            format!(
                "Claude Code hooks: SessionStart is wired to {gone}, which does not exist, so no \
                 memory context is loaded at session start"
            )
        );
        assert_eq!(c[0].hint.as_deref(), Some(MISSING_PROGRAM_HINT));
    }

    #[test]
    fn a_program_that_is_not_an_executable_file_does_not_run() {
        let bins = tempfile::tempdir().unwrap();
        // A directory named ironmem.
        let dir = bins.path().join("dir/ironmem");
        std::fs::create_dir_all(&dir).unwrap();
        let mut cases = vec![dir];
        #[cfg(unix)]
        {
            // A file with no execute bit.
            let plain = bins.path().join("plain/ironmem");
            std::fs::create_dir_all(plain.parent().unwrap()).unwrap();
            std::fs::write(&plain, "").unwrap();
            cases.push(plain);
        }
        for program in cases {
            let program = program.display().to_string();
            let stop = wired_to("IRONMEM_MCP_MODE=trusted ", &program, "stop");
            let c = mining_of(&settings_with(&[("Stop", stop.as_str())]), None);
            assert_eq!(
                c.summary,
                format!(
                    "no wired hook mines — Stop: wired to {program}, which is not an executable \
                     file -> does not run"
                )
            );
        }
    }

    #[test]
    fn a_program_found_on_path_or_built_by_the_shell_is_not_checked() {
        for program in [
            "ironmem",
            "$HOME/gone/ironmem",
            "/gone*/ironmem",
            // Quoted, `~` is not expanded: a relative path, not checked.
            "'~/gone/ironmem'",
            "~\"/gone/ironmem\"",
        ] {
            let stop = wired_to("IRONMEM_MCP_MODE=trusted ", program, "stop");
            let c = mining_of(&settings_with(&[("Stop", stop.as_str())]), None);
            assert_eq!(c.status, CheckStatus::Ok, "{program}: {}", c.summary);
            assert!(!c.summary.contains("does not"), "{program}: {}", c.summary);
        }
    }

    #[test]
    fn a_hand_wired_copy_with_a_missing_program_leaves_only_the_plugin_copy() {
        let gone = tempfile::tempdir().unwrap().path().join("old/bin/ironmem");
        let gone = gone.display().to_string();
        let cmd = wired_to("", &gone, "user-prompt-submit");
        let home = home_with_settings(&plugin_and_ups(&cmd));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(
            c[0].summary,
            format!(
                "Claude Code hooks: the hand-wired UserPromptSubmit is wired to {gone}, which \
                 does not exist, so only the plugin's copy runs"
            )
        );
        assert_eq!(c[0].hint.as_deref(), Some(DUPLICATE_HINT));
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
        // Not told as a read-only run that starts but does not mine.
        assert!(!c[1].summary.contains("-> not mined"), "{}", c[1].summary);
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
        let [_, mining] = claude_hook_checks(Some(home.path()), process_env);
        mining
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
    fn a_quoted_mode_prefix_is_not_an_assignment_so_the_hook_is_missing() {
        // The shell runs a program named `IRONMEM_MCP_MODE=trusted` (exit
        // 127), so Stop is not wired, and nothing says it mines.
        let home = home_with_settings(&all_four_with("Stop", r#""IRONMEM_MCP_MODE=trusted""#));
        let c = claude_hook_checks(Some(home.path()), None);
        let path = home.path().join(".claude").join("settings.json");
        assert_eq!(
            c[0].summary,
            format!(
                "Claude Code hooks: missing Stop in {} (workspace files are not mined when a \
                 session stops)",
                path.display()
            )
        );
        assert_eq!(
            c[1].summary,
            "no wired hook mines — PreCompact: read-only (IRONMEM_MCP_MODE unset, so the \
             default) -> not mined"
        );
    }

    /// All four bare hooks, with `event`'s command replaced by `command`.
    fn all_four_with_command(event: &str, command: &str) -> String {
        let cmds: Vec<(&str, &str)> = ALL_FOUR_BARE
            .iter()
            .map(|(e, c)| (*e, if *e == event { command } else { *c }))
            .collect();
        settings_with(&cmds)
    }

    #[test]
    fn an_invalid_session_start_names_its_own_effect_not_recall() {
        let home = home_with_settings(&all_four_with("SessionStart", "IRONMEM_MCP_MODE=bogus"));
        let c = claude_hook_checks(Some(home.path()), None);
        let s = &c[0].summary;
        assert_eq!(c[0].status, CheckStatus::Warn, "{s}");
        assert!(s.contains("SessionStart fails to start"), "{s}");
        assert!(
            s.contains("no memory context is loaded at session start"),
            "{s}"
        );
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
        assert!(
            !s.contains("no memory context is loaded at session start"),
            "{s}"
        );
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
        // The whole line: the trusted PreCompact is not lumped in with Stop.
        assert_eq!(
            c.summary,
            "PreCompact is mined; Stop is not — Stop: read-only (IRONMEM_MCP_MODE unset, so \
             the default) -> not mined; PreCompact: trusted (set in the hook command) -> mined"
        );
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
        assert_eq!(
            hint,
            r#"doctor ran with IRONMEM_MCP_MODE="ReadOnly", and a Claude Code launched from this shell inherits it; launch Claude Code with IRONMEM_MCP_MODE=trusted to mine"#
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
            c.summary.starts_with(
                "no wired hook is known to mine; PreCompact may not be mined; Stop is not \
                 mined — "
            ),
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
    fn dynamic_hand_wired_miners_beside_failing_plugin_copies_name_their_effect() {
        let home = home_with_settings(&plugin_with(
            "bogus",
            &[
                ("Stop", "IRONMEM_MCP_MODE=$M ironmem hook stop"),
                ("PreCompact", "IRONMEM_MCP_MODE=$M ironmem hook precompact"),
            ],
        ));
        let c = claude_hook_checks(Some(home.path()), None);
        let may_cover = |event: &str, effect: &str| {
            format!(
                "the plugin's {event} fails to start: {BOGUS_REASON} (set by env in \
                 settings.json), so only the hand-wired copy may run: its mode cannot be \
                 determined statically (IRONMEM_MCP_MODE=$M, set in the hook command), and if \
                 it fails too, {effect}"
            )
        };
        assert_eq!(
            c[0].summary,
            format!(
                "Claude Code hooks: {}; {}; SessionStart fails to start: {BOGUS_REASON} (set by \
                 env in settings.json), so no memory context is loaded at session start; \
                 UserPromptSubmit fails to start: {BOGUS_REASON} (set by env in \
                 settings.json), so prompt recall is off",
                may_cover("Stop", "workspace files are not mined when a session stops"),
                may_cover(
                    "PreCompact",
                    "workspace files are not mined before compaction"
                ),
            )
        );
        assert_eq!(
            c[0].hint.as_deref(),
            Some(format!("{INVALID_MODE_HINT} | {DYNAMIC_MODE_HINT}").as_str())
        );
        // hook_mining reports, for each miner, the best of its runs (the
        // hand-wired `$M` copy).
        assert_eq!(c[1].status, CheckStatus::Warn, "{}", c[1].summary);
        assert_eq!(
            c[1].summary,
            "no wired hook is known to mine; Stop and PreCompact may not be mined — Stop: $M \
             (set in the hook command) -> mode cannot be determined statically; PreCompact: $M \
             (set in the hook command) -> mode cannot be determined statically"
        );
        assert_eq!(c[1].hint.as_deref(), Some(DYNAMIC_MODE_HINT));
    }

    #[test]
    fn the_mining_headline_names_every_group_in_every_mix() {
        // A miner that fails to start is in `not_mined`.
        /// (mined, not mined, unknown, expected headline).
        type Case = (
            &'static [&'static str],
            &'static [&'static str],
            &'static [&'static str],
            &'static str,
        );
        let cases: [Case; 9] = [
            (
                &["Stop", "PreCompact"],
                &[],
                &[],
                "Stop and PreCompact are mined",
            ),
            (&["Stop"], &[], &[], "Stop is mined"),
            (
                &["PreCompact"],
                &["Stop"],
                &[],
                "PreCompact is mined; Stop is not",
            ),
            (
                &["Stop"],
                &[],
                &["PreCompact"],
                "Stop is mined; PreCompact may not be mined",
            ),
            (&[], &["Stop", "PreCompact"], &[], "no wired hook mines"),
            (&[], &["Stop"], &[], "no wired hook mines"),
            (
                &[],
                &["PreCompact"],
                &["Stop"],
                "no wired hook is known to mine; Stop may not be mined; PreCompact is not mined",
            ),
            (
                &[],
                &[],
                &["Stop", "PreCompact"],
                "no wired hook is known to mine; Stop and PreCompact may not be mined",
            ),
            (
                &[],
                &[],
                &["Stop"],
                "no wired hook is known to mine; Stop may not be mined",
            ),
        ];
        for (mined, not_mined, unknown, want) in cases {
            assert_eq!(
                mining_headline(mined, not_mined, unknown),
                want,
                "{mined:?} {not_mined:?} {unknown:?}"
            );
        }
    }

    #[test]
    fn a_failing_miner_beside_a_dynamic_one_is_named_in_the_headline() {
        let c = mining_of(
            &miners(
                "IRONMEM_MCP_MODE=bogus ironmem hook stop",
                "IRONMEM_MCP_MODE=$M ironmem hook precompact",
            ),
            None,
        );
        assert_eq!(c.status, CheckStatus::Warn, "{}", c.summary);
        assert_eq!(
            c.summary,
            "no wired hook is known to mine; PreCompact may not be mined; Stop is not mined — \
             Stop: \"bogus\" (set in the hook command) -> fails to start: \
             IRONMEM_MCP_MODE=\"bogus\" is not trusted, read-only or restricted; PreCompact: $M \
             (set in the hook command) -> mode cannot be determined statically"
        );
        assert_eq!(
            c.hint.as_deref(),
            Some(format!("{INVALID_MODE_HINT} | {DYNAMIC_MODE_HINT}").as_str())
        );
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
                 command), and if it fails too, prompt recall is off; SessionStart fails to \
                 start: {BOGUS_REASON} (set by env in settings.json), so no memory context is \
                 loaded at session start"
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
        // A defensive branch: the CLI's `Config::load` rejects an invalid
        // IRONMEM_MCP_MODE in doctor's own environment before doctor runs
        // (pinned by tests/doctor.rs), so only a library caller reaches this.
        let home = home_with_settings(&settings_with(&ALL_FOUR_BARE));
        let c = claude_hook_checks(Some(home.path()), Some(""));
        for check in &c {
            assert!(check.summary.contains(r#""""#), "{}", check.summary);
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
                    .starts_with("no wired hook is known to mine; Stop may not be mined — "),
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
                 env in settings.json), so no memory context is loaded at session start",
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
        for hook in &PLUGIN_HOOKS {
            let (event, name) = (hook.event, hook.name);
            let plugin = [HookCommand::Shell(format!("{PLUGIN_HOOK_COMMAND} {name}"))];
            let home = tempfile::tempdir().unwrap();
            let settings = Context {
                env: ModeEnv {
                    settings: Some("$M"),
                    process: None,
                },
                home: home.path(),
            };
            assert_eq!(
                starts(hook, &plugin, settings),
                Starts::No(Failure::Invalid("$M".to_owned(), ModeSource::SettingsEnv)),
                "{event}"
            );
            // A defensive case: the CLI's `Config::load` rejects this value
            // in doctor's own environment before doctor runs.
            let shell = Context {
                env: ModeEnv {
                    settings: None,
                    process: Some("$(cat /m)"),
                },
                home: home.path(),
            };
            assert_eq!(
                starts(hook, &plugin, shell),
                Starts::No(Failure::Invalid(
                    "$(cat /m)".to_owned(),
                    ModeSource::Environment
                )),
                "{event}"
            );
        }
    }

    const UPS: &str = "~/.ironrace/bin/ironmem hook user-prompt-submit";

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
                 env in settings.json), so no memory context is loaded at session start",
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
    fn a_command_with_no_valid_run_fails_or_may_fail() {
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
                 (IRONMEM_MCP_MODE=$M, set in the hook command), so it may fail to start, and if \
                 it does, prompt recall is off",
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
             (IRONMEM_MCP_MODE=$M, set in the hook command), so it may fail to start, and if \
             it does, no memory context is loaded at session start"
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
                "Claude Code hooks: missing PreCompact in {} (workspace files are not mined \
                 before compaction); \
                 SessionStart fails to start: IRONMEM_MCP_MODE=\"bogus\" is not trusted, \
                 read-only or restricted (set in the hook command), so no memory context is \
                 loaded at session start; UserPromptSubmit's mode cannot be determined \
                 statically (IRONMEM_MCP_MODE=$M, set in the hook command), so it may fail to \
                 start, and if it does, prompt recall is off",
                path.display()
            )
        );
        assert_eq!(
            c[0].hint.as_deref(),
            Some(
                format!(
                    "{INSTALL_HINT} | {READ_SHAPE_HINT} | {INVALID_MODE_HINT} | \
                     {DYNAMIC_MODE_HINT}"
                )
                .as_str()
            )
        );

        // Cause order, not event order: the later UserPromptSubmit's failure
        // comes before the earlier SessionStart's uncertain start.
        let home = home_with_settings(&settings_with(&[
            (
                "SessionStart",
                "IRONMEM_MCP_MODE=$M ironmem hook session-start",
            ),
            (
                "UserPromptSubmit",
                "IRONMEM_MCP_MODE=bogus ironmem hook user-prompt-submit",
            ),
            ("Stop", "ironmem hook stop"),
            ("PreCompact", "ironmem hook precompact"),
        ]));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(
            c[0].summary,
            "Claude Code hooks: UserPromptSubmit fails to start: IRONMEM_MCP_MODE=\"bogus\" is \
             not trusted, read-only or restricted (set in the hook command), so prompt recall \
             is off; SessionStart's mode cannot be determined statically (IRONMEM_MCP_MODE=$M, \
             set in the hook command), so it may fail to start, and if it does, no memory \
             context is loaded at session start"
        );
        assert_eq!(
            c[0].hint.as_deref(),
            Some(format!("{INVALID_MODE_HINT} | {DYNAMIC_MODE_HINT}").as_str())
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
        assert_eq!(c[1].status, CheckStatus::Warn, "{}", c[1].summary);
        assert_eq!(
            c[1].summary,
            "no wired hook is known to mine; Stop may not be mined; PreCompact is not mined — \
             Stop: $M (set in the hook command) -> mode cannot be determined statically; \
             PreCompact: read-only (IRONMEM_MCP_MODE unset, so the default) -> not mined"
        );
        assert_eq!(
            c[1].hint.as_deref(),
            Some(
                format!(
                    "{DYNAMIC_MODE_HINT} | {}",
                    read_only_text("PreCompact command")
                )
                .as_str()
            )
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
                "no wired hook is known to mine; Stop may not be mined — Stop: $M (set in the \
                 hook command) -> mode cannot be determined statically",
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
                    "IRONMEM_MCP_MODE=trusted ~/.ironrace/bin/ironmem hook session-start",
                ),
                (
                    "UserPromptSubmit",
                    "IRONMEM_MCP_MODE=trusted ~/.ironrace/bin/ironmem hook user-prompt-submit",
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
                fails(
                    "SessionStart",
                    "no memory context is loaded at session start"
                ),
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
                 {BOGUS_REASON} (set by env in settings.json), so no memory context is loaded \
                 at session start; UserPromptSubmit fails to start: {BOGUS_REASON} (set by env \
                 in settings.json), so prompt recall is off"
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

    /// A hook entry in exec form (`args` set).
    fn exec_entry(command: &str, args: &[&str]) -> Value {
        serde_json::json!({"type": "command", "command": command, "args": args})
    }

    /// settings.json with one hook entry per event.
    fn settings_with_entries(entries: &[(&str, Value)]) -> String {
        let mut hooks = serde_json::Map::new();
        for (event, entry) in entries {
            hooks.insert(
                (*event).to_string(),
                serde_json::json!([{ "hooks": [entry] }]),
            );
        }
        serde_json::json!({ "hooks": hooks }).to_string()
    }

    /// A home as [`home_with_settings`] makes, with `settings(home)` as its
    /// settings.json, for entries that name a program under the home.
    fn home_with_settings_for(settings: impl FnOnce(&Path) -> String) -> tempfile::TempDir {
        let home = home_with_settings("{}");
        let text = settings(home.path());
        std::fs::write(home.path().join(".claude").join("settings.json"), text).unwrap();
        home
    }

    /// The ironmem program `home_with_settings` creates, as an absolute path.
    fn home_program(home: &Path) -> String {
        home.join(".ironrace/bin/ironmem").display().to_string()
    }

    /// The shared Stop fixtures in scripts/fixtures/hook_stop_fixtures.json:
    /// each row as a hook entry, whether it runs `ironmem hook stop`, and its
    /// note. scripts/test_install_ironmem.py reads the same file for the
    /// installer's `_claude_hook_is_wired`, so both are pinned to one answer.
    fn stop_fixtures() -> Vec<(Value, bool, String)> {
        let doc: Value = serde_json::from_str(include_str!(
            "../../../../scripts/fixtures/hook_stop_fixtures.json"
        ))
        .unwrap();
        doc["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let mut entry = serde_json::json!({"type": "command", "command": row["command"]});
                if let Some(args) = row.get("args") {
                    entry["args"] = args.clone();
                }
                let wired = row["wired"].as_bool().unwrap();
                (entry, wired, row["note"].as_str().unwrap().to_owned())
            })
            .collect()
    }

    #[test]
    fn the_shared_stop_fixtures_are_read_as_the_installer_reads_them() {
        let rows = stop_fixtures();
        assert!(rows.len() > 30, "{} rows", rows.len());
        for (entry, wired, note) in rows {
            let runs = hook_command(&entry).is_some_and(|c| c.runs_hook("stop"));
            assert_eq!(runs, wired, "{entry} ({note})");
        }
    }

    /// The installer's exec-form miners hint, pinned word for word.
    fn exec_read_only_text(entries: &str) -> String {
        format!(
            "expected outside a plugin install: MCP writes and prompt recall still work. \
             A cheaper way to mine without the plugin is tracked in #418. To mine now, in \
             the {entries} (exec form, args set) make env the command and put \
             IRONMEM_MCP_MODE=trusted, then the program, first in args; each run then loads \
             the embedding model and rebuilds the vector index"
        )
    }

    #[test]
    fn an_exec_form_wiring_of_all_four_is_wired_and_read_only() {
        let home = home_with_settings_for(|home| {
            let program = home_program(home);
            let entries: Vec<(&str, Value)> = PLUGIN_HOOKS
                .iter()
                .map(|h| {
                    let args = ["hook", h.name, "--harness", "claude-code"];
                    (h.event, exec_entry(&program, &args))
                })
                .collect();
            settings_with_entries(&entries)
        });
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(c[0].status, CheckStatus::Ok, "{}", c[0].summary);
        assert_eq!(c[0].summary, all_four_wired(home.path()));
        assert_eq!(c[1].status, CheckStatus::Warn);
        assert_eq!(
            c[1].summary,
            "no wired hook mines — Stop: read-only (IRONMEM_MCP_MODE unset, so the default) -> \
             not mined; PreCompact: read-only (IRONMEM_MCP_MODE unset, so the default) -> not \
             mined"
        );
        assert_eq!(
            c[1].hint.as_deref(),
            Some(exec_read_only_text("Stop and PreCompact entries").as_str())
        );
    }

    #[test]
    fn an_exec_form_env_assignment_sets_the_mode_and_mines() {
        // What the exec-form read-only hint tells the user to write.
        let home = home_with_settings_for(|home| {
            let program = home_program(home);
            let trusted = |name| {
                let args = ["IRONMEM_MCP_MODE=trusted", program.as_str(), "hook", name];
                exec_entry("env", &args)
            };
            settings_with_entries(&[
                ("Stop", trusted("stop")),
                ("PreCompact", trusted("precompact")),
            ])
        });
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(c[1].status, CheckStatus::Ok, "{}", c[1].summary);
        assert_eq!(
            c[1].summary,
            "Stop and PreCompact are mined — Stop: trusted (set in the hook command) -> mined; \
             PreCompact: trusted (set in the hook command) -> mined"
        );
        assert_eq!(c[1].hint, None);
    }

    #[test]
    fn an_exec_form_program_that_does_not_exist_does_not_run() {
        let gone = tempfile::tempdir().unwrap().path().join("old/bin/ironmem");
        let gone = gone.display().to_string();
        let entry = exec_entry(&gone, &["hook", "session-start"]);
        let home = home_with_settings(&settings_with_entries(&[("SessionStart", entry)]));
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(c[0].status, CheckStatus::Warn, "{}", c[0].summary);
        assert!(
            c[0].summary.contains(&format!(
                "SessionStart is wired to {gone}, which does not exist, so no memory context is \
                 loaded at session start"
            )),
            "{}",
            c[0].summary
        );
        assert!(c[0].hint.as_deref().unwrap().contains(MISSING_PROGRAM_HINT));
    }

    #[test]
    fn an_exec_form_tilde_program_is_not_expanded() {
        // ~/.ironrace/bin/ironmem exists under the test home, but no shell
        // expands the ~ of an exec-form command.
        let args = [
            "IRONMEM_MCP_MODE=trusted",
            "~/.ironrace/bin/ironmem",
            "hook",
            "stop",
        ];
        let settings = settings_with_entries(&[
            ("Stop", exec_entry("env", &args)),
            (
                "SessionStart",
                exec_entry("~/.ironrace/bin/ironmem", &["hook", "session-start"]),
            ),
        ]);
        let home = home_with_settings(&settings);
        let c = claude_hook_checks(Some(home.path()), None);
        let why = "which starts with a ~ that is not expanded, because with args set (exec form) \
                   no shell reads the command";
        assert!(
            c[0].summary.contains(&format!(
                "SessionStart is wired to ~/.ironrace/bin/ironmem, {why}, so no memory context is \
                 loaded at session start"
            )),
            "{}",
            c[0].summary
        );
        assert_eq!(
            c[1].summary,
            format!("no wired hook mines — Stop: wired to ~/.ironrace/bin/ironmem, {why} -> does not run")
        );
        assert_eq!(c[1].hint.as_deref(), Some(MISSING_PROGRAM_HINT));
    }

    #[test]
    fn an_exec_form_bash_c_string_is_read_as_shell_form() {
        let script = "IRONMEM_MCP_MODE=trusted ~/.ironrace/bin/ironmem hook stop";
        let settings = settings_with_entries(&[("Stop", exec_entry("bash", &["-c", script]))]);
        let c = mining_of(&settings, None);
        assert_eq!(c.status, CheckStatus::Ok, "{}", c.summary);
        assert_eq!(
            c.summary,
            "Stop is mined — Stop: trusted (set in the hook command) -> mined"
        );
    }

    /// Doctor's read-only hint for bare miners whose fix is `fix`, pinned
    /// word for word.
    fn read_only_text_fixed_by(fix: &str) -> String {
        format!(
            "expected outside a plugin install: MCP writes and prompt recall still work. \
             A cheaper way to mine without the plugin is tracked in #418. To mine now, {fix}; \
             each run then loads the embedding model and rebuilds the vector index"
        )
    }

    const READ_ONLY_DEFAULT: &str =
        "read-only (IRONMEM_MCP_MODE unset, so the default) -> not mined";

    #[test]
    fn a_bare_run_in_an_exec_form_c_string_is_told_to_edit_the_string() {
        // Prefixing the entry's command (`bash`) would rename the program, so
        // the fix goes inside the -c string.
        let one =
            settings_with_entries(&[("Stop", exec_entry("bash", &["-c", "ironmem hook stop"]))]);
        let c = mining_of(&one, None);
        assert_eq!(
            c.summary,
            format!("no wired hook mines — Stop: {READ_ONLY_DEFAULT}")
        );
        assert_eq!(
            c.hint.as_deref(),
            Some(
                read_only_text_fixed_by(
                    "in the Stop entry (exec form, args set) put IRONMEM_MCP_MODE=trusted before \
                     the ironmem command inside its -c string"
                )
                .as_str()
            )
        );
        let both = settings_with_entries(&[
            ("Stop", exec_entry("bash", &["-c", "ironmem hook stop"])),
            (
                "PreCompact",
                exec_entry("sh", &["-c", "ironmem hook precompact"]),
            ),
        ]);
        assert_eq!(
            mining_of(&both, None).hint.as_deref(),
            Some(
                read_only_text_fixed_by(
                    "in the Stop and PreCompact entries (exec form, args set) put \
                     IRONMEM_MCP_MODE=trusted before the ironmem command inside each -c string"
                )
                .as_str()
            )
        );
    }

    #[test]
    fn one_exec_form_bare_miner_gets_the_singular_exec_hint() {
        let home = home_with_settings_for(|home| {
            let program = home_program(home);
            settings_with_entries(&[("Stop", exec_entry(&program, &["hook", "stop"]))])
        });
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(
            c[1].summary,
            format!("no wired hook mines — Stop: {READ_ONLY_DEFAULT}")
        );
        assert_eq!(
            c[1].hint.as_deref(),
            Some(exec_read_only_text("Stop entry").as_str())
        );
    }

    #[test]
    fn exec_form_and_shell_form_bare_miners_get_one_hint_each() {
        let home = home_with_settings_for(|home| {
            let program = home_program(home);
            let shell =
                serde_json::json!({"type": "command", "command": "ironmem hook precompact"});
            settings_with_entries(&[
                ("Stop", exec_entry(&program, &["hook", "stop"])),
                ("PreCompact", shell),
            ])
        });
        let c = claude_hook_checks(Some(home.path()), None);
        assert_eq!(
            c[1].summary,
            format!(
                "no wired hook mines — Stop: {READ_ONLY_DEFAULT}; PreCompact: {READ_ONLY_DEFAULT}"
            )
        );
        assert_eq!(
            c[1].hint,
            Some(format!(
                "{} | {}",
                exec_read_only_text("Stop entry"),
                read_only_text("PreCompact command")
            ))
        );
    }

    #[test]
    fn an_exec_form_entry_with_a_non_string_arg_is_skipped() {
        let junk = serde_json::json!({"type": "command", "command": "ironmem hook stop", "args": ["x", 5]});
        let mut doc: Value = serde_json::from_str(&settings_with(&ALL_FOUR_BARE)).unwrap();
        doc["hooks"]["Stop"] = serde_json::json!([{ "hooks": [junk] }]);
        let home = home_with_settings(&doc.to_string());
        let c = claude_hook_checks(Some(home.path()), None);
        let path = home.path().join(".claude").join("settings.json");
        assert!(
            c[0].summary.starts_with(&format!(
                "Claude Code hooks: missing Stop in {} (",
                path.display()
            )),
            "{}",
            c[0].summary
        );
    }

    #[test]
    fn an_exec_form_dollar_is_literal_but_a_placeholder_is_unknown() {
        let home = home_with_settings_for(|home| {
            let program = home_program(home);
            let stop = ["IRONMEM_MCP_MODE=$M", program.as_str(), "hook", "stop"];
            let pre = [
                "IRONMEM_MCP_MODE=${CLAUDE_PLUGIN_ROOT}",
                program.as_str(),
                "hook",
                "precompact",
            ];
            settings_with_entries(&[
                ("Stop", exec_entry("env", &stop)),
                ("PreCompact", exec_entry("env", &pre)),
            ])
        });
        let c = claude_hook_checks(Some(home.path()), None);
        assert!(
            c[1].summary.contains(
                r#"Stop: "$M" (set in the hook command) -> fails to start: IRONMEM_MCP_MODE="$M" is not trusted, read-only or restricted"#
            ),
            "{}",
            c[1].summary
        );
        assert!(
            c[1].summary.contains(
                "PreCompact: ${CLAUDE_PLUGIN_ROOT} (set in the hook command) -> mode cannot be \
                 determined statically"
            ),
            "{}",
            c[1].summary
        );
    }
}

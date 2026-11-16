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

const MODE_VAR: &str = "IRONMEM_MCP_MODE";

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

/// Why a hook's mode could not be turned into a [`McpAccessMode`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum ModeError {
    /// `Config::load` rejects the value, so the hook exits at startup.
    Invalid(String),
    /// The value is built by the shell (`$VAR`, `$(..)`), so doctor can't know it.
    Unknown(String),
}

type ModeResult = (Result<McpAccessMode, ModeError>, ModeSource);

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

/// One word of a shell command after quote removal.
#[derive(Debug, Clone)]
struct Word {
    text: String,
    /// Contains an unquoted or double-quoted `$` or backtick: the shell
    /// decides its value, not us.
    dynamic: bool,
}

/// Accumulates the words and simple commands of [`split_commands`].
#[derive(Default)]
struct Splitter {
    commands: Vec<Vec<Word>>,
    words: Vec<Word>,
    cur: String,
    in_word: bool,
    dynamic: bool,
    quoted: bool,
    /// The next word is a redirection target and is dropped.
    skip_next: bool,
}

impl Splitter {
    fn end_word(&mut self) {
        if self.in_word {
            let word = Word {
                text: std::mem::take(&mut self.cur),
                dynamic: self.dynamic,
            };
            if self.skip_next {
                self.skip_next = false;
            } else {
                self.words.push(word);
            }
        }
        self.in_word = false;
        self.dynamic = false;
        self.quoted = false;
    }

    fn end_command(&mut self) {
        self.end_word();
        self.skip_next = false;
        if !self.words.is_empty() {
            self.commands.push(std::mem::take(&mut self.words));
        }
    }
}

/// Split a shell command string into simple commands of words.
///
/// A deliberately small model, not a shell: single quotes are literal, double
/// quotes group (a backslash escapes the next character), unquoted whitespace
/// separates words, and unquoted `; & | ( )` and newlines end a simple
/// command (so `&&`, `||` and glued forms like `stop&&true` split too). An
/// unquoted word starting with `#` begins a comment. Redirections (`>`, `>>`,
/// `<`, `2>`, `2>&1`, `stop>>/log`) are dropped together with their target.
/// Not modelled: heredocs, here-strings, `$(..)` bodies, brace groups, `if`
/// and `case`; those shapes are documented limits, not bugs.
fn split_commands(input: &str) -> Vec<Vec<Word>> {
    let mut sp = Splitter::default();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                sp.in_word = true;
                sp.quoted = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    sp.cur.push(q);
                }
            }
            '"' => {
                sp.in_word = true;
                sp.quoted = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => sp.cur.extend(chars.next()),
                        '$' | '`' => {
                            sp.dynamic = true;
                            sp.cur.push(q);
                        }
                        _ => sp.cur.push(q),
                    }
                }
            }
            '\\' => match chars.next() {
                Some('\n') | None => {}
                Some(n) => {
                    sp.in_word = true;
                    sp.cur.push(n);
                }
            },
            '$' | '`' => {
                sp.in_word = true;
                sp.dynamic = true;
                sp.cur.push(c);
            }
            '#' if !sp.in_word => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
                sp.end_command();
            }
            ';' | '\n' | '&' | '|' | '(' | ')' => sp.end_command(),
            '>' | '<' => {
                // `2>` and `2>&1`: a bare file-descriptor number is part of the operator.
                if sp.in_word && !sp.quoted && sp.cur.chars().all(|d| d.is_ascii_digit()) {
                    sp.cur.clear();
                    sp.in_word = false;
                    sp.dynamic = false;
                } else {
                    sp.end_word();
                }
                if chars.peek() == Some(&c) || (c == '>' && chars.peek() == Some(&'&')) {
                    chars.next();
                }
                sp.skip_next = true;
            }
            c if c.is_whitespace() => sp.end_word(),
            _ => {
                sp.in_word = true;
                sp.cur.push(c);
            }
        }
    }
    sp.end_command();
    sp.commands
}

fn basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// A `NAME=value` word, if it is one.
fn assignment(word: &Word) -> Option<(&str, &str)> {
    let (name, value) = word.text.split_once('=')?;
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    (first_ok && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')).then_some((name, value))
}

/// A value given to `IRONMEM_MCP_MODE` by a command or an `export`.
#[derive(Debug, Clone)]
struct ModeValue {
    raw: String,
    dynamic: bool,
}

fn mode_assignment(word: &Word) -> Option<ModeValue> {
    match assignment(word) {
        Some((MODE_VAR, value)) => Some(ModeValue {
            raw: value.to_owned(),
            dynamic: word.dynamic,
        }),
        _ => None,
    }
}

/// One run of `ironmem hook <hook>` found in a command string.
#[derive(Debug, Clone)]
struct Invocation {
    hook: String,
    /// Goes through `ironmem-mcp.sh` or `ironmem-hook.sh`, which default the
    /// mode to trusted (`${IRONMEM_MCP_MODE:-trusted}`).
    wrapper: bool,
    /// The mode set for this run by an assignment word or an earlier `export`.
    mode: Option<ModeValue>,
}

/// The hook name after `hook`, skipping `--harness <v>` / `--harness=<v>`.
fn hook_after_subcommand(args: &[Word]) -> Option<&str> {
    if args.first()?.text != "hook" {
        return None;
    }
    let at = match args.get(1).map(|w| w.text.as_str()) {
        Some("--harness") => 3,
        Some(t) if t.starts_with("--harness=") => 2,
        _ => 1,
    };
    args.get(at).map(|w| w.text.as_str())
}

/// Consume leading assignment words (and, for `env`, option flags) from `rest`.
fn take_assignments(rest: &mut &[Word], inline: &mut Option<ModeValue>, allow_flags: bool) {
    while let Some(word) = rest.first() {
        if let Some(value) = mode_assignment(word) {
            *inline = Some(value);
        } else if !(assignment(word).is_some() || allow_flags && word.text.starts_with('-')) {
            return;
        }
        *rest = &rest[1..];
    }
}

/// The `ironmem hook` runs in `command`; see [`runs_hook`] for the model.
fn invocations(command: &str, inherited: Option<ModeValue>) -> Vec<Invocation> {
    let mut exported = inherited;
    let mut found = Vec::new();
    for words in split_commands(command) {
        let mut inline = None;
        let mut rest: &[Word] = &words;
        take_assignments(&mut rest, &mut inline, false);
        while let Some(first) = rest.first() {
            let name = basename(&first.text);
            if !matches!(name, "env" | "exec" | "command" | "nohup") {
                break;
            }
            rest = &rest[1..];
            if name == "env" {
                take_assignments(&mut rest, &mut inline, true);
            }
        }
        let Some((program, args)) = rest.split_first() else {
            continue;
        };
        let program = basename(&program.text);
        if program == "export" {
            if let Some(value) = args.iter().find_map(mode_assignment) {
                exported = Some(value);
            }
            continue;
        }
        let mode = inline.or_else(|| exported.clone());
        let mut push = |hook: &str, wrapper: bool| {
            found.push(Invocation {
                hook: hook.to_owned(),
                wrapper,
                mode: mode.clone(),
            });
        };
        match program {
            "ironmem" | "ironmem-mcp.sh" => {
                if let Some(hook) = hook_after_subcommand(args) {
                    push(hook, program == "ironmem-mcp.sh");
                }
            }
            "ironmem-hook.sh" => {
                if let Some(hook) = args.first() {
                    push(&hook.text, true);
                }
            }
            "bash" | "sh" => shell_args(args, mode.clone(), &mut found),
            _ => {}
        }
    }
    found
}

/// Handle the arguments of `bash`/`sh`: `-c <string>` recurses into the
/// string, otherwise the first non-flag argument is the script.
fn shell_args(args: &[Word], mode: Option<ModeValue>, found: &mut Vec<Invocation>) {
    for (i, arg) in args.iter().enumerate() {
        let text = arg.text.as_str();
        if text.starts_with('-') && !text.starts_with("--") && text.ends_with('c') {
            if let Some(script) = args.get(i + 1) {
                found.extend(invocations(&script.text, mode));
            }
            return;
        }
        if text.starts_with('-') {
            continue;
        }
        if basename(text) == "ironmem-hook.sh" {
            if let Some(hook) = args.get(i + 1) {
                found.push(Invocation {
                    hook: hook.text.clone(),
                    wrapper: true,
                    mode,
                });
            }
        }
        return;
    }
}

/// True when `command` runs `ironmem hook <hook>`.
///
/// The model is a small simple-command reader, not a shell (see
/// [`split_commands`] for the word and operator rules). In each simple
/// command, leading `NAME=value` words are assignments; the wrappers `env`
/// (with its own assignments and flags), `exec`, `command` and `nohup` are
/// peeled off; the program's basename must then be `ironmem` or
/// `ironmem-mcp.sh` followed by `hook`, optionally `--harness <v>` or
/// `--harness=<v>`, then the name; or `ironmem-hook.sh <name>` run directly or
/// by `bash`/`sh`; or `bash`/`sh -c <string>`, which is read recursively.
/// Anything else (`echo`, `printf`, `true`) is not the hook.
///
/// Known limits, documented rather than bugs: control flow is not followed, so
/// `true || ironmem hook stop` counts as wired; heredocs, brace groups and
/// `$(..)` bodies are not read; aliases and functions are unknown.
fn runs_hook(command: &str, hook: &str) -> bool {
    invocations(command, None).iter().any(|i| i.hook == hook)
}

/// The mode the `hook` run(s) in `command` get, and where that came from.
///
/// Only the same simple command counts: the mode comes from the assignment
/// words (or `env` assignments) of the command that runs the hook, or from an
/// earlier `export IRONMEM_MCP_MODE=v` in the same string, and the command's
/// own assignment wins. It never comes from a later or unrelated command, so
/// `ironmem hook stop; IRONMEM_MCP_MODE=trusted true` runs read-only. When
/// the command holds several runs of `hook`, a trusted one is returned.
///
/// With no inline value the order is settings.json `env`, then the inherited
/// environment, then the plugin wrapper's trusted default (an empty value
/// counts as unset to the wrapper, but `Config::load` rejects it), then
/// read-only. A value containing `$` or a backtick is [`ModeError::Unknown`].
fn hook_mode(
    command: &str,
    hook: &str,
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> ModeResult {
    let results: Vec<ModeResult> = invocations(command, None)
        .iter()
        .filter(|i| i.hook == hook)
        .map(|i| invocation_mode(i, settings_env, process_env))
        .collect();
    first_trusted(&results)
        .or_else(|| results.first())
        .cloned()
        .unwrap_or((Ok(McpAccessMode::ReadOnly), ModeSource::Default))
}

fn is_trusted(result: &ModeResult) -> bool {
    matches!(result.0, Ok(McpAccessMode::Trusted))
}

/// A trusted result, preferring one that doesn't depend on doctor's shell.
fn first_trusted(results: &[ModeResult]) -> Option<&ModeResult> {
    results
        .iter()
        .find(|r| is_trusted(r) && r.1 != ModeSource::Environment)
        .or_else(|| results.iter().find(|r| is_trusted(r)))
}

fn invocation_mode(
    run: &Invocation,
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> ModeResult {
    let (raw, dynamic, source) = match (&run.mode, settings_env, process_env) {
        (Some(v), _, _) => (v.raw.as_str(), v.dynamic, ModeSource::Inline),
        (None, Some(v), _) => (v, false, ModeSource::SettingsEnv),
        (None, None, Some(v)) => (v, false, ModeSource::Environment),
        (None, None, None) if run.wrapper => {
            return (Ok(McpAccessMode::Trusted), ModeSource::PluginWrapper)
        }
        (None, None, None) => return (Ok(McpAccessMode::ReadOnly), ModeSource::Default),
    };
    if dynamic {
        return (Err(ModeError::Unknown(raw.to_owned())), source);
    }
    if run.wrapper && raw.is_empty() {
        return (Ok(McpAccessMode::Trusted), ModeSource::PluginWrapper);
    }
    match McpAccessMode::parse(raw) {
        // The wrapper defaults to trusted, so this holds without doctor's shell.
        Ok(McpAccessMode::Trusted) if run.wrapper && source == ModeSource::Environment => {
            (Ok(McpAccessMode::Trusted), ModeSource::PluginWrapper)
        }
        Ok(mode) => (Ok(mode), source),
        Err(e) => (Err(ModeError::Invalid(e.to_string())), source),
    }
}

/// Per-command results for `event`'s matching commands, best first: a trusted
/// run if any, else the first command in settings order.
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
    first_trusted(&results)
        .or_else(|| {
            results
                .iter()
                .find(|r| !matches!(r.0, Err(ModeError::Invalid(_))))
        })
        .or_else(|| results.first())
        .cloned()
        .unwrap_or((Ok(McpAccessMode::ReadOnly), ModeSource::Default))
}

/// Non-mining hooks (SessionStart, UserPromptSubmit) whose mode is invalid:
/// `Config::load` fails, so they exit at startup and prompt recall is off.
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
                (Err(ModeError::Invalid(e)), source) => Some(format!(
                    "{event} fails to start: {e} ({})",
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
        problems.push(format!(
            "missing {} in {} ({})",
            missing.join(", "),
            path.display(),
            effects.join("; ")
        ));
        hints.push(INSTALL_HINT);
    }
    if !failures.is_empty() {
        problems.push(format!("{}; prompt recall is off", failures.join("; ")));
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
            (Err(ModeError::Invalid(e)), source) => failing.push(format!(
                "{event} fails to start: {e} ({})",
                source_label(source)
            )),
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
                "hooks cannot mine: {}. A hook with an invalid mode exits at startup, so it \
                 neither recalls nor mines",
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
    let summary = if from_shell {
        "Stop/PreCompact hooks run trusted (inherited from IRONMEM_MCP_MODE in this shell; \
         Claude Code must be launched with it): transcripts are mined"
    } else {
        "Stop/PreCompact hooks run trusted: transcripts are mined"
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
            hook_mode(bare, "stop", None, None),
            (Ok(ReadOnly), ModeSource::Default)
        );
        assert_eq!(
            hook_mode(bare, "stop", None, Some("trusted")),
            (Ok(Trusted), ModeSource::Environment)
        );
        assert_eq!(
            hook_mode(bare, "stop", Some("trusted"), Some("read-only")),
            (Ok(Trusted), ModeSource::SettingsEnv)
        );
        assert_eq!(
            hook_mode(
                "IRONMEM_MCP_MODE='read-only' ironmem hook stop",
                "stop",
                Some("trusted"),
                Some("trusted")
            ),
            (Ok(ReadOnly), ModeSource::Inline)
        );
        assert_eq!(
            hook_mode("bash /p/hooks/ironmem-hook.sh stop", "stop", None, None),
            (Ok(Trusted), ModeSource::PluginWrapper)
        );
        assert_eq!(
            hook_mode(
                "bash /p/hooks/ironmem-hook.sh stop",
                "stop",
                None,
                Some("read-only")
            ),
            (Ok(ReadOnly), ModeSource::Environment),
            "the wrapper only defaults; an inherited mode wins"
        );
        assert!(hook_mode(
            "IRONMEM_MCP_MODE=bogus ironmem hook stop",
            "stop",
            None,
            None
        )
        .0
        .is_err());
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
        assert!(
            c[1].summary.contains("neither recalls nor mines"),
            "{}",
            c[1].summary
        );
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
    fn runs_hook_matches_quoted_wrapped_and_flagged_forms() {
        assert!(runs_hook(
            r#""$HOME/.ironrace/bin/ironmem" hook stop"#,
            "stop"
        ));
        assert!(runs_hook(
            r#"bash "${CLAUDE_PLUGIN_ROOT}/hooks/ironmem-hook.sh" stop"#,
            "stop"
        ));
        assert!(runs_hook("sh -c 'ironmem hook stop'", "stop"));
        assert!(runs_hook("ironmem hook --harness claude-code stop", "stop"));
        assert!(runs_hook("ironmem hook --harness=claude-code stop", "stop"));
        assert!(runs_hook("cd /x; ironmem hook stop; true", "stop"));
        assert!(!runs_hook(
            "ironmem hook --harness claude-code stopx",
            "stop"
        ));
        assert!(!runs_hook("'notironmem' hook stop", "stop"));
        assert!(!runs_hook("ironmem hook --harness", "stop"));
    }

    #[test]
    fn inline_mode_ignores_shell_separators_and_quotes() {
        use McpAccessMode::*;
        assert_eq!(
            hook_mode(
                "export IRONMEM_MCP_MODE=trusted; ironmem hook stop",
                "stop",
                None,
                None
            ),
            (Ok(Trusted), ModeSource::Inline)
        );
        assert_eq!(
            hook_mode(
                "export IRONMEM_MCP_MODE=\"read-only\"; ironmem hook stop",
                "stop",
                None,
                None
            ),
            (Ok(ReadOnly), ModeSource::Inline)
        );
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
    fn mode_is_scoped_to_the_simple_command_that_runs_the_hook() {
        use McpAccessMode::*;
        let later = "ironmem hook stop; IRONMEM_MCP_MODE=trusted true";
        assert_eq!(
            hook_mode(later, "stop", None, None),
            (Ok(ReadOnly), ModeSource::Default)
        );
        let two = "IRONMEM_MCP_MODE=read-only ironmem hook precompact; \
                   IRONMEM_MCP_MODE=trusted ironmem hook stop";
        assert_eq!(
            hook_mode(two, "stop", None, None),
            (Ok(Trusted), ModeSource::Inline)
        );
        assert_eq!(
            hook_mode(two, "precompact", None, None),
            (Ok(ReadOnly), ModeSource::Inline)
        );
        let exported = "export IRONMEM_MCP_MODE=trusted; ironmem hook stop";
        assert_eq!(
            hook_mode(exported, "stop", None, None),
            (Ok(Trusted), ModeSource::Inline)
        );
        let inline_wins =
            "export IRONMEM_MCP_MODE=trusted; IRONMEM_MCP_MODE=read-only ironmem hook stop";
        assert_eq!(
            hook_mode(inline_wins, "stop", None, None),
            (Ok(ReadOnly), ModeSource::Inline)
        );
        let late_export = "ironmem hook stop; export IRONMEM_MCP_MODE=trusted";
        assert_eq!(
            hook_mode(late_export, "stop", None, None),
            (Ok(ReadOnly), ModeSource::Default)
        );
        let c = mining_of(&settings_with(&[("Stop", later)]), None);
        assert_eq!(c.status, CheckStatus::Warn, "{}", c.summary);
        assert!(c.summary.contains("Stop runs read-only"), "{}", c.summary);
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
    fn simple_command_model_resolves_odd_shapes() {
        assert!(!runs_hook("echo ironmem hook stop", "stop"));
        assert!(!runs_hook("# ironmem hook stop", "stop"));
        assert!(!runs_hook("true # ironmem hook stop", "stop"));
        assert!(!runs_hook("printf '%s' 'ironmem hook stop'", "stop"));
        assert!(runs_hook("ironmem hook stop>>/tmp/log", "stop"));
        assert!(runs_hook("ironmem hook stop >/dev/null 2>&1", "stop"));
        assert!(runs_hook("ironmem hook stop&&true", "stop"));
        assert!(
            runs_hook("true || ironmem hook stop", "stop"),
            "documented limit"
        );
        assert!(runs_hook("sh -c 'ironmem hook stop'", "stop"));
        assert!(runs_hook("nohup ironmem hook stop", "stop"));
        assert!(runs_hook("exec ironmem hook stop", "stop"));
        assert!(runs_hook("command ironmem hook stop", "stop"));
        assert!(runs_hook(
            "bash -x \"/p/hooks/ironmem-hook.sh\" stop",
            "stop"
        ));
        assert!(!runs_hook("bash /p/other.sh ironmem-hook.sh stop", "stop"));
        use McpAccessMode::*;
        let env = "env IRONMEM_MCP_MODE=trusted ironmem hook stop";
        assert_eq!(
            hook_mode(env, "stop", None, None),
            (Ok(Trusted), ModeSource::Inline)
        );
        let sh = "IRONMEM_MCP_MODE=trusted sh -c 'ironmem hook stop'";
        assert_eq!(
            hook_mode(sh, "stop", None, None),
            (Ok(Trusted), ModeSource::Inline)
        );
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
}

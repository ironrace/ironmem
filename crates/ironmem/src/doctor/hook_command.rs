//! A small model of the shell commands in Claude Code hook entries (#377).
//!
//! Answers two questions about a hook command string: does it run
//! `ironmem hook <name>`, and in which `IRONMEM_MCP_MODE` does it run. It is
//! a simple-command reader, not a shell; the limits are documented on
//! [`runs_hook`] and [`hook_mode_with_raw`]. Diagnose-only: it parses strings and never
//! runs anything or touches the filesystem.

use crate::config::McpAccessMode;

const MODE_VAR: &str = "IRONMEM_MCP_MODE";

/// Where a hook's effective `IRONMEM_MCP_MODE` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModeSource {
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
pub(super) enum ModeError {
    /// `Config::load` rejects this raw value, so the hook exits at startup.
    Invalid(String),
    /// The value is built by the shell (`$VAR`, `$(..)`), so doctor can't know it.
    Unknown(String),
}

pub(super) type ModeResult = (Result<McpAccessMode, ModeError>, ModeSource);

pub(super) fn source_label(source: ModeSource) -> &'static str {
    match source {
        ModeSource::Inline => "set in the hook command",
        ModeSource::SettingsEnv => "set by env in settings.json",
        ModeSource::Environment => "inherited from IRONMEM_MCP_MODE in this shell",
        ModeSource::PluginWrapper => "the plugin wrapper's default",
        ModeSource::Default => "IRONMEM_MCP_MODE unset, so the default",
    }
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
/// unquoted word starting with `#` begins a comment; a `#` inside a word is
/// literal. Redirections (`>`, `>>`, `<`, `2>`, `2>&1`, `stop>>/log`) are
/// dropped together with their target; only a word made entirely of digits is
/// read as a file descriptor. An unquoted `$( .. )` (nested parens included,
/// so `$(( .. ))` too) or `` ` .. ` `` is kept whole inside the current word
/// and makes it dynamic; its body is not read. Not modelled: heredocs,
/// here-strings, `$(..)` inside double quotes when the body itself holds a
/// `"`, brace groups, `if` and `case`; those shapes are documented limits,
/// not bugs.
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
                if c == '`' {
                    read_backticks(&mut chars, &mut sp.cur);
                } else if chars.peek() == Some(&'(') {
                    read_parens(&mut chars, &mut sp.cur);
                }
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

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// Copy a backtick substitution body and its closing backtick into `cur`.
fn read_backticks(chars: &mut Chars<'_>, cur: &mut String) {
    while let Some(c) = chars.next() {
        cur.push(c);
        match c {
            '`' => return,
            '\\' => cur.extend(chars.next()),
            _ => {}
        }
    }
}

/// Copy a `( .. )` span (the `$` is already in `cur`) into `cur`, up to the
/// paren that closes the first one. Quoted parens don't count.
fn read_parens(chars: &mut Chars<'_>, cur: &mut String) {
    let mut depth = 0usize;
    while let Some(c) = chars.next() {
        cur.push(c);
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return;
                }
            }
            '\\' => cur.extend(chars.next()),
            '\'' | '"' => {
                while let Some(q) = chars.next() {
                    cur.push(q);
                    if q == c {
                        break;
                    }
                    if q == '\\' && c == '"' {
                        cur.extend(chars.next());
                    }
                }
            }
            _ => {}
        }
    }
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

/// `env` options whose argument is the next word.
const ENV_ARG_FLAGS: [&str; 4] = ["-u", "--unset", "-C", "--chdir"];

/// `bash`/`sh` options whose argument is the next word.
const SHELL_ARG_FLAGS: [&str; 4] = ["-o", "+o", "-O", "+O"];

/// Consume leading assignment words (and, for `env`, option flags) from `rest`.
fn take_assignments(rest: &mut &[Word], inline: &mut Option<ModeValue>, env: bool) {
    while let Some(word) = rest.first() {
        let mut taken = 1;
        if let Some(value) = mode_assignment(word) {
            *inline = Some(value);
        } else if env && ENV_ARG_FLAGS.contains(&word.text.as_str()) {
            taken = 2;
        } else if !(assignment(word).is_some() || env && word.text.starts_with('-')) {
            return;
        }
        *rest = rest.get(taken..).unwrap_or(&[]);
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

/// Handle the arguments of `bash`/`sh`: `-c <string>` (any single-dash option
/// word containing `c`) recurses into the string; `-o`/`+o`/`-O`/`+O` skip
/// their option name; other `-x`/`+x` words are flags; the first other
/// argument is the script, and `ironmem-hook.sh <name>` or
/// `ironmem-mcp.sh hook [--harness <v>] <name>` is the hook.
fn shell_args(args: &[Word], mode: Option<ModeValue>, found: &mut Vec<Invocation>) {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let text = arg.text.as_str();
        // bash reads any single-dash option word containing `c` (`-lc`,
        // `-ce`) as `-c`.
        if text.starts_with('-') && !text.starts_with("--") && text.contains('c') {
            if let Some(script) = args.get(i + 1) {
                found.extend(invocations(&script.text, mode));
            }
            return;
        }
        if SHELL_ARG_FLAGS.contains(&text) {
            i += 2;
            continue;
        }
        if text.starts_with('-') || text.starts_with('+') {
            i += 1;
            continue;
        }
        let rest = &args[i + 1..];
        let hook = match basename(text) {
            "ironmem-hook.sh" => rest.first().map(|w| w.text.as_str()),
            "ironmem-mcp.sh" => hook_after_subcommand(rest),
            _ => None,
        };
        if let Some(hook) = hook {
            found.push(Invocation {
                hook: hook.to_owned(),
                wrapper: true,
                mode,
            });
        }
        return;
    }
}

/// True when `command` runs `ironmem hook <hook>`.
///
/// The model is a small simple-command reader, not a shell (see
/// [`split_commands`] for the word and operator rules). In each simple
/// command, leading `NAME=value` words are assignments; the wrappers `env`,
/// `exec`, `command` and `nohup` are peeled off. `env` takes its own
/// assignments, `-u NAME`/`--unset NAME`, `-C DIR`/`--chdir DIR`, and flags
/// without an argument (`-i`, `-`, `--unset=NAME`). The program's basename
/// must then be `ironmem` or `ironmem-mcp.sh` followed by `hook`, optionally
/// `--harness <v>` or `--harness=<v>`, then the name; or `ironmem-hook.sh
/// <name>`; either wrapper script may also be run by `bash`/`sh` (after flags
/// and `-o`/`+o <opt>`); or `bash`/`sh -c <string>`, which is read
/// recursively. Anything else (`echo`, `printf`, `true`) is not the hook.
///
/// Known limits, documented rather than bugs: control flow is not followed, so
/// `true || ironmem hook stop` counts as wired; heredocs, brace groups and
/// `$(..)` bodies are not read; aliases and functions are unknown; other
/// options that take an argument (`env -S`, `bash --rcfile F`) are not
/// modelled; `env -i` or `env -u IRONMEM_MCP_MODE` clearing an inherited mode
/// is not modelled.
pub(super) fn runs_hook(command: &str, hook: &str) -> bool {
    invocations(command, None).iter().any(|i| i.hook == hook)
}

/// The mode of `command` with the raw value dropped: test shorthand for
/// [`hook_mode_with_raw`], which doctor itself calls.
#[cfg(test)]
pub(super) fn hook_mode(
    command: &str,
    hook: &str,
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> ModeResult {
    hook_mode_with_raw(command, hook, settings_env, process_env).0
}

/// A [`ModeResult`] with the raw `IRONMEM_MCP_MODE` value it was read from
/// (`None` for the wrapper's default and the read-only default).
pub(super) type RawModeResult = (ModeResult, Option<String>);

/// The mode the `hook` run(s) in `command` get, where that came from, and
/// the raw value it was read from, so hints can quote what the file says.
///
/// Only the same simple command counts: the mode comes from the assignment
/// words (or `env` assignments) of the command that runs the hook, or from an
/// earlier `export IRONMEM_MCP_MODE=v` in the same string, and the command's
/// own assignment wins. It never comes from a later or a different simple
/// command, so `ironmem hook stop; IRONMEM_MCP_MODE=trusted true` runs
/// read-only and `IRONMEM_MCP_MODE=bogus true || ironmem hook stop` runs
/// in the default mode. When the command holds several runs of `hook`, one is
/// picked by [`best_result`].
///
/// With no inline value the order is settings.json `env`, then the inherited
/// environment, then the plugin wrapper's trusted default (an empty value
/// counts as unset to the wrapper, but `Config::load` rejects it), then
/// read-only. A value containing `$` or a backtick is [`ModeError::Unknown`].
/// The command-reading limits are listed on [`runs_hook`].
pub(super) fn hook_mode_with_raw(
    command: &str,
    hook: &str,
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> RawModeResult {
    best_result(hook_modes_with_raw(
        command,
        hook,
        settings_env,
        process_env,
    ))
}

/// The [`hook_mode_with_raw`] result of every run of `hook` in `command`, in
/// order and unreduced, for callers that need to know whether any run starts.
pub(super) fn hook_modes_with_raw(
    command: &str,
    hook: &str,
    settings_env: Option<&str>,
    process_env: Option<&str>,
) -> Vec<RawModeResult> {
    invocations(command, None)
        .iter()
        .filter(|i| i.hook == hook)
        .map(|i| {
            let result = invocation_mode(i, settings_env, process_env);
            let raw = match result.1 {
                ModeSource::Inline => i.mode.as_ref().map(|v| v.raw.clone()),
                ModeSource::SettingsEnv => settings_env.map(str::to_owned),
                ModeSource::Environment => process_env.map(str::to_owned),
                ModeSource::PluginWrapper | ModeSource::Default => None,
            };
            (result, raw)
        })
        .collect()
}

/// How good a result is for mining, lowest first: trusted without doctor's
/// shell, trusted via the shell, unknown (it may be trusted), read-only or
/// restricted, then invalid (the hook exits at startup).
fn rank(result: &ModeResult) -> u8 {
    match result {
        (Ok(McpAccessMode::Trusted), source) if *source != ModeSource::Environment => 0,
        (Ok(McpAccessMode::Trusted), _) => 1,
        (Err(ModeError::Unknown(_)), _) => 2,
        (Ok(_), _) => 3,
        (Err(ModeError::Invalid(_)), _) => 4,
    }
}

/// The one result to report for several runs of the same hook (in one
/// command, or across an event's commands): the best by [`rank`], the first
/// in order among equals, so the order of commands decides only ties. With
/// no runs, read-only by default.
pub(super) fn best_result(results: Vec<RawModeResult>) -> RawModeResult {
    results
        .into_iter()
        .min_by_key(|(result, _)| rank(result))
        .unwrap_or(((Ok(McpAccessMode::ReadOnly), ModeSource::Default), None))
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
        Err(_) => (Err(ModeError::Invalid(raw.to_owned())), source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn command_substitution_is_one_dynamic_word() {
        for (cmd, raw) in [
            ("IRONMEM_MCP_MODE=$(cat /m) ironmem hook stop", "$(cat /m)"),
            ("IRONMEM_MCP_MODE=$((1)) ironmem hook stop", "$((1))"),
            (
                "IRONMEM_MCP_MODE=$(echo $(cat /m)) ironmem hook stop",
                "$(echo $(cat /m))",
            ),
            ("IRONMEM_MCP_MODE=`cat /m` ironmem hook stop", "`cat /m`"),
        ] {
            assert!(runs_hook(cmd, "stop"), "{cmd}");
            assert_eq!(
                hook_mode(cmd, "stop", None, None),
                (Err(ModeError::Unknown(raw.to_owned())), ModeSource::Inline),
                "{cmd}"
            );
        }
    }

    #[test]
    fn plugin_mcp_wrapper_run_through_bash_matches() {
        use McpAccessMode::*;
        let cmd = "bash /p/bin/ironmem-mcp.sh hook stop";
        assert!(runs_hook(cmd, "stop"));
        assert_eq!(
            hook_mode(cmd, "stop", None, None),
            (Ok(Trusted), ModeSource::PluginWrapper)
        );
        assert!(runs_hook(
            "sh /p/bin/ironmem-mcp.sh hook --harness claude-code stop",
            "stop"
        ));
        assert!(runs_hook(
            "bash /p/bin/ironmem-mcp.sh hook --harness=claude-code stop",
            "stop"
        ));
        assert!(!runs_hook("bash /p/bin/ironmem-mcp.sh serve", "stop"));
        assert!(!runs_hook("bash /p/bin/ironmem-mcp.sh hook stopx", "stop"));
    }

    #[test]
    fn wrapper_flags_that_take_an_argument_are_skipped_with_it() {
        use McpAccessMode::*;
        assert!(runs_hook("env -u FOO ironmem hook stop", "stop"));
        assert!(runs_hook("env --unset FOO ironmem hook stop", "stop"));
        assert!(runs_hook("env --unset=FOO ironmem hook stop", "stop"));
        assert!(runs_hook("env --chdir /tmp ironmem hook stop", "stop"));
        assert!(runs_hook("env - ironmem hook stop", "stop"));
        let chdir = "env -C /tmp IRONMEM_MCP_MODE=trusted ironmem hook stop";
        assert_eq!(
            hook_mode(chdir, "stop", None, None),
            (Ok(Trusted), ModeSource::Inline)
        );
        assert!(runs_hook("bash -o pipefail -c 'ironmem hook stop'", "stop"));
        assert!(runs_hook(
            "bash +o posix /p/hooks/ironmem-hook.sh stop",
            "stop"
        ));
        assert!(runs_hook(
            "sh -e -o pipefail /p/bin/ironmem-mcp.sh hook stop",
            "stop"
        ));
    }

    #[test]
    fn any_single_dash_shell_option_word_with_c_is_dash_c() {
        assert!(runs_hook("bash -ce 'ironmem hook stop'", "stop"));
        assert!(runs_hook("bash -cl 'ironmem hook stop'", "stop"));
        assert!(runs_hook("sh -ec 'ironmem hook stop'", "stop"));
        assert!(!runs_hook("bash --rcfile-c /p/x.sh stop", "stop"));
    }

    #[test]
    fn invalid_mode_carries_the_raw_value() {
        assert_eq!(
            hook_mode("IRONMEM_MCP_MODE= ironmem hook stop", "stop", None, None),
            (Err(ModeError::Invalid(String::new())), ModeSource::Inline)
        );
        assert_eq!(
            hook_mode("ironmem hook stop", "stop", None, Some("Bogus")),
            (
                Err(ModeError::Invalid("Bogus".to_owned())),
                ModeSource::Environment
            )
        );
    }

    #[test]
    fn env_flags_before_assignments_are_peeled() {
        use McpAccessMode::*;
        let cmd = "env -i IRONMEM_MCP_MODE=trusted ironmem hook stop";
        assert!(runs_hook(cmd, "stop"));
        assert_eq!(
            hook_mode(cmd, "stop", None, None),
            (Ok(Trusted), ModeSource::Inline)
        );
    }

    #[test]
    fn fd_digit_redirections_are_dropped_with_their_target() {
        assert!(runs_hook("ironmem hook stop 2>/dev/null", "stop"));
        assert!(runs_hook("ironmem hook 2>/dev/null stop", "stop"));
        assert!(runs_hook("ironmem hook 2>&1 stop", "stop"));
        // Only a whole-digit word is a file descriptor: `stop2` is the word.
        assert!(!runs_hook("ironmem hook stop2>/dev/null", "stop"));
    }

    #[test]
    fn hash_inside_a_word_is_not_a_comment() {
        assert!(!runs_hook("ironmem hook stop#x", "stop"));
        assert!(runs_hook("ironmem hook stop #x", "stop"));
    }

    #[test]
    fn a_trusted_run_wins_over_an_earlier_read_only_run() {
        use McpAccessMode::*;
        let cmd = "ironmem hook stop; IRONMEM_MCP_MODE=trusted ironmem hook stop";
        assert_eq!(
            hook_mode(cmd, "stop", None, None),
            (Ok(Trusted), ModeSource::Inline)
        );
    }

    #[test]
    fn an_unknown_run_outranks_a_read_only_run_in_either_order() {
        let unknown = (Err(ModeError::Unknown("$M".to_owned())), ModeSource::Inline);
        for cmd in [
            "ironmem hook stop; IRONMEM_MCP_MODE=$M ironmem hook stop",
            "IRONMEM_MCP_MODE=$M ironmem hook stop; ironmem hook stop",
        ] {
            assert_eq!(hook_mode(cmd, "stop", None, None), unknown, "{cmd}");
        }
        let with_invalid =
            "IRONMEM_MCP_MODE=bogus ironmem hook stop; IRONMEM_MCP_MODE=$M ironmem hook stop";
        assert_eq!(hook_mode(with_invalid, "stop", None, None), unknown);
    }

    #[test]
    fn an_assignment_on_another_command_does_not_reach_the_hook() {
        use McpAccessMode::*;
        for cmd in [
            "IRONMEM_MCP_MODE=bogus true || ironmem hook stop",
            "IRONMEM_MCP_MODE=bogus true && ironmem hook stop",
            "IRONMEM_MCP_MODE=bogus true | ironmem hook stop",
            "IRONMEM_MCP_MODE=bogus; ironmem hook stop",
            "(IRONMEM_MCP_MODE=bogus true) ; ironmem hook stop",
        ] {
            assert_eq!(
                hook_mode(cmd, "stop", None, None),
                (Ok(ReadOnly), ModeSource::Default),
                "{cmd}"
            );
        }
        // Several runs in one command pick a result the same way event_mode
        // does across commands: a valid mode beats one that fails to start.
        let two_runs = "IRONMEM_MCP_MODE=bogus ironmem hook stop || ironmem hook stop";
        assert_eq!(
            hook_mode(two_runs, "stop", None, None),
            (Ok(ReadOnly), ModeSource::Default)
        );
    }
}

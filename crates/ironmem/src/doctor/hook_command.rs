//! A small model of the shell commands in Claude Code hook entries (#377).
//!
//! Answers two questions about a hook command string: does it run
//! `ironmem hook <name>`, and in which `IRONMEM_MCP_MODE` does it run. It is
//! a simple-command reader, not a shell; the limits are documented on
//! [`runs_hook`] and [`hook_mode`]. Diagnose-only: it parses strings and never
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
    /// `Config::load` rejects the value, so the hook exits at startup.
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

pub(super) fn mode_label(mode: McpAccessMode) -> &'static str {
    match mode {
        McpAccessMode::Trusted => "trusted",
        McpAccessMode::ReadOnly => "read-only",
        McpAccessMode::Restricted => "restricted",
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
pub(super) fn runs_hook(command: &str, hook: &str) -> bool {
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
pub(super) fn hook_mode(
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
pub(super) fn first_trusted(results: &[ModeResult]) -> Option<&ModeResult> {
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
}

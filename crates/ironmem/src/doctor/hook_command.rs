//! A small model of the commands in Claude Code hook entries (#377).
//!
//! Answers two questions about a hook entry ([`HookCommand`], shell form or
//! exec form): does it run `ironmem hook <name>`, and in which
//! `IRONMEM_MCP_MODE` does it run. For shell form it is a simple-command
//! reader, not a shell; the limits are documented on
//! [`HookCommand::runs_hook`] and [`HookCommand::hook_modes`]. Diagnose-only:
//! it parses strings and never runs anything or touches the filesystem; it
//! names the literal program path a run starts ([`RunMode::program`]) for
//! `hooks.rs` to check.

use crate::config::McpAccessMode;

const MODE_VAR: &str = "IRONMEM_MCP_MODE";

/// The mode the plugin wrapper `.claude-plugin/bin/ironmem-mcp.sh` gives a
/// hook when `IRONMEM_MCP_MODE` is unset or empty
/// (`${IRONMEM_MCP_MODE:-trusted}`). A unit test pins it to that line.
pub(super) const WRAPPER_DEFAULT: McpAccessMode = McpAccessMode::Trusted;

/// Where a hook can get `IRONMEM_MCP_MODE` from besides its own command.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ModeEnv<'a> {
    /// `env.IRONMEM_MCP_MODE` in settings.json.
    pub(super) settings: Option<&'a str>,
    /// `IRONMEM_MCP_MODE` in doctor's own environment, used as a stand-in for
    /// the environment Claude Code passes to its hooks.
    pub(super) process: Option<&'a str>,
}

/// Where a hook's effective `IRONMEM_MCP_MODE` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModeSource {
    /// `IRONMEM_MCP_MODE=...` in the hook command itself.
    Inline,
    /// `env.IRONMEM_MCP_MODE` in settings.json. Claude Code's docs say it
    /// writes each `env` entry into its process environment, replacing the
    /// value inherited from the shell, and that a hook process inherits that
    /// environment; so this beats [`Self::Environment`].
    SettingsEnv,
    /// `IRONMEM_MCP_MODE` in doctor's own environment, used as a stand-in
    /// for the environment Claude Code passes to its hooks.
    Environment,
    /// The plugin wrapper's `${IRONMEM_MCP_MODE:-trusted}` default.
    PluginWrapper,
    /// Nothing set it: `Config::load` uses [`McpAccessMode::UNSET_DEFAULT`].
    Default,
}

/// Why a run of a hook has no usable [`McpAccessMode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ModeError {
    /// `Config::load` rejects this raw value, so the hook exits at startup.
    Invalid(String),
    /// The value is built by the shell (`$VAR`, `$(..)`), so doctor can't know it.
    Unknown(String),
    /// The run's program is this literal path, which is not a runnable file,
    /// so it cannot start at all. Set by `hooks.rs`, which checks
    /// [`RunMode::program`] on the file system, and by
    /// [`HookCommand::hook_modes`] for [`ProgramProblem::TildeNotExpanded`].
    NoProgram(String, ProgramProblem),
}

/// What is wrong with a hook's literal program path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProgramProblem {
    /// Nothing exists at the path.
    Missing,
    /// Something exists there, but it is not a file with an execute bit.
    NotExecutable,
    /// An exec-form program (or the program `env` and the like run from its
    /// arguments) starts with `~/`. Claude Code's hooks docs say exec form
    /// runs no shell, passes each element "exactly as written" and
    /// substitutes only path placeholders such as `${CLAUDE_PLUGIN_ROOT}`; they
    /// never mention `~`, so doctor reads it as left literal, a path under a
    /// directory named `~` that no install creates (inferred, not stated by
    /// the docs).
    TildeNotExpanded,
}

impl ProgramProblem {
    /// The clause after "which", e.g. "which does not exist".
    pub(super) fn describe(self) -> &'static str {
        match self {
            Self::Missing => "does not exist",
            Self::NotExecutable => "is not an executable file",
            Self::TildeNotExpanded => {
                "starts with a ~ that is not expanded, because with args set (exec form) no \
                 shell reads the command"
            }
        }
    }
}

/// The mode one run of a hook gets, where that came from, and the raw
/// `IRONMEM_MCP_MODE` value it was read from (`None` for the wrapper's
/// default and the unset default), so hints can quote what the file says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RunMode {
    pub(super) mode: Result<McpAccessMode, ModeError>,
    pub(super) source: ModeSource,
    pub(super) raw: Option<String>,
    /// The program word as written, when it is a literal absolute path or
    /// begins with an unquoted `~/` and nothing in it is built by the shell
    /// (no `$`, backtick, glob or brace). `None` for a PATH lookup (`ironmem`)
    /// and for anything doctor cannot know; those are not checked.
    pub(super) program: Option<String>,
    /// Where the run sits in its entry, which decides how a fix adds an
    /// assignment to it.
    pub(super) form: RunForm,
}

/// Where a run sits in its hook entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunForm {
    /// A shell-form `command` string: an assignment prefix works.
    Shell,
    /// An exec-form entry's own argument vector: no shell reads `command`,
    /// so an assignment prefix there would rename the program.
    Exec,
    /// A shell string inside an exec-form entry (`bash -c <string>` in
    /// `args`): the fix goes inside that string, not on `command`.
    ExecString,
}

impl RunMode {
    /// A mode nothing in the hook's command or environment set.
    fn defaulted(mode: McpAccessMode, source: ModeSource) -> Self {
        Self {
            mode: Ok(mode),
            source,
            raw: None,
            program: None,
            form: RunForm::Shell,
        }
    }
}

pub(super) fn source_label(source: ModeSource) -> &'static str {
    match source {
        ModeSource::Inline => "set in the hook command",
        ModeSource::SettingsEnv => "set by env in settings.json",
        ModeSource::Environment => "inherited from IRONMEM_MCP_MODE in this shell",
        ModeSource::PluginWrapper => "the plugin wrapper's default",
        ModeSource::Default => "IRONMEM_MCP_MODE unset, so the default",
    }
}

/// One hook entry's command, as Claude Code runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HookCommand {
    /// Shell form (no `args`): a shell runs the `command` string.
    Shell(String),
    /// Exec form (`args` set, an array of strings): Claude Code spawns
    /// `program` directly with `args` as the argument vector, no shell.
    Exec { program: String, args: Vec<String> },
}

impl HookCommand {
    /// True when the entry runs `ironmem hook <hook>`.
    ///
    /// Shell form: the model is a small simple-command reader, not a shell
    /// (see [`split_commands`] for the word and operator rules). In each
    /// simple command, leading `NAME=value` words are assignments; the
    /// wrappers `env`, `exec`, `command` and `nohup` are peeled off. `env`
    /// takes its own assignments, `-u NAME`/`--unset NAME`, `-C DIR`/`--chdir
    /// DIR`, and flags without an argument (`-i`, `-`, `--unset=NAME`). The
    /// program's basename must then be `ironmem` or `ironmem-mcp.sh` followed
    /// by `hook`, optionally `--harness <v>` or `--harness=<v>`, then the
    /// name; or `ironmem-hook.sh <name>`; either wrapper script may also be
    /// run by `bash`/`sh` (after flags and `-o`/`+o <opt>`); or `bash`/`sh -c
    /// <string>`, which is read recursively. Anything else (`echo`, `printf`,
    /// `true`) is not the hook.
    ///
    /// Exec form: `program` and `args` are one simple command whose words are
    /// exactly the elements, with no quoting, operators or leading
    /// assignments (a `NAME=value` program is a program of that name). The
    /// same wrappers, `env` options and programs apply to that vector, and a
    /// `bash`/`sh -c <string>` element is read as shell form. An element
    /// containing `${` holds a path placeholder Claude Code may substitute,
    /// so doctor treats it as built at run time; any other `$` is literal.
    ///
    /// Known limits, documented rather than bugs: control flow is not
    /// followed, so `true || ironmem hook stop` counts as wired; heredocs,
    /// brace groups and `$(..)` bodies are not read; aliases and functions are
    /// unknown; other options that take an argument (`env -S`, `bash --rcfile
    /// F`) are not modelled; `env -i` or `env -u IRONMEM_MCP_MODE` clearing an
    /// inherited mode is not modelled. The other way round, a hook that does
    /// run reads as missing when it sits behind `if ...; then`, `{ ...; }`,
    /// `timeout 5`, `time`, `nice`, `command -p` or any wrapper other than
    /// `env`, `exec`, `command` and `nohup`: the first word is then not a
    /// program doctor knows. scripts/install-ironmem.sh matches such shell-form
    /// commands as already wired, so doctor's missing-hook hint says what to do
    /// then.
    pub(super) fn runs_hook(&self, hook: &str) -> bool {
        self.invocations().iter().any(|i| i.hook == hook)
    }

    /// The [`RunMode`] of every run of `hook` in the entry, in order.
    ///
    /// Only the same simple command counts: the mode comes from the
    /// assignment words (or `env` assignments) of the command that runs the
    /// hook, or, in shell form, from an earlier `export IRONMEM_MCP_MODE=v` in
    /// the same string, and the command's own assignment wins. It never comes
    /// from a later or a different simple command, so `ironmem hook stop;
    /// IRONMEM_MCP_MODE=trusted true` runs read-only and
    /// `IRONMEM_MCP_MODE=bogus true || ironmem hook stop` runs in the default
    /// mode. Callers that report one result for several runs reduce them with
    /// [`best_result`].
    ///
    /// With no inline value the order is settings.json `env`, then the
    /// inherited environment, then the plugin wrapper's [`WRAPPER_DEFAULT`]
    /// (an empty value counts as unset to the wrapper, but `Config::load`
    /// rejects it), then [`McpAccessMode::UNSET_DEFAULT`]. A value the shell
    /// builds (`$`, a backtick) or, in exec form, one holding `${`, is
    /// [`ModeError::Unknown`]. An exec-form program starting with `~/` is
    /// [`ProgramProblem::TildeNotExpanded`]. The command-reading limits are
    /// listed on [`Self::runs_hook`].
    pub(super) fn hook_modes(&self, hook: &str, env: ModeEnv<'_>) -> Vec<RunMode> {
        self.invocations()
            .iter()
            .filter(|i| i.hook == hook)
            .map(|i| {
                let mut run = RunMode {
                    program: i.program.clone(),
                    form: i.form,
                    ..invocation_mode(i, env)
                };
                if let Some(path) = &i.unexpanded_tilde {
                    run.mode = Err(ModeError::NoProgram(
                        path.clone(),
                        ProgramProblem::TildeNotExpanded,
                    ));
                }
                run
            })
            .collect()
    }

    fn invocations(&self) -> Vec<Invocation> {
        match self {
            Self::Shell(command) => invocations(command, None),
            Self::Exec { program, args } => {
                let words: Vec<Word> = std::iter::once(program)
                    .chain(args)
                    .map(|text| Word::exec(text))
                    .collect();
                let mut found = Vec::new();
                simple_command(&words, None, None, &mut found);
                found
            }
        }
    }
}

/// One word of a shell command after quote removal, or one element of an
/// exec-form entry.
#[derive(Debug, Clone)]
struct Word {
    text: String,
    /// Contains an unquoted or double-quoted `$` or backtick: the shell
    /// decides its value, not us. For an exec-form element: contains `${`,
    /// a path placeholder Claude Code may substitute.
    dynamic: bool,
    /// The byte offset in `text` where the first quoted or backslash-escaped
    /// character went, if any. The shell reads a `NAME=value` word as an
    /// assignment only when nothing up to its first `=` was quoted. An
    /// exec-form element counts as quoted from its first byte: nothing in it
    /// is shell syntax.
    first_quoted: Option<usize>,
    /// An element of an exec-form entry, which no shell reads.
    exec: bool,
}

impl Word {
    /// One element of an exec-form entry.
    fn exec(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            dynamic: text.contains("${"),
            first_quoted: Some(0),
            exec: true,
        }
    }

    /// True when a quote or backslash came before the first `=` (or there is
    /// no `=`), so the shell does not read the word as an assignment.
    fn quoted_name(&self) -> bool {
        match (self.first_quoted, self.text.find('=')) {
            (None, _) => false,
            (Some(q), Some(eq)) => q <= eq,
            (Some(_), None) => true,
        }
    }
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
    /// See [`Word::first_quoted`].
    first_quoted: Option<usize>,
    /// The next word is a redirection target and is dropped.
    skip_next: bool,
}

impl Splitter {
    fn end_word(&mut self) {
        if self.in_word {
            let word = Word {
                text: std::mem::take(&mut self.cur),
                dynamic: self.dynamic,
                first_quoted: self.first_quoted,
                exec: false,
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
        self.first_quoted = None;
    }

    /// Note that the next character pushed to `cur` is quoted or escaped.
    fn mark_quoted(&mut self) {
        self.first_quoted.get_or_insert(self.cur.len());
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
                sp.mark_quoted();
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
                sp.mark_quoted();
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
                    sp.mark_quoted();
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
                    sp.first_quoted = None;
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

/// A `NAME=value` word, if it is one. `to_shell` says the shell itself reads
/// the word (a leading assignment), which rejects a quoted name; `env` and
/// `export` read their arguments after quote removal, so they don't.
fn assignment(word: &Word, to_shell: bool) -> Option<(&str, &str)> {
    if to_shell && word.quoted_name() {
        return None;
    }
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

fn mode_assignment(word: &Word, to_shell: bool) -> Option<ModeValue> {
    match assignment(word, to_shell) {
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
    /// See [`RunMode::program`].
    program: Option<String>,
    /// An exec-form program word starting with `~/`, which doctor reads as
    /// not expanded ([`ProgramProblem::TildeNotExpanded`]).
    unexpanded_tilde: Option<String>,
    /// See [`RunMode::form`].
    form: RunForm,
}

/// The program word's literal path, if doctor can check it: see
/// [`RunMode::program`].
fn literal_path(word: &Word) -> Option<String> {
    let text = word.text.as_str();
    if word.dynamic || text.contains(['*', '?', '[', '{']) {
        return None;
    }
    // The shell expands `~/` only when neither character is quoted.
    let tilde = text.starts_with("~/") && !matches!(word.first_quoted, Some(0 | 1));
    (text.starts_with('/') || tilde).then(|| text.to_owned())
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
        if let Some(value) = mode_assignment(word, !env) {
            *inline = Some(value);
        } else if env && ENV_ARG_FLAGS.contains(&word.text.as_str()) {
            taken = 2;
        } else if !(assignment(word, !env).is_some() || env && word.text.starts_with('-')) {
            return;
        }
        *rest = rest.get(taken..).unwrap_or(&[]);
    }
}

/// The `ironmem hook` runs in shell-form `command`; see
/// [`HookCommand::runs_hook`] for the model.
fn invocations(command: &str, inherited: Option<ModeValue>) -> Vec<Invocation> {
    let mut exported = inherited;
    let mut found = Vec::new();
    for words in split_commands(command) {
        let mut inline = None;
        let mut rest: &[Word] = &words;
        take_assignments(&mut rest, &mut inline, false);
        if let Some(value) = simple_command(rest, inline, exported.as_ref(), &mut found) {
            exported = Some(value);
        }
    }
    found
}

/// Read one simple command, `words`, whose leading assignments (shell form)
/// are already taken into `inline`: peel the `env`/`exec`/`command`/`nohup`
/// wrappers, then push the hook runs it starts to `found`. `exported` is the
/// mode an earlier `export` set. Returns the value an `export
/// IRONMEM_MCP_MODE=v` command sets, for the simple commands after it.
fn simple_command(
    words: &[Word],
    mut inline: Option<ModeValue>,
    exported: Option<&ModeValue>,
    found: &mut Vec<Invocation>,
) -> Option<ModeValue> {
    let mut rest = words;
    while let Some(first) = rest.first() {
        let name = basename(&first.text);
        // `exec` is a shell builtin with no program behind it, so an
        // argument vector that names it as a program (directly or after
        // `env`) never starts; it is left as the program, which is no hook.
        let builtin_in_exec = name == "exec" && first.exec;
        if !matches!(name, "env" | "exec" | "command" | "nohup") || builtin_in_exec {
            break;
        }
        rest = &rest[1..];
        if name == "env" {
            take_assignments(&mut rest, &mut inline, true);
        }
    }
    let (program_word, args) = rest.split_first()?;
    let program = basename(&program_word.text);
    if program == "export" {
        return args.iter().find_map(|w| mode_assignment(w, false));
    }
    let mode = inline.or_else(|| exported.cloned());
    let unexpanded_tilde = (program_word.exec && program_word.text.starts_with("~/"))
        .then(|| program_word.text.clone());
    let mut push = |hook: &str, wrapper: bool| {
        found.push(Invocation {
            hook: hook.to_owned(),
            wrapper,
            mode: mode.clone(),
            program: literal_path(program_word),
            unexpanded_tilde: unexpanded_tilde.clone(),
            form: if program_word.exec {
                RunForm::Exec
            } else {
                RunForm::Shell
            },
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
        "bash" | "sh" => shell_args(args, mode.clone(), found),
        _ => {}
    }
    None
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
                let runs = invocations(&script.text, mode);
                // A string from an exec-form entry's args: a fix edits it.
                found.extend(runs.into_iter().map(|mut run| {
                    if script.exec {
                        run.form = RunForm::ExecString;
                    }
                    run
                }));
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
            // The program is the shell, found on PATH; the script it runs
            // is not checked.
            found.push(Invocation {
                hook: hook.to_owned(),
                wrapper: true,
                mode,
                program: None,
                unexpanded_tilde: None,
                form: if arg.exec {
                    RunForm::Exec
                } else {
                    RunForm::Shell
                },
            });
        }
        return;
    }
}

/// Shell-form shorthand for [`HookCommand::runs_hook`].
#[cfg(test)]
pub(super) fn runs_hook(command: &str, hook: &str) -> bool {
    HookCommand::Shell(command.to_owned()).runs_hook(hook)
}

/// The best mode of the `hook` runs in shell-form `command`, without its raw
/// value: test shorthand for [`best_result`] over [`HookCommand::hook_modes`].
#[cfg(test)]
pub(super) fn hook_mode(
    command: &str,
    hook: &str,
    env: ModeEnv<'_>,
) -> (Result<McpAccessMode, ModeError>, ModeSource) {
    let run = best_result(HookCommand::Shell(command.to_owned()).hook_modes(hook, env));
    (run.mode, run.source)
}

/// How good a run is for mining, lowest first: can mine (its mode
/// [`McpAccessMode::allows_writes`], the hook's own gate) without doctor's
/// shell, can mine via the shell, unknown (it may mine), cannot mine, then
/// does not start (an invalid mode, or a program that is not there).
fn rank(run: &RunMode) -> u8 {
    match (&run.mode, run.source) {
        (Ok(mode), source) if mode.allows_writes() && source != ModeSource::Environment => 0,
        (Ok(mode), _) if mode.allows_writes() => 1,
        (Err(ModeError::Unknown(_)), _) => 2,
        (Ok(_), _) => 3,
        (Err(ModeError::Invalid(_) | ModeError::NoProgram(..)), _) => 4,
    }
}

/// The one result to report for several runs of the same hook (in one
/// command, or across an event's commands): the best by [`rank`], the first
/// in order among equals, so the order of commands decides only ties. With
/// no runs, the unset default.
pub(super) fn best_result(runs: Vec<RunMode>) -> RunMode {
    runs.into_iter()
        .min_by_key(rank)
        .unwrap_or_else(|| RunMode::defaulted(McpAccessMode::UNSET_DEFAULT, ModeSource::Default))
}

fn invocation_mode(run: &Invocation, env: ModeEnv<'_>) -> RunMode {
    let (raw, dynamic, source) = match (&run.mode, env.settings, env.process) {
        (Some(v), _, _) => (v.raw.as_str(), v.dynamic, ModeSource::Inline),
        (None, Some(v), _) => (v, false, ModeSource::SettingsEnv),
        (None, None, Some(v)) => (v, false, ModeSource::Environment),
        (None, None, None) if run.wrapper => {
            return RunMode::defaulted(WRAPPER_DEFAULT, ModeSource::PluginWrapper)
        }
        (None, None, None) => {
            return RunMode::defaulted(McpAccessMode::UNSET_DEFAULT, ModeSource::Default)
        }
    };
    let read = |mode| RunMode {
        mode,
        source,
        raw: Some(raw.to_owned()),
        program: None,
        form: RunForm::Shell,
    };
    if dynamic {
        return read(Err(ModeError::Unknown(raw.to_owned())));
    }
    if run.wrapper && raw.is_empty() {
        return RunMode::defaulted(WRAPPER_DEFAULT, ModeSource::PluginWrapper);
    }
    match McpAccessMode::parse(raw) {
        // The wrapper defaults to this mode, so it holds without doctor's shell.
        Ok(mode) if run.wrapper && source == ModeSource::Environment && mode == WRAPPER_DEFAULT => {
            RunMode::defaulted(mode, ModeSource::PluginWrapper)
        }
        Ok(mode) => read(Ok(mode)),
        Err(_) => read(Err(ModeError::Invalid(raw.to_owned()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only doctor's own environment sets IRONMEM_MCP_MODE, to `mode`.
    fn shell(mode: &str) -> ModeEnv<'_> {
        ModeEnv {
            settings: None,
            process: Some(mode),
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
            hook_mode(bare, "stop", ModeEnv::default()),
            (Ok(ReadOnly), ModeSource::Default)
        );
        assert_eq!(
            hook_mode(bare, "stop", shell("trusted")),
            (Ok(Trusted), ModeSource::Environment)
        );
        assert_eq!(
            hook_mode(
                bare,
                "stop",
                ModeEnv {
                    settings: Some("trusted"),
                    process: Some("read-only")
                }
            ),
            (Ok(Trusted), ModeSource::SettingsEnv)
        );
        assert_eq!(
            hook_mode(
                "IRONMEM_MCP_MODE='read-only' ironmem hook stop",
                "stop",
                ModeEnv {
                    settings: Some("trusted"),
                    process: Some("trusted")
                }
            ),
            (Ok(ReadOnly), ModeSource::Inline)
        );
        assert_eq!(
            hook_mode(
                "bash /p/hooks/ironmem-hook.sh stop",
                "stop",
                ModeEnv::default()
            ),
            (Ok(Trusted), ModeSource::PluginWrapper)
        );
        assert_eq!(
            hook_mode(
                "bash /p/hooks/ironmem-hook.sh stop",
                "stop",
                shell("read-only")
            ),
            (Ok(ReadOnly), ModeSource::Environment),
            "the wrapper only defaults; an inherited mode wins"
        );
        assert!(hook_mode(
            "IRONMEM_MCP_MODE=bogus ironmem hook stop",
            "stop",
            ModeEnv::default()
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
                ModeEnv::default()
            ),
            (Ok(Trusted), ModeSource::Inline)
        );
        assert_eq!(
            hook_mode(
                "export IRONMEM_MCP_MODE=\"read-only\"; ironmem hook stop",
                "stop",
                ModeEnv::default()
            ),
            (Ok(ReadOnly), ModeSource::Inline)
        );
    }

    #[test]
    fn mode_is_scoped_to_the_simple_command_that_runs_the_hook() {
        use McpAccessMode::*;
        let later = "ironmem hook stop; IRONMEM_MCP_MODE=trusted true";
        assert_eq!(
            hook_mode(later, "stop", ModeEnv::default()),
            (Ok(ReadOnly), ModeSource::Default)
        );
        let two = "IRONMEM_MCP_MODE=read-only ironmem hook precompact; \
                   IRONMEM_MCP_MODE=trusted ironmem hook stop";
        assert_eq!(
            hook_mode(two, "stop", ModeEnv::default()),
            (Ok(Trusted), ModeSource::Inline)
        );
        assert_eq!(
            hook_mode(two, "precompact", ModeEnv::default()),
            (Ok(ReadOnly), ModeSource::Inline)
        );
        let exported = "export IRONMEM_MCP_MODE=trusted; ironmem hook stop";
        assert_eq!(
            hook_mode(exported, "stop", ModeEnv::default()),
            (Ok(Trusted), ModeSource::Inline)
        );
        let inline_wins =
            "export IRONMEM_MCP_MODE=trusted; IRONMEM_MCP_MODE=read-only ironmem hook stop";
        assert_eq!(
            hook_mode(inline_wins, "stop", ModeEnv::default()),
            (Ok(ReadOnly), ModeSource::Inline)
        );
        let late_export = "ironmem hook stop; export IRONMEM_MCP_MODE=trusted";
        assert_eq!(
            hook_mode(late_export, "stop", ModeEnv::default()),
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
            hook_mode(env, "stop", ModeEnv::default()),
            (Ok(Trusted), ModeSource::Inline)
        );
        let sh = "IRONMEM_MCP_MODE=trusted sh -c 'ironmem hook stop'";
        assert_eq!(
            hook_mode(sh, "stop", ModeEnv::default()),
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
                hook_mode(cmd, "stop", ModeEnv::default()),
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
            hook_mode(cmd, "stop", ModeEnv::default()),
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
            hook_mode(chdir, "stop", ModeEnv::default()),
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
            hook_mode(
                "IRONMEM_MCP_MODE= ironmem hook stop",
                "stop",
                ModeEnv::default()
            ),
            (Err(ModeError::Invalid(String::new())), ModeSource::Inline)
        );
        // A defensive case: the CLI's `Config::load` rejects an invalid
        // IRONMEM_MCP_MODE in doctor's own environment before doctor runs.
        assert_eq!(
            hook_mode("ironmem hook stop", "stop", shell("Bogus")),
            (
                Err(ModeError::Invalid("Bogus".to_owned())),
                ModeSource::Environment
            )
        );
    }

    #[test]
    fn a_quoted_name_is_not_an_assignment_to_the_shell() {
        // The shell runs a program named `IRONMEM_MCP_MODE=trusted`, so
        // neither command runs the hook.
        for cmd in [
            r#""IRONMEM_MCP_MODE=trusted" ironmem hook stop"#,
            "'IRONMEM_MCP_MODE'=trusted ironmem hook stop",
            r"IRONMEM_MCP_MODE\=trusted ironmem hook stop",
        ] {
            assert!(!runs_hook(cmd, "stop"), "{cmd}");
        }
        // A later simple command can still run it, in the default mode.
        let later = r#""IRONMEM_MCP_MODE=trusted" true; ironmem hook stop"#;
        assert_eq!(
            hook_mode(later, "stop", ModeEnv::default()),
            (Ok(McpAccessMode::ReadOnly), ModeSource::Default)
        );
    }

    #[test]
    fn a_quoted_value_is_still_an_assignment() {
        for cmd in [
            r#"A="1" ironmem hook stop"#,
            "A='1' ironmem hook stop",
            "A=1 ironmem hook stop",
        ] {
            assert!(runs_hook(cmd, "stop"), "{cmd}");
        }
        assert_eq!(
            hook_mode(
                r#"IRONMEM_MCP_MODE="trusted" ironmem hook stop"#,
                "stop",
                ModeEnv::default()
            ),
            (Ok(McpAccessMode::Trusted), ModeSource::Inline)
        );
    }

    #[test]
    fn env_and_export_read_a_quoted_assignment_after_quote_removal() {
        // `env` and `export` see the word without its quotes, so to them
        // it is an assignment.
        for cmd in [
            r#"env "IRONMEM_MCP_MODE=trusted" ironmem hook stop"#,
            r#"export "IRONMEM_MCP_MODE=trusted"; ironmem hook stop"#,
        ] {
            assert_eq!(
                hook_mode(cmd, "stop", ModeEnv::default()),
                (Ok(McpAccessMode::Trusted), ModeSource::Inline),
                "{cmd}"
            );
        }
    }

    #[test]
    fn a_hook_behind_control_flow_or_another_wrapper_is_not_recognised() {
        // Documented limit (see `runs_hook`): these run the hook, but the
        // first word is not a program doctor knows, so they read as missing.
        for cmd in [
            "if true; then ironmem hook stop; fi",
            "{ ironmem hook stop; }",
            "timeout 5 ironmem hook stop",
            "command -p ironmem hook stop",
        ] {
            assert!(!runs_hook(cmd, "stop"), "{cmd}");
        }
    }

    #[test]
    fn env_flags_before_assignments_are_peeled() {
        use McpAccessMode::*;
        let cmd = "env -i IRONMEM_MCP_MODE=trusted ironmem hook stop";
        assert!(runs_hook(cmd, "stop"));
        assert_eq!(
            hook_mode(cmd, "stop", ModeEnv::default()),
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
            hook_mode(cmd, "stop", ModeEnv::default()),
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
            assert_eq!(hook_mode(cmd, "stop", ModeEnv::default()), unknown, "{cmd}");
        }
        let with_invalid =
            "IRONMEM_MCP_MODE=bogus ironmem hook stop; IRONMEM_MCP_MODE=$M ironmem hook stop";
        assert_eq!(hook_mode(with_invalid, "stop", ModeEnv::default()), unknown);
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
                hook_mode(cmd, "stop", ModeEnv::default()),
                (Ok(ReadOnly), ModeSource::Default),
                "{cmd}"
            );
        }
        // Several runs in one command pick a result the same way event_mode
        // does across commands: a valid mode beats one that fails to start.
        let two_runs = "IRONMEM_MCP_MODE=bogus ironmem hook stop || ironmem hook stop";
        assert_eq!(
            hook_mode(two_runs, "stop", ModeEnv::default()),
            (Ok(ReadOnly), ModeSource::Default)
        );
    }
}

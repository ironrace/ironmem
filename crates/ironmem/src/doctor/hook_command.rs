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
    /// The run's program (or wrapper script) is this word, as written, and
    /// it cannot start, for this reason. Set only by `hooks.rs`, which
    /// resolves [`RunMode::program`].
    NoProgram(String, ProgramProblem),
}

/// What is wrong with a hook's program path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProgramProblem {
    /// Nothing exists at the path.
    Missing,
    /// Something exists there, but it is not a file with an execute bit.
    NotExecutable,
    /// Looking the path up failed with this error, other than not found:
    /// a regular file used as a directory, a symlink loop, or a directory
    /// the user may not search. The hook runs as the same user, so its
    /// lookup fails the same way.
    Unreachable(std::io::ErrorKind),
    /// The program or wrapper-script word uses `${CLAUDE_PLUGIN_ROOT}` (or
    /// `$CLAUDE_PLUGIN_ROOT`) in a non-plugin install. Measured on Claude
    /// Code 2.1.329: a `settings.json` hook without the plugin runs with
    /// that variable unset, so the run finds no script. A use the shell
    /// gives a default (`${CLAUDE_PLUGIN_ROOT:-/path}`) is not this problem:
    /// see [`uses_plugin_root`].
    PluginRootOutsidePlugin,
    /// An exec-form program (or the program `env` and the like run from its
    /// arguments) starts with `~/`. Claude Code's hooks docs say exec form
    /// runs no shell, passes each element "exactly as written" and
    /// substitutes path placeholders such as `${CLAUDE_PLUGIN_ROOT}`; they
    /// never mention `~`, so doctor reads it as left literal, a path under a
    /// directory named `~` that no install creates. That is inferred, not
    /// stated by the docs, and the printed reason says so; its hint (correct
    /// the program path) fixes the hook whichever way Claude Code reads it.
    TildeNotExpanded,
}

impl ProgramProblem {
    /// The clause after "which", e.g. "which does not exist".
    pub(super) fn describe(self) -> String {
        match self {
            Self::Missing => "does not exist".to_owned(),
            Self::NotExecutable => "is not an executable file".to_owned(),
            Self::Unreachable(kind) => format!("cannot be reached ({kind})"),
            Self::PluginRootOutsidePlugin => "uses ${CLAUDE_PLUGIN_ROOT}, a variable Claude Code \
                 sets only for plugin hooks"
                .to_owned(),
            Self::TildeNotExpanded => "starts with a ~ that doctor reads as not expanded (in exec \
                 form, with args set, no shell reads the command, and Claude Code's hooks docs do \
                 not say that it expands ~)"
                .to_owned(),
        }
    }
}

/// The program a run starts, as far as doctor can tell from the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Program {
    /// A PATH lookup (`ironmem`, or `bash` running a wrapper script, whose
    /// script is not checked), a path the shell builds, or anything else
    /// doctor cannot know: not checked.
    Unchecked,
    /// The program word as written: a literal absolute path, or one that
    /// begins with an unquoted `~/`, with nothing in it built by the shell
    /// (no `$`, backtick, glob or brace). `hooks.rs` checks it on the file
    /// system.
    Literal(String),
    /// An exec-form program word starting with `~/`, which doctor reads as
    /// not expanded ([`ProgramProblem::TildeNotExpanded`]).
    ExecTilde(String),
    /// A program or wrapper-script word, as written, that uses
    /// `${CLAUDE_PLUGIN_ROOT}` or `$CLAUDE_PLUGIN_ROOT` with nothing to fall
    /// back on when it is unset ([`uses_plugin_root`];
    /// [`ProgramProblem::PluginRootOutsidePlugin`] outside the plugin).
    PluginRoot(String),
}

/// The mode one run of a hook gets, where that came from, and the raw
/// `IRONMEM_MCP_MODE` value it was read from (`None` for the wrapper's
/// default and the unset default), so hints can quote what the file says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RunMode {
    pub(super) mode: Result<McpAccessMode, ModeError>,
    pub(super) source: ModeSource,
    pub(super) raw: Option<String>,
    /// The program the run starts, which `hooks.rs` resolves.
    pub(super) program: Program,
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
            program: Program::Unchecked,
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
    /// wrappers `env`, `exec`, `command` and `nohup` are peeled off, except an
    /// `exec` right after `env`, `nohup` or `exec`, which run their argument
    /// as a program and find none (`env exec ironmem hook stop` never
    /// starts); `command exec` runs the builtin and is peeled. `env`
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
    /// same wrappers, `env` options and programs apply to that vector, except
    /// `exec`: it is a shell builtin with no program to spawn, so a vector
    /// that names it (as `command`, or after `env`) never starts and is not
    /// the hook. A `bash`/`sh -c <string>` element is read as shell form. An element
    /// containing `${` holds a path placeholder Claude Code may substitute,
    /// so doctor treats it as built at run time; any other `$` is literal.
    ///
    /// A run whose program or wrapper-script word uses `${CLAUDE_PLUGIN_ROOT}`
    /// still counts here; its [`RunMode::program`] is [`Program::PluginRoot`],
    /// which `hooks.rs` reads as not starting outside the plugin.
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
    /// program doctor knows.
    ///
    /// scripts/install-ironmem.sh reads shell-form commands with a regex that
    /// wants ironmem in the same program positions and reads these shapes the
    /// same way; scripts/fixtures/hook_stop_fixtures.json pins both to one
    /// answer per entry. It still differs in two ways: it reads a `;`, `&`,
    /// `|`, `(`, `)` or line break inside quotes or a comment as the start of
    /// a command (so `echo "x; ironmem hook stop"` is already wired to it and
    /// missing to doctor, and doctor's missing-hook hint says what to do then),
    /// and it does not skip a redirection before the program (`2>/dev/null
    /// ironmem hook stop` is wired to doctor, and the installer appends a
    /// second copy).
    pub(super) fn runs_hook(&self, hook: &str) -> bool {
        self.invocations().iter().any(|i| i.hook == hook)
    }

    /// The [`RunMode`] of every run of `hook` in the entry, in order.
    ///
    /// The mode comes from the assignment words (or `env` assignments) of the
    /// simple command that runs the hook, which win, or, in shell form, from
    /// what earlier simple commands in the same string did to the variable:
    /// `export IRONMEM_MCP_MODE=v`; `IRONMEM_MCP_MODE=v` alone, which reaches
    /// later programs only when the variable was inherited (settings.json
    /// `env` or the environment set it) or is exported after it with `export
    /// IRONMEM_MCP_MODE`; and `unset IRONMEM_MCP_MODE`, after which no
    /// inherited value reaches them. An assignment prefix on another command
    /// never counts, so `ironmem hook stop; IRONMEM_MCP_MODE=trusted true`
    /// runs read-only and `IRONMEM_MCP_MODE=bogus true || ironmem hook stop`
    /// runs in the default mode. Callers that report one result for several
    /// runs reduce them with [`best_result`].
    ///
    /// With no inline value the order is settings.json `env`, then the
    /// inherited environment, then the plugin wrapper's [`WRAPPER_DEFAULT`]
    /// (an empty value counts as unset to the wrapper, but `Config::load`
    /// rejects it), then [`McpAccessMode::UNSET_DEFAULT`]. A value the shell
    /// builds (`$`, a backtick) or, in exec form, one holding `${`, is
    /// [`ModeError::Unknown`]. The run's [`Program`] says what `hooks.rs`
    /// checks. The command-reading limits are listed on [`Self::runs_hook`].
    pub(super) fn hook_modes(&self, hook: &str, env: ModeEnv<'_>) -> Vec<RunMode> {
        self.invocations()
            .iter()
            .filter(|i| i.hook == hook)
            .map(|i| RunMode {
                program: i.program.clone(),
                form: i.form,
                ..invocation_mode(i, env)
            })
            .collect()
    }

    fn invocations(&self) -> Vec<Invocation> {
        match self {
            Self::Shell(command) => invocations(command, None, &[]),
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
    origin: Origin,
    /// The byte offset in `text` of each `$` that a shell expands. In a
    /// shell-form word that is a `$` outside single quotes with no backslash
    /// before it, any `$` in a `$( .. )` or backtick body (the body is not
    /// read), and a `$` that a shell which read the string earlier had
    /// already expanded: one that shell expanded in the word that held this
    /// `-c` string. No shell reads an exec-form element, so it has none.
    expanded: Vec<usize>,
}

/// Where a [`Word`] came from, with what only that kind of word has.
#[derive(Debug, Clone, Copy)]
enum Origin {
    /// A word of a shell-form command string.
    Shell {
        /// Contains an unquoted or double-quoted `$` or backtick: the shell
        /// decides its value, not us.
        dynamic: bool,
        /// The byte offset in `text` where the first quoted or
        /// backslash-escaped character went, if any. The shell reads a
        /// `NAME=value` word as an assignment only when nothing up to its
        /// first `=` was quoted.
        first_quoted: Option<usize>,
    },
    /// An element of an exec-form entry, which no shell reads: nothing in it
    /// is shell syntax.
    Exec {
        /// Contains `${`, a path placeholder Claude Code may substitute.
        placeholder: bool,
    },
}

impl Word {
    /// One element of an exec-form entry.
    fn exec(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            origin: Origin::Exec {
                placeholder: text.contains("${"),
            },
            expanded: Vec::new(),
        }
    }

    fn is_exec(&self) -> bool {
        matches!(self.origin, Origin::Exec { .. })
    }

    /// Its value is only known at run time: built by the shell, or holding
    /// a placeholder Claude Code may substitute.
    fn decided_at_run_time(&self) -> bool {
        match self.origin {
            Origin::Shell { dynamic, .. } => dynamic,
            Origin::Exec { placeholder } => placeholder,
        }
    }

    /// Where a run whose program is this word sits: exec form for an
    /// exec-form element, else shell form.
    fn form(&self) -> RunForm {
        if self.is_exec() {
            RunForm::Exec
        } else {
            RunForm::Shell
        }
    }

    /// True when the shell does not read the word as an assignment: a quote
    /// or backslash came before the first `=`, or there is no `=`. No shell
    /// reads an exec-form element at all.
    fn quoted_name(&self) -> bool {
        match self.origin {
            Origin::Exec { .. } => true,
            Origin::Shell { first_quoted, .. } => match (first_quoted, self.text.find('=')) {
                (None, _) => false,
                (Some(q), Some(eq)) => q <= eq,
                (Some(_), None) => true,
            },
        }
    }
}

/// Accumulates the words and simple commands of [`split_commands`].
#[derive(Default)]
struct Splitter<'a> {
    commands: Vec<Vec<Word>>,
    words: Vec<Word>,
    cur: String,
    in_word: bool,
    dynamic: bool,
    quoted: bool,
    /// See [`Origin::Shell`].
    first_quoted: Option<usize>,
    /// See [`Word::expanded`].
    expanded: Vec<usize>,
    /// The byte offset in the input of each `$` that a shell which read the
    /// string earlier had already expanded.
    expanded_before: &'a [usize],
    /// The next word is a redirection target and is dropped.
    skip_next: bool,
}

impl Splitter<'_> {
    fn end_word(&mut self) {
        let expanded = std::mem::take(&mut self.expanded);
        if self.in_word {
            let word = Word {
                text: std::mem::take(&mut self.cur),
                origin: Origin::Shell {
                    dynamic: self.dynamic,
                    first_quoted: self.first_quoted,
                },
                expanded,
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

    /// Push `c`, which this shell does not expand (it is in single quotes or
    /// after a backslash), read at byte `at` of the input. An earlier shell
    /// may still have expanded it.
    fn push_literal(&mut self, at: usize, c: char) {
        if c == '$' && self.expanded_before.contains(&at) {
            self.expanded.push(self.cur.len());
        }
        self.cur.push(c);
    }

    /// Push `c`, which this shell expands when it is a `$`.
    fn push_expanding(&mut self, c: char) {
        if c == '$' {
            self.expanded.push(self.cur.len());
        }
        self.cur.push(c);
    }

    /// Count every `$` in `cur` from byte `from` on as expanded: the body of
    /// a substitution, which is not read.
    fn expand_all_from(&mut self, from: usize) {
        let body = self.cur[from..].match_indices('$');
        self.expanded.extend(body.map(|(at, _)| from + at));
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
/// quotes group (a backslash escapes `$`, a backtick, `"`, `\` or a newline
/// there, and is kept before anything else), unquoted whitespace
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
fn split_commands(input: &str, expanded_before: &[usize]) -> Vec<Vec<Word>> {
    let mut sp = Splitter {
        expanded_before,
        ..Splitter::default()
    };
    let mut chars = input.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        match c {
            '\'' => {
                sp.in_word = true;
                sp.quoted = true;
                sp.mark_quoted();
                for (at, q) in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    sp.push_literal(at, q);
                }
            }
            '"' => {
                sp.in_word = true;
                sp.quoted = true;
                sp.mark_quoted();
                while let Some((_, q)) = chars.next() {
                    match q {
                        '"' => break,
                        // POSIX: inside double quotes a backslash escapes
                        // only `$`, a backtick, `"`, `\` and a newline (a
                        // line continuation, removed); before anything else
                        // it stays.
                        '\\' => match chars.peek() {
                            Some(&(at, n @ ('$' | '`' | '"' | '\\'))) => {
                                chars.next();
                                sp.push_literal(at, n);
                            }
                            Some((_, '\n')) => {
                                chars.next();
                            }
                            _ => sp.cur.push('\\'),
                        },
                        '$' | '`' => {
                            sp.dynamic = true;
                            sp.push_expanding(q);
                        }
                        _ => sp.cur.push(q),
                    }
                }
            }
            '\\' => match chars.next() {
                Some((_, '\n')) | None => {}
                Some((at, n)) => {
                    sp.in_word = true;
                    sp.mark_quoted();
                    sp.push_literal(at, n);
                }
            },
            '$' | '`' => {
                sp.in_word = true;
                sp.dynamic = true;
                sp.push_expanding(c);
                let body = sp.cur.len();
                if c == '`' {
                    read_backticks(&mut chars, &mut sp.cur);
                } else if chars.peek().is_some_and(|&(_, n)| n == '(') {
                    read_parens(&mut chars, &mut sp.cur);
                }
                sp.expand_all_from(body);
            }
            '#' if !sp.in_word => {
                for (_, n) in chars.by_ref() {
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
                if chars
                    .peek()
                    .is_some_and(|&(_, n)| n == c || (c == '>' && n == '&'))
                {
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

/// The characters of a command string, each with its byte offset.
type Chars<'a> = std::iter::Peekable<std::str::CharIndices<'a>>;

/// Copy a backtick substitution body and its closing backtick into `cur`.
fn read_backticks(chars: &mut Chars<'_>, cur: &mut String) {
    while let Some((_, c)) = chars.next() {
        cur.push(c);
        match c {
            '`' => return,
            '\\' => cur.extend(chars.next().map(|(_, n)| n)),
            _ => {}
        }
    }
}

/// Copy a `( .. )` span (the `$` is already in `cur`) into `cur`, up to the
/// paren that closes the first one. Quoted parens don't count.
fn read_parens(chars: &mut Chars<'_>, cur: &mut String) {
    let mut depth = 0usize;
    while let Some((_, c)) = chars.next() {
        cur.push(c);
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return;
                }
            }
            '\\' => cur.extend(chars.next().map(|(_, n)| n)),
            '\'' | '"' => {
                while let Some((_, q)) = chars.next() {
                    cur.push(q);
                    if q == c {
                        break;
                    }
                    if q == '\\' && c == '"' {
                        cur.extend(chars.next().map(|(_, n)| n));
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
            dynamic: word.decided_at_run_time(),
        }),
        _ => None,
    }
}

/// What a run's environment holds for `IRONMEM_MCP_MODE`, as its own command
/// or the earlier commands of its string set it.
#[derive(Debug, Clone)]
enum ModeSet {
    /// This value.
    Value(ModeValue),
    /// This value when the variable was in the environment the shell
    /// started with, so the shell exports it; otherwise none. A plain
    /// `IRONMEM_MCP_MODE=v` command leaves this.
    IfInherited(ModeValue),
    /// No value, whatever the shell inherited (`unset IRONMEM_MCP_MODE`).
    Unset,
}

/// `IRONMEM_MCP_MODE`'s value in one shell's own variables.
#[derive(Debug, Clone)]
enum VarValue {
    /// Untouched: whatever the shell inherited.
    Inherited,
    Unset,
    Set(ModeValue),
}

/// Whether the shell exports `IRONMEM_MCP_MODE` to the programs it runs.
#[derive(Debug, Clone, Copy)]
enum Exported {
    /// Exactly when it was inherited (the shell exports what it inherits).
    AsInherited,
    Yes,
    No,
}

/// `IRONMEM_MCP_MODE` as the simple commands of one shell-form string have
/// left it so far.
#[derive(Debug, Clone)]
struct ShellVar {
    value: VarValue,
    exported: Exported,
}

impl ShellVar {
    /// The state a shell starts in when its parent's environment holds
    /// `inherited` (`None`: whatever doctor's [`ModeEnv`] says).
    fn starting_with(inherited: Option<ModeSet>) -> Self {
        let (value, exported) = match inherited {
            None => (VarValue::Inherited, Exported::AsInherited),
            Some(ModeSet::Value(v)) => (VarValue::Set(v), Exported::Yes),
            Some(ModeSet::IfInherited(v)) => (VarValue::Set(v), Exported::AsInherited),
            Some(ModeSet::Unset) => (VarValue::Unset, Exported::No),
        };
        Self { value, exported }
    }

    /// What the programs the shell runs next get (`None`: as inherited).
    fn seen(&self) -> Option<ModeSet> {
        match (&self.value, self.exported) {
            (VarValue::Unset, _) | (_, Exported::No) => Some(ModeSet::Unset),
            (VarValue::Inherited, _) => None,
            (VarValue::Set(v), Exported::Yes) => Some(ModeSet::Value(v.clone())),
            (VarValue::Set(v), Exported::AsInherited) => Some(ModeSet::IfInherited(v.clone())),
        }
    }

    fn apply(&mut self, change: VarChange) {
        match change {
            VarChange::Assign(v) => self.value = VarValue::Set(v),
            VarChange::Export(value) => {
                if let Some(v) = value {
                    self.value = VarValue::Set(v);
                }
                self.exported = Exported::Yes;
            }
            // `export -n` drops the export flag and keeps the value, which
            // a later bare `export` sends again.
            VarChange::Unexport(value) => {
                if let Some(v) = value {
                    self.value = VarValue::Set(v);
                }
                self.exported = Exported::No;
            }
            // `unset` drops the variable and its export flag.
            VarChange::Unset => {
                self.value = VarValue::Unset;
                self.exported = Exported::No;
            }
        }
    }
}

/// What one simple command does to `IRONMEM_MCP_MODE` for the commands
/// after it.
#[derive(Debug, Clone)]
enum VarChange {
    /// `IRONMEM_MCP_MODE=v` with no program.
    Assign(ModeValue),
    /// `export IRONMEM_MCP_MODE=v`, or a bare `export IRONMEM_MCP_MODE`.
    Export(Option<ModeValue>),
    /// `export -n IRONMEM_MCP_MODE=v`, or a bare `export -n IRONMEM_MCP_MODE`.
    Unexport(Option<ModeValue>),
    /// `unset IRONMEM_MCP_MODE`.
    Unset,
}

/// One run of `ironmem hook <hook>` found in a command string.
#[derive(Debug, Clone)]
struct Invocation {
    hook: String,
    /// Goes through `ironmem-mcp.sh` or `ironmem-hook.sh`, which default the
    /// mode to trusted (`${IRONMEM_MCP_MODE:-trusted}`).
    wrapper: bool,
    /// The mode its command or the string before it set (`None`: nothing
    /// did, so it is as inherited).
    mode: Option<ModeSet>,
    /// See [`RunMode::program`].
    program: Program,
    /// See [`RunMode::form`].
    form: RunForm,
}

/// The name of the variable Claude Code sets to a plugin's directory.
const PLUGIN_ROOT_VAR: &str = "CLAUDE_PLUGIN_ROOT";

/// True when `word` uses `$CLAUDE_PLUGIN_ROOT` or `${CLAUDE_PLUGIN_ROOT...}`
/// (the name not running on into a longer one) in a way that does not run a
/// path when the variable is unset, as scripts/install-ironmem.sh reads it
/// (`re_plugin_root($dq)` for a word a shell reads, `plugin_root` for an
/// exec-form element).
///
/// A use whose `$` a shell expands ([`Word::expanded`]) does not count when
/// it is braced with a default that is not empty (`:-`, `-`, `:=` or `=`):
/// measured with /bin/sh, bash and dash, the variable unset, the shell runs
/// the default's path. Any other expansion of an unset variable (`%`, `#`,
/// `:+`, an empty default) is still empty. A use whose `$` no shell expands
/// counts, default or not, because the path then holds the text of the use:
/// one in single quotes or after a backslash, each use on its own (so
/// `'${CLAUDE_PLUGIN_ROOT:-/x}'"$HOME"/ironmem` counts), and every use in an
/// exec-form element; what Claude Code does with a default in one of those
/// was not measured.
fn uses_plugin_root(word: &Word) -> bool {
    let text = word.text.as_str();
    text.match_indices('$').any(|(at, _)| {
        let rest = &text[at + 1..];
        let (braced, rest) = match rest.strip_prefix('{') {
            Some(rest) => (true, rest),
            None => (false, rest),
        };
        let Some(after) = rest.strip_prefix(PLUGIN_ROOT_VAR) else {
            return false;
        };
        if after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
        !(braced && word.expanded.contains(&at) && starts_with_a_default(after))
    })
}

/// True when `after`, the text that follows a variable's name inside `${`,
/// is a default operator (`:-`, `-`, `:=` or `=`) and a default that is not
/// empty.
fn starts_with_a_default(after: &str) -> bool {
    after
        .strip_prefix(':')
        .unwrap_or(after)
        .strip_prefix(['-', '='])
        .is_some_and(|default| !default.is_empty() && !default.starts_with('}'))
}

/// The program word's literal path, if doctor can check it: see
/// [`Program::Literal`].
fn literal_path(word: &Word) -> Option<String> {
    let text = word.text.as_str();
    if word.decided_at_run_time() || text.contains(['*', '?', '[', '{']) {
        return None;
    }
    // The shell expands `~/` only when neither character is quoted; no shell
    // reads an exec-form element (see [`Program::ExecTilde`]).
    let tilde = match word.origin {
        Origin::Shell { first_quoted, .. } => {
            text.starts_with("~/") && !matches!(first_quoted, Some(0 | 1))
        }
        Origin::Exec { .. } => false,
    };
    (text.starts_with('/') || tilde).then(|| text.to_owned())
}

/// What doctor can tell about a run's program from its program word.
fn program_of(word: &Word) -> Program {
    if uses_plugin_root(word) {
        Program::PluginRoot(word.text.clone())
    } else if word.is_exec() && word.text.starts_with("~/") {
        Program::ExecTilde(word.text.clone())
    } else {
        literal_path(word).map_or(Program::Unchecked, Program::Literal)
    }
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

/// The `ironmem hook` runs in shell-form `command`, run by a shell whose
/// environment holds `inherited`; see [`HookCommand::runs_hook`] and
/// [`HookCommand::hook_modes`] for the model. `expanded_before` is the byte
/// offset in `command` of each `$` that a shell which read the string
/// earlier had already expanded ([`Word::expanded`] of the word it came
/// from).
fn invocations(
    command: &str,
    inherited: Option<ModeSet>,
    expanded_before: &[usize],
) -> Vec<Invocation> {
    let mut var = ShellVar::starting_with(inherited);
    let mut found = Vec::new();
    for words in split_commands(command, expanded_before) {
        let mut inline = None;
        let mut rest: &[Word] = &words;
        take_assignments(&mut rest, &mut inline, false);
        let change = match (rest.is_empty(), inline) {
            (true, Some(value)) => Some(VarChange::Assign(value)),
            (_, inline) => simple_command(rest, inline, var.seen().as_ref(), &mut found),
        };
        if let Some(change) = change {
            var.apply(change);
        }
    }
    found
}

/// Read one simple command, `words`, whose leading assignments (shell form)
/// are already taken into `inline`: peel the `env`/`exec`/`command`/`nohup`
/// wrappers, then push the hook runs it starts to `found`. `seen` is what
/// the earlier commands of the string left for it. Returns what an `export`
/// or `unset` of `IRONMEM_MCP_MODE` does, for the simple commands after it.
fn simple_command(
    words: &[Word],
    mut inline: Option<ModeValue>,
    seen: Option<&ModeSet>,
    found: &mut Vec<Invocation>,
) -> Option<VarChange> {
    let mut rest = words;
    // The last wrapper peeled runs its argument as a program (execvp), as
    // `env`, `nohup` and the `exec` builtin do; `command` runs builtins too.
    let mut after_execvp = false;
    while let Some(first) = rest.first() {
        let name = basename(&first.text);
        // `exec` is a shell builtin with no program behind it, so an
        // argument vector that names it as a program (directly or after
        // `env`), or a shell command that hands it to `env`, `nohup` or
        // `exec`, never starts; it is left as the program, which is no hook.
        let exec_cannot_start = name == "exec" && (first.is_exec() || after_execvp);
        if !matches!(name, "env" | "exec" | "command" | "nohup") || exec_cannot_start {
            break;
        }
        after_execvp = name != "command";
        rest = &rest[1..];
        if name == "env" {
            take_assignments(&mut rest, &mut inline, true);
        }
    }
    let (program_word, args) = rest.split_first()?;
    let program = basename(&program_word.text);
    match program {
        "export" => {
            // The option words come first. `-f` makes every name a function,
            // which leaves the variable as it was, and `-n` takes the export
            // away (both measured with /bin/sh and bash).
            let flags = || {
                args.iter()
                    .map(|w| w.text.as_str())
                    .take_while(|text| text.starts_with('-') && *text != "--")
                    .flat_map(|text| text.chars().skip(1))
            };
            if flags().any(|c| c == 'f') {
                return None;
            }
            let unexports = flags().any(|c| c == 'n');
            return args.iter().rev().find_map(|w| {
                let value = mode_assignment(w, false);
                if value.is_none() && w.text != MODE_VAR {
                    return None;
                }
                Some(if unexports {
                    VarChange::Unexport(value)
                } else {
                    VarChange::Export(value)
                })
            });
        }
        // `unset -f` removes functions, not variables.
        "unset" => {
            let names_it = args.iter().any(|w| w.text == MODE_VAR);
            let functions = args.iter().any(|w| w.text == "-f");
            return (names_it && !functions).then_some(VarChange::Unset);
        }
        _ => {}
    }
    let mode = inline.map(ModeSet::Value).or_else(|| seen.cloned());
    let mut push = |hook: &str, wrapper: bool| {
        found.push(Invocation {
            hook: hook.to_owned(),
            wrapper,
            mode: mode.clone(),
            program: program_of(program_word),
            form: program_word.form(),
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
fn shell_args(args: &[Word], mode: Option<ModeSet>, found: &mut Vec<Invocation>) {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let text = arg.text.as_str();
        // bash reads any single-dash option word containing `c` (`-lc`,
        // `-ce`) as `-c`.
        if text.starts_with('-') && !text.starts_with("--") && text.contains('c') {
            if let Some(script) = args.get(i + 1) {
                let runs = invocations(&script.text, mode, &script.expanded);
                // A string from an exec-form entry's args: a fix edits it.
                found.extend(runs.into_iter().map(|mut run| {
                    if script.is_exec() {
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
            // is not checked, except for a `${CLAUDE_PLUGIN_ROOT}` in it.
            let program = if uses_plugin_root(arg) {
                Program::PluginRoot(text.to_owned())
            } else {
                Program::Unchecked
            };
            found.push(Invocation {
                hook: hook.to_owned(),
                wrapper: true,
                mode,
                program,
                form: arg.form(),
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
    // settings.json `env` and the environment both reach the hook's shell
    // as inherited variables.
    let inherited = env.settings.is_some() || env.process.is_some();
    let (own, env) = match &run.mode {
        Some(ModeSet::Value(v)) => (Some(v), env),
        Some(ModeSet::IfInherited(v)) if inherited => (Some(v), env),
        Some(ModeSet::IfInherited(_)) | None => (None, env),
        Some(ModeSet::Unset) => (None, ModeEnv::default()),
    };
    let (raw, dynamic, source) = match (own, env.settings, env.process) {
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
        program: Program::Unchecked,
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
        assert!(!runs_hook("true # ironmem hook stop", "stop"));
        assert!(!runs_hook("printf '%s' 'ironmem hook stop'", "stop"));
        assert!(runs_hook("ironmem hook stop>>/tmp/log", "stop"));
        assert!(runs_hook("ironmem hook stop >/dev/null 2>&1", "stop"));
        assert!(runs_hook("ironmem hook stop&&true", "stop"));
        assert!(
            runs_hook("true || ironmem hook stop", "stop"),
            "documented limit"
        );
        assert!(runs_hook("nohup ironmem hook stop", "stop"));
        assert!(runs_hook("exec ironmem hook stop", "stop"));
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
    /// The program of the one `stop` run in shell-form `command`.
    fn program(command: &str) -> Program {
        let runs = HookCommand::Shell(command.to_owned()).hook_modes("stop", ModeEnv::default());
        assert_eq!(runs.len(), 1, "{command}");
        runs[0].program.clone()
    }

    #[test]
    fn a_backslash_in_double_quotes_escapes_only_what_posix_says() {
        // `sh -c 'printf "%s\n" "/opt/a\b/ironmem"'` prints the backslash.
        assert_eq!(
            program(r#""/opt/a\b/ironmem" hook stop"#),
            Program::Literal(r"/opt/a\b/ironmem".to_owned())
        );
        // `\$` is a literal dollar, not a shell-built value.
        assert_eq!(
            program(r#""/opt/a\$b/ironmem" hook stop"#),
            Program::Literal("/opt/a$b/ironmem".to_owned())
        );
        assert_eq!(
            program(r#""/opt/a\"b\\c/ironmem" hook stop"#),
            Program::Literal(r#"/opt/a"b\c/ironmem"#.to_owned())
        );
        // A backslash-newline inside double quotes is a line continuation.
        assert_eq!(
            program("\"/opt/a\\\nb/ironmem\" hook stop"),
            Program::Literal("/opt/ab/ironmem".to_owned())
        );
    }

    #[test]
    fn unset_drops_an_inherited_mode() {
        use McpAccessMode::*;
        let cmd = "unset IRONMEM_MCP_MODE; ironmem hook stop";
        assert_eq!(
            hook_mode(cmd, "stop", shell("trusted")),
            (Ok(ReadOnly), ModeSource::Default)
        );
        let settings = ModeEnv {
            settings: Some("trusted"),
            process: None,
        };
        assert_eq!(
            hook_mode(cmd, "stop", settings),
            (Ok(ReadOnly), ModeSource::Default)
        );
        // The plugin wrapper still defaults an unset mode to trusted.
        assert_eq!(
            hook_mode(
                "unset IRONMEM_MCP_MODE; bash /p/hooks/ironmem-hook.sh stop",
                "stop",
                shell("read-only")
            ),
            (Ok(Trusted), ModeSource::PluginWrapper)
        );
        // `unset -f` removes a function, and an assignment on the run wins.
        assert_eq!(
            hook_mode(
                "unset -f IRONMEM_MCP_MODE; ironmem hook stop",
                "stop",
                shell("trusted")
            ),
            (Ok(Trusted), ModeSource::Environment)
        );
        assert_eq!(
            hook_mode(
                "unset IRONMEM_MCP_MODE; IRONMEM_MCP_MODE=trusted ironmem hook stop",
                "stop",
                shell("read-only")
            ),
            (Ok(Trusted), ModeSource::Inline)
        );
    }

    #[test]
    fn a_standalone_assignment_reaches_the_run_only_when_inherited_or_exported() {
        use McpAccessMode::*;
        // Real shell: `IRONMEM_MCP_MODE=trusted sh -c 'IRONMEM_MCP_MODE=read-only; <child>'`
        // gives the child read-only; with nothing inherited, the child sees none.
        let cmd = "IRONMEM_MCP_MODE=read-only; ironmem hook stop";
        assert_eq!(
            hook_mode(cmd, "stop", shell("trusted")),
            (Ok(ReadOnly), ModeSource::Inline)
        );
        let trusted = "IRONMEM_MCP_MODE=trusted; ironmem hook stop";
        assert_eq!(
            hook_mode(trusted, "stop", ModeEnv::default()),
            (Ok(ReadOnly), ModeSource::Default)
        );
        // A bare export exports the shell variable.
        let exported = "IRONMEM_MCP_MODE=trusted; export IRONMEM_MCP_MODE; ironmem hook stop";
        assert_eq!(
            hook_mode(exported, "stop", ModeEnv::default()),
            (Ok(Trusted), ModeSource::Inline)
        );
        // After an export, a plain assignment changes what is exported.
        let reassigned =
            "export IRONMEM_MCP_MODE=read-only; IRONMEM_MCP_MODE=trusted; ironmem hook stop";
        assert_eq!(
            hook_mode(reassigned, "stop", ModeEnv::default()),
            (Ok(Trusted), ModeSource::Inline)
        );
        // `unset` drops the export flag; `export` before the assignment
        // sets it again (both measured with /bin/sh).
        let after_unset = "unset IRONMEM_MCP_MODE; IRONMEM_MCP_MODE=trusted; ironmem hook stop";
        assert_eq!(
            hook_mode(after_unset, "stop", shell("trusted")),
            (Ok(ReadOnly), ModeSource::Default)
        );
        let export_first =
            "unset IRONMEM_MCP_MODE; export IRONMEM_MCP_MODE; IRONMEM_MCP_MODE=trusted; \
             ironmem hook stop";
        assert_eq!(
            hook_mode(export_first, "stop", ModeEnv::default()),
            (Ok(Trusted), ModeSource::Inline)
        );
        // A bare export with nothing assigned leaves the inherited value.
        let bare = "export IRONMEM_MCP_MODE; ironmem hook stop";
        assert_eq!(
            hook_mode(bare, "stop", shell("trusted")),
            (Ok(Trusted), ModeSource::Environment)
        );
    }

    #[test]
    fn export_n_takes_the_export_away_and_keeps_the_value() {
        use McpAccessMode::*;
        // Each case measured with /bin/sh and bash on macOS.
        // `export -n NAME`: programs run after it get no value.
        assert_eq!(
            hook_mode(
                "export -n IRONMEM_MCP_MODE; ironmem hook stop",
                "stop",
                shell("trusted")
            ),
            (Ok(ReadOnly), ModeSource::Default)
        );
        // The shell keeps the value, so a later bare export sends it again.
        assert_eq!(
            hook_mode(
                "export -n IRONMEM_MCP_MODE; export IRONMEM_MCP_MODE; ironmem hook stop",
                "stop",
                shell("trusted")
            ),
            (Ok(Trusted), ModeSource::Environment)
        );
        // `export -n NAME=value` sets the shell's value and does not export it.
        let assigned = "export -n IRONMEM_MCP_MODE=trusted; ironmem hook stop";
        assert_eq!(
            hook_mode(assigned, "stop", ModeEnv::default()),
            (Ok(ReadOnly), ModeSource::Default)
        );
        assert_eq!(
            hook_mode(assigned, "stop", shell("trusted")),
            (Ok(ReadOnly), ModeSource::Default)
        );
        assert_eq!(
            hook_mode(
                "export -n IRONMEM_MCP_MODE=trusted; export IRONMEM_MCP_MODE; ironmem hook stop",
                "stop",
                shell("read-only")
            ),
            (Ok(Trusted), ModeSource::Inline)
        );
        // An assignment on the run itself still reaches it.
        assert_eq!(
            hook_mode(
                "export -n IRONMEM_MCP_MODE; IRONMEM_MCP_MODE=trusted ironmem hook stop",
                "stop",
                shell("read-only")
            ),
            (Ok(Trusted), ModeSource::Inline)
        );
        // A bash -c string starts from what its parent shell exports.
        assert_eq!(
            hook_mode(
                "export -n IRONMEM_MCP_MODE; bash -c 'ironmem hook stop'",
                "stop",
                shell("trusted")
            ),
            (Ok(ReadOnly), ModeSource::Default)
        );
    }

    #[test]
    fn export_f_names_a_function_not_the_variable() {
        use McpAccessMode::*;
        // `export -f NAME` exports a function: the variable stays unexported.
        assert_eq!(
            hook_mode(
                "IRONMEM_MCP_MODE=trusted; export -f IRONMEM_MCP_MODE; ironmem hook stop",
                "stop",
                ModeEnv::default()
            ),
            (Ok(ReadOnly), ModeSource::Default)
        );
        // `export -fn NAME` un-exports a function: the variable stays exported.
        assert_eq!(
            hook_mode(
                "export -fn IRONMEM_MCP_MODE; ironmem hook stop",
                "stop",
                shell("trusted")
            ),
            (Ok(Trusted), ModeSource::Environment)
        );
        assert_eq!(
            hook_mode(
                "export -nf IRONMEM_MCP_MODE; ironmem hook stop",
                "stop",
                shell("trusted")
            ),
            (Ok(Trusted), ModeSource::Environment)
        );
    }

    #[test]
    fn a_bash_c_string_inherits_what_its_parent_string_set() {
        use McpAccessMode::*;
        let cmd = "IRONMEM_MCP_MODE=trusted; bash -c 'ironmem hook stop'";
        assert_eq!(
            hook_mode(cmd, "stop", shell("read-only")),
            (Ok(Trusted), ModeSource::Inline)
        );
        assert_eq!(
            hook_mode(cmd, "stop", ModeEnv::default()),
            (Ok(ReadOnly), ModeSource::Default)
        );
        let unset = "unset IRONMEM_MCP_MODE; bash -c 'ironmem hook stop'";
        assert_eq!(
            hook_mode(unset, "stop", shell("trusted")),
            (Ok(ReadOnly), ModeSource::Default)
        );
    }

    #[test]
    fn a_plugin_root_program_or_script_word_is_marked() {
        let root = |w: &str| Program::PluginRoot(w.to_owned());
        assert_eq!(
            program("bash ${CLAUDE_PLUGIN_ROOT}/hooks/ironmem-hook.sh stop"),
            root("${CLAUDE_PLUGIN_ROOT}/hooks/ironmem-hook.sh")
        );
        assert_eq!(
            program(r#"bash "${CLAUDE_PLUGIN_ROOT}/hooks/ironmem-hook.sh" stop"#),
            root("${CLAUDE_PLUGIN_ROOT}/hooks/ironmem-hook.sh")
        );
        assert_eq!(
            program("$CLAUDE_PLUGIN_ROOT/hooks/ironmem-hook.sh stop"),
            root("$CLAUDE_PLUGIN_ROOT/hooks/ironmem-hook.sh")
        );
        assert_eq!(
            program("${CLAUDE_PLUGIN_ROOT}/bin/ironmem-mcp.sh hook stop"),
            root("${CLAUDE_PLUGIN_ROOT}/bin/ironmem-mcp.sh")
        );
        assert_eq!(
            program("bash -c '${CLAUDE_PLUGIN_ROOT}/bin/ironmem-mcp.sh hook stop'"),
            root("${CLAUDE_PLUGIN_ROOT}/bin/ironmem-mcp.sh")
        );
        let exec = HookCommand::Exec {
            program: "bash".to_owned(),
            args: vec![
                "${CLAUDE_PLUGIN_ROOT}/hooks/ironmem-hook.sh".into(),
                "stop".into(),
            ],
        };
        assert_eq!(
            exec.hook_modes("stop", ModeEnv::default())[0].program,
            root("${CLAUDE_PLUGIN_ROOT}/hooks/ironmem-hook.sh")
        );
        // A longer variable name is another variable, and a plain script is
        // not checked.
        assert_eq!(
            program("bash ${CLAUDE_PLUGIN_ROOTS}/hooks/ironmem-hook.sh stop"),
            Program::Unchecked
        );
        assert_eq!(
            program("bash /p/hooks/ironmem-hook.sh stop"),
            Program::Unchecked
        );
        // `text` as the one word of a shell-form command, or as an
        // exec-form element.
        let uses = |text: &str, by_shell: bool| {
            let word = if by_shell {
                split_commands(text, &[]).remove(0).remove(0)
            } else {
                Word::exec(text)
            };
            assert_eq!(word.text, text);
            uses_plugin_root(&word)
        };
        for by_shell in [true, false] {
            assert!(!uses("CLAUDE_PLUGIN_ROOT/hooks", by_shell));
            assert!(!uses("${CLAUDE_PLUGIN_ROOTS:-/x}/hooks", by_shell));
            assert!(uses("${CLAUDE_PLUGIN_ROOT:+/x}/hooks", by_shell));
        }
        assert!(!uses("${CLAUDE_PLUGIN_ROOT:-/x}/hooks", true));
        assert!(uses("${CLAUDE_PLUGIN_ROOT:-/x}/hooks", false));
    }

    #[test]
    fn a_default_the_shell_applies_is_not_a_plugin_root_run() {
        let root = |w: &str| Program::PluginRoot(w.to_owned());
        // Measured with /bin/sh and bash, the variable unset: each of these
        // runs the default's path, which the shell builds, so doctor does
        // not check it.
        for defaulted in [
            r#""${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem" hook stop"#,
            "${CLAUDE_PLUGIN_ROOT-/x}/bin/ironmem hook stop",
            "bash ${CLAUDE_PLUGIN_ROOT:=/x}/hooks/ironmem-hook.sh stop",
            "bash -c '${CLAUDE_PLUGIN_ROOT=/x}/bin/ironmem-mcp.sh hook stop'",
            "${CLAUDE_PLUGIN_ROOT:-$HOME/.ironrace}/bin/ironmem hook stop",
        ] {
            assert_eq!(program(defaulted), Program::Unchecked, "{defaulted}");
        }
        // An empty default, any other expansion, or a second use with no
        // default still leaves nothing where the variable was.
        for (command, word) in [
            (
                "${CLAUDE_PLUGIN_ROOT:-}/bin/ironmem hook stop",
                "${CLAUDE_PLUGIN_ROOT:-}/bin/ironmem",
            ),
            (
                "${CLAUDE_PLUGIN_ROOT-}/bin/ironmem hook stop",
                "${CLAUDE_PLUGIN_ROOT-}/bin/ironmem",
            ),
            (
                "${CLAUDE_PLUGIN_ROOT%/}/bin/ironmem hook stop",
                "${CLAUDE_PLUGIN_ROOT%/}/bin/ironmem",
            ),
            (
                "${CLAUDE_PLUGIN_ROOT:+/x}/bin/ironmem hook stop",
                "${CLAUDE_PLUGIN_ROOT:+/x}/bin/ironmem",
            ),
            (
                "${CLAUDE_PLUGIN_ROOT:-/x}/${CLAUDE_PLUGIN_ROOT}/ironmem hook stop",
                "${CLAUDE_PLUGIN_ROOT:-/x}/${CLAUDE_PLUGIN_ROOT}/ironmem",
            ),
            (
                "$CLAUDE_PLUGIN_ROOT:-/x/bin/ironmem hook stop",
                "$CLAUDE_PLUGIN_ROOT:-/x/bin/ironmem",
            ),
        ] {
            assert_eq!(program(command), root(word), "{command}");
        }
        // No shell reads an exec-form word, so nothing applies the default.
        let exec = HookCommand::Exec {
            program: "${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem".to_owned(),
            args: vec!["hook".into(), "stop".into()],
        };
        assert_eq!(
            exec.hook_modes("stop", ModeEnv::default())[0].program,
            root("${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem")
        );
        let exec_script = HookCommand::Exec {
            program: "bash".to_owned(),
            args: vec![
                "${CLAUDE_PLUGIN_ROOT:-/x}/hooks/ironmem-hook.sh".into(),
                "stop".into(),
            ],
        };
        assert_eq!(
            exec_script.hook_modes("stop", ModeEnv::default())[0].program,
            root("${CLAUDE_PLUGIN_ROOT:-/x}/hooks/ironmem-hook.sh")
        );
        // bash -c reads its string, exec form or not.
        let exec_string = HookCommand::Exec {
            program: "bash".to_owned(),
            args: vec![
                "-c".into(),
                "${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem hook stop".into(),
            ],
        };
        assert_eq!(
            exec_string.hook_modes("stop", ModeEnv::default())[0].program,
            Program::Unchecked
        );
    }

    #[test]
    fn a_default_counts_only_where_a_shell_expands_its_use() {
        let root = |w: &str| Program::PluginRoot(w.to_owned());
        // Measured with /bin/sh, bash and dash, the variable unset: none of
        // these runs the default's path, because no shell expands the use.
        for (command, word) in [
            (
                "'${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem' hook stop",
                "${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem",
            ),
            (
                r"\${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem hook stop",
                "${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem",
            ),
            (
                r#""\${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem" hook stop"#,
                "${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem",
            ),
            // Another `$` in the word, which the shell does expand, does
            // not make this use an expanded one.
            (
                r#"'${CLAUDE_PLUGIN_ROOT:-/x}'"$HOME"/bin/ironmem hook stop"#,
                "${CLAUDE_PLUGIN_ROOT:-/x}$HOME/bin/ironmem",
            ),
            (
                r"bash -c '\${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem hook stop'",
                "${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem",
            ),
            // In a double-quoted -c string the outer shell leaves a `\$` as
            // `$` for the inner one, which does not expand it in single
            // quotes or after a backslash of its own.
            (
                r#"bash -c "'\${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem' hook stop""#,
                "${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem",
            ),
            (
                r#"bash -c "\\\${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem hook stop""#,
                "${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem",
            ),
        ] {
            assert_eq!(program(command), root(word), "{command}");
        }
        // And each of these does run it: some shell expands the use.
        for defaulted in [
            r#""${CLAUDE_PLUGIN_ROOT:-/x}"'/a$b'/bin/ironmem hook stop"#,
            r#"bash -c "\${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem hook stop""#,
            r#"bash -c "true; \${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem hook stop""#,
            // The outer shell expands it before the inner one reads the
            // single quotes.
            r#"bash -c "'${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem' hook stop""#,
            r#"sh -c "bash -c \"'${CLAUDE_PLUGIN_ROOT:-/x}/bin/ironmem' hook stop\"""#,
        ] {
            assert_eq!(program(defaulted), Program::Unchecked, "{defaulted}");
        }
    }
}

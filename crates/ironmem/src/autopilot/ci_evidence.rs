//! CI evidence — what a repo's own CI says it enforces.
//!
//! [`super::onboard`] infers gate commands from build manifests, which is
//! enough to name a stack's *test* command but says nothing about the format
//! and lint checks a repo is actually judged by. The first live Autopilot run
//! found the cost of that gap: an autopilot branch met its approved gate
//! (`cargo test --workspace`) and then failed CI twice on `cargo fmt` and
//! `cargo clippy`, which the gate never mentioned. The IC could not have
//! known — by design it is told the gate condition and nothing else.
//!
//! # The gate is a proxy for CI, so prefer CI's own command
//!
//! An invocation this module reads becomes a gate command only when it is
//! **plainly runnable** as written ([`Invocation::is_plainly_runnable`]) *and*
//! **adoptable** ([`Invocation::adoptable`]) — nothing about the step qualifies
//! how it runs. Such a command is the faithful thing to propose: CI runs it on
//! every merge to the default branch, so it is satisfiable by construction, and
//! matching it exactly is the whole point (a gate stricter than CI blocks work
//! CI would have accepted; a gate looser than CI is the defect above).
//!
//! Everything else the caller falls back to its own canonical command for, and
//! says so on the proposal. The two failure modes this module must never cause
//! are a gate that **cannot pass** and a gate that **enforces nothing**, so
//! every ambiguity resolves toward "tell the human" rather than toward a guess.
//!
//! # Bounded reader, not a YAML parser
//!
//! This finds command text and stops: no anchors, no matrices, no job graph, no
//! `env`/expression resolution, no `on:` triggers, no `if:` conditions, and no
//! knowledge of which jobs are *required*. So a check that runs only on a
//! schedule, or only under some condition, reads exactly like one that runs on
//! every merge — the same blind spot in both directions, and the reason a
//! proposal is something a human approves rather than something that takes
//! effect. It does read three facts that decide whether a command
//! means what it appears to mean — `working-directory:`, `continue-on-error:`
//! (on a step, on a job, or under `defaults: run:`), and the `||` a step uses
//! to make a check advisory — because ignoring those is how a command CI
//! tolerates failing, or runs somewhere else entirely, becomes a hard gate at
//! the repo root.
//!
//! A tool enforced only through a third-party action (`uses:
//! actions-rs/clippy-check@v1`) or behind an indirection (`make lint`, `cargo
//! xtask ci`) is invisible here, and such a repo onboards exactly as it did
//! before this module existed. The human approval step is the backstop the
//! spec's own risk table names for a wrongly inferred gate command.
//!
//! # Scope: GitHub Actions
//!
//! Only `.github/workflows/*.yml`/`*.yaml`. A repo whose CI lives in GitLab,
//! CircleCI, Jenkins or elsewhere reads as *no evidence*, which degrades to
//! the inference that existed before this module — never to a gate the repo
//! cannot pass. A human can still supply commands directly via
//! `gate_config::propose_gate_config`.

use std::path::{Path, PathBuf};

/// Generous cap on a workflow file's size, mirroring `onboard`'s manifest
/// cap: a real workflow is a few KB, and `read_to_string` has no bound of
/// its own.
const MAX_WORKFLOW_BYTES: u64 = 1024 * 1024;

/// Characters a command may contain and still be run as written. Everything
/// outside this set — `$`, quotes, braces, backslashes, redirections, globs
/// — means the text depends on a shell or a workflow runner to become the
/// command it describes, which is precisely when it must not be copied into
/// a gate. Conservative on purpose: a false "not plain" costs a warning and
/// a canonical fallback, while a false "plain" puts unrunnable text in a
/// gate every dispatch then fails on.
const PLAIN_COMMAND_CHARS: &[char] = &['_', '.', '/', ':', '=', '+', ',', '-', '@'];

/// Keys that change what running a command would mean, and so disqualify its
/// text from being adopted verbatim. `working-directory:` runs it somewhere
/// other than the repo root — the gate has no such notion — and
/// `continue-on-error:` means CI does not actually require it to pass, so
/// promoting it to a gate command would enforce something CI does not.
///
/// Both are read wherever they appear, not only on a step: GitHub Actions
/// also accepts `continue-on-error:` on a *job* and `working-directory:`
/// under `defaults: run:`, and a reader that only knew the step form would
/// lift an advisory job's command straight into a hard gate.
const QUALIFYING_STEP_KEYS: &[&str] = &["working-directory:", "continue-on-error:"];

/// One command a repo's CI runs, and the workflow it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// A single shell segment, comment-stripped and trimmed.
    pub command: String,
    /// File name of the workflow it was read from, for a warning that has to
    /// tell a human where to look.
    pub workflow: String,
    /// Whether the *context* allows this text to become a gate command:
    /// false when the step qualified how it runs (see
    /// [`QUALIFYING_STEP_KEYS`]) or when the command line moved somewhere
    /// else first (`cd sub && …`). Still evidence that the tool is enforced
    /// — just not text that can be lifted out and run at the repo root.
    pub adoptable: bool,
}

impl Invocation {
    /// Whether this command can be run as written: it starts with `tool`'s
    /// own words — no environment assignment, no `+toolchain`, no directory
    /// prefix in front of them — and contains only [`PLAIN_COMMAND_CHARS`]
    /// and alphanumerics.
    ///
    /// Deliberately stricter than [`segment_invokes`], which decides the
    /// looser question *is this tool enforced here*: a `$HOME/.cargo/bin/cargo
    /// clippy …` step is evidence that clippy is enforced, and is still not
    /// text a gate can execute.
    pub fn is_plainly_runnable(&self, tool: &[&str]) -> bool {
        let mut words = self.command.split_whitespace();
        let starts_with_tool = tool.iter().all(|expected| words.next() == Some(*expected));
        starts_with_tool
            && self.command.chars().all(|ch| {
                ch.is_ascii_alphanumeric() || ch == ' ' || PLAIN_COMMAND_CHARS.contains(&ch)
            })
    }
}

/// The commands a repo's CI runs, gathered from `run:` steps.
#[derive(Debug, Default)]
pub struct CiCommands {
    invocations: Vec<Invocation>,
    /// Workflow files that exist but could not be inspected. Surfaced rather
    /// than swallowed: a skipped workflow is precisely how a gate ends up
    /// narrower than CI without anyone noticing, which is the defect this
    /// module exists to close.
    pub warnings: Vec<String>,
}

impl CiCommands {
    /// Read every workflow under `<repo_path>/.github/workflows`, in file-name
    /// order so the same checkout always yields the same evidence.
    ///
    /// A *missing* workflow directory is no evidence and no warning — CI
    /// config is optional and a repo without it must still onboard. A
    /// directory that exists and cannot be listed is a warning, because that
    /// is indistinguishable from "narrower than CI" to everyone downstream.
    ///
    /// Both path segments are resolved by exact name from a directory
    /// listing rather than through `Path::join`, for the reason
    /// [`super::exact_entry`] documents.
    pub fn read(repo_path: &Path) -> Self {
        let mut evidence = Self::default();

        let workflows = match super::exact_entry(repo_path, ".github")
            .and_then(|github| github.map_or(Ok(None), |dir| super::exact_entry(&dir, "workflows")))
        {
            Ok(Some(dir)) => dir,
            Ok(None) => return evidence,
            Err(err) => {
                evidence.warn_unreadable(&err);
                return evidence;
            }
        };

        let entries = match std::fs::read_dir(&workflows) {
            Ok(entries) => entries,
            Err(err) => {
                evidence
                    .warn_unreadable(&format!("failed to read '{}': {err}", workflows.display()));
                return evidence;
            }
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                let is_workflow_name = matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("yml") | Some("yaml")
                );
                // `Path::metadata` follows symlinks; `DirEntry::metadata` is
                // `lstat` on Unix and would drop a workflow symlinked in from
                // elsewhere in the checkout — silently, which is exactly the
                // narrowing this module exists to prevent, and the opposite of
                // what `onboard::exact_file_exists` does for a symlinked
                // manifest.
                is_workflow_name && path.metadata().map(|m| m.is_file()).unwrap_or(false)
            })
            .collect();
        files.sort();

        for file in files {
            let workflow = file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            match crate::error::read_to_string_capped(&file, MAX_WORKFLOW_BYTES, "CI workflow") {
                Ok(content) => {
                    for raw in run_commands(&content) {
                        evidence
                            .invocations
                            .extend(enforced_segments(&raw.text).map(|(segment, adoptable)| {
                                Invocation {
                                    command: segment.to_string(),
                                    workflow: workflow.clone(),
                                    adoptable: adoptable && !raw.qualified,
                                }
                            }));
                    }
                }
                Err(err) => evidence.warn_unreadable(&err),
            }
        }
        evidence
    }

    fn warn_unreadable(&mut self, err: &str) {
        self.warnings.push(format!(
            "a CI workflow could not be inspected, so the proposed gate may be narrower than \
             CI: {err}"
        ));
    }

    /// Every CI invocation of `tool` (a word sequence, e.g.
    /// `["cargo", "clippy"]`), in workflow file-name order.
    ///
    /// All of them, not the first: which one a gate should be built from is
    /// the caller's policy decision, and a caller that cannot tell two
    /// disagreeing invocations apart must be able to see that there are two.
    /// This module has no way to know which workflow runs on merge to the
    /// default branch.
    pub fn invocations_of<'a>(
        &'a self,
        tool: &'a [&'a str],
    ) -> impl Iterator<Item = &'a Invocation> {
        self.invocations
            .iter()
            .filter(move |invocation| segment_invokes(&invocation.command, tool))
    }
}

/// One command line read out of a `run:` step, with the step-level context
/// that decides whether its text may be adopted.
struct RawCommand {
    text: String,
    qualified: bool,
}

/// Every command line a workflow's `run:` steps execute, in file order, each
/// tagged with whether its step qualified how it runs.
///
/// Handles the forms a `run:` value takes: an inline scalar (`run: cargo
/// test`, optionally quoted), a literal block (`run: |`) whose lines are
/// separate commands, and a **folded** block (`run: >`, `run: >-`) whose
/// lines YAML joins into one. Folding is not cosmetic: `run: >-` is the
/// common way to wrap a long command, and reading its lines separately would
/// adopt a *truncated* command — `cargo clippy --all-targets` without the
/// `-- -D warnings` on the next line — as the gate, which is the #334 defect
/// all over again.
///
/// A block ends at the first non-blank line indented no further than the
/// **`run:` key itself** — not the `- ` that may precede it, which sits two
/// columns further left and would swallow the step's sibling keys (`name:`,
/// `env:`) as commands. A block scalar under any *other* key is skipped
/// entirely, so a `run:` line inside e.g. a `script: |` payload is not read
/// as a command this workflow runs.
///
/// Inside a literal block a line ending in an odd number of `\` continues
/// onto the next, as the shell reads it: without that, a wrapped invocation
/// is captured in truncated form.
fn run_commands(content: &str) -> Vec<RawCommand> {
    let mut scanner = Scanner::default();
    for line in content.lines() {
        scanner.read_line(line);
    }
    scanner.finish()
}

#[derive(Default)]
struct Scanner {
    commands: Vec<RawCommand>,
    /// Commands from the step being read, held until the whole step has been
    /// seen: `continue-on-error:` may sit *after* the `run:` key it
    /// qualifies.
    pending: Vec<String>,
    qualified: bool,
    /// Set by a qualifying key that sits *outside* any step — a job's own
    /// `continue-on-error:`, or a `defaults: run:` block's
    /// `working-directory:`. Nothing here maps a step back to the job it
    /// belongs to, so such a key disqualifies every command in the file: the
    /// tool still counts as enforced, its text just cannot be lifted into a
    /// gate that runs at the repo root.
    file_qualified: bool,
    /// Column of an open `defaults:` key. Its subtree configures how steps
    /// run; nothing inside it is a command the workflow executes.
    defaults_indent: Option<usize>,
    item_indent: Option<usize>,
    block: Option<Block>,
    /// A folded block's accumulated text, or a literal block's pending
    /// backslash continuation.
    joined: Option<String>,
}

struct Block {
    key_indent: usize,
    folded: bool,
    is_run: bool,
}

impl Scanner {
    fn read_line(&mut self, line: &str) {
        if self.read_block_line(line) {
            return;
        }

        let without_comment = strip_comment(line);
        let trimmed = without_comment.trim();
        if trimmed.is_empty() {
            return;
        }
        let indent = indent_of(without_comment);

        if trimmed.starts_with("- ") || trimmed == "-" {
            // A deeper sequence (an action's list argument, say) is not a new
            // step; a same-or-shallower one is.
            if self.item_indent.is_none_or(|current| indent <= current) {
                self.flush_item();
                self.item_indent = Some(indent);
            }
        }

        let Some((value, key_indent, is_run)) = scalar_value(without_comment) else {
            return;
        };

        // A `defaults:` subtree ends at the first key back at its own column.
        if self.defaults_indent.is_some_and(|open| key_indent <= open) {
            self.defaults_indent = None;
        }

        if qualifies(key_of(trimmed), &value) {
            // Inside the step being read, the qualifier is that step's alone.
            // Outside one it covers everything the workflow runs — see
            // `file_qualified`.
            if self.defaults_indent.is_none()
                && self.item_indent.is_some_and(|dash| key_indent > dash)
            {
                self.qualified = true;
            } else {
                self.file_qualified = true;
            }
        }

        if value.is_empty() && key_of(trimmed) == Some("defaults:") {
            self.defaults_indent = Some(key_indent);
        }

        // `run:` under `defaults:` is a *mapping* (`shell:`,
        // `working-directory:`), not a command. Reading it as a block scalar
        // would swallow the very `working-directory:` that says every step in
        // the workflow runs somewhere other than the repo root.
        let is_run = is_run && self.defaults_indent.is_none();
        // An empty value under a key other than `run:` is a nested *mapping*
        // (`jobs:`, `steps:`, `with:`), not a block scalar — treating it as
        // one would swallow the rest of the file.
        let starts_block =
            value.starts_with('|') || value.starts_with('>') || (is_run && value.is_empty());
        if starts_block {
            self.block = Some(Block {
                key_indent,
                folded: value.starts_with('>'),
                is_run,
            });
        } else if is_run {
            if let Some(command) = command_text(&value, true) {
                self.pending.push(command);
            }
        }
    }

    /// Consume `line` as part of an open block scalar. Returns false once the
    /// block has ended, so the caller reads the line normally — it may itself
    /// be the next key.
    fn read_block_line(&mut self, line: &str) -> bool {
        let Some(block) = &self.block else {
            return false;
        };
        if line.trim().is_empty() {
            // A blank line ends a folded paragraph; inside a literal block it
            // is simply not a command.
            if block.folded {
                self.flush_joined();
            }
            return true;
        }
        if indent_of(line) <= block.key_indent {
            self.flush_joined();
            self.block = None;
            return false;
        }
        if !block.is_run {
            return true;
        }
        let folded = block.folded;
        let Some(command) = command_text(line, false) else {
            return true;
        };
        let joined = match self.joined.take() {
            Some(head) => format!("{head} {command}"),
            None => command,
        };
        if folded {
            self.joined = Some(joined);
            return true;
        }
        match strip_line_continuation(&joined) {
            Some(head) => self.joined = Some(head),
            None => self.pending.push(joined),
        }
        true
    }

    fn flush_joined(&mut self) {
        if let Some(joined) = self.joined.take() {
            self.pending.push(joined);
        }
    }

    fn flush_item(&mut self) {
        let qualified = self.qualified;
        self.commands.extend(
            self.pending
                .drain(..)
                .map(|text| RawCommand { text, qualified }),
        );
        self.qualified = false;
    }

    fn finish(mut self) -> Vec<RawCommand> {
        self.flush_joined();
        self.flush_item();
        if self.file_qualified {
            // Applied last because a job-level qualifier may be read after
            // some of the steps it covers have already been flushed.
            for command in &mut self.commands {
                command.qualified = true;
            }
        }
        self.commands
    }
}

/// Whether this key/value pair actually changes what running a command would
/// mean.
///
/// A qualifier that qualifies nothing must not disqualify a command:
/// `working-directory: .` names the repo root — where a gate command runs
/// anyway — and `continue-on-error: false` is the explicit spelling of the
/// default. Treating either as a disqualifier would discard an exactly
/// runnable command in favour of a canonical one that may be stricter, and
/// tell the human CI runs something a gate cannot take, which would be false.
fn qualifies(key: Option<&str>, value: &str) -> bool {
    if !QUALIFYING_STEP_KEYS.iter().any(|known| key == Some(*known)) {
        return false;
    }
    let value = super::strip_matching_quotes(value.trim());
    match key {
        Some("working-directory:") => !matches!(value, "." | "./"),
        Some("continue-on-error:") => value != "false",
        _ => true,
    }
}

/// The bare `key:` at the start of a trimmed line, including its colon, with
/// any `- ` sequence marker removed.
fn key_of(trimmed: &str) -> Option<&str> {
    let key = trimmed.strip_prefix("- ").unwrap_or(trimmed).trim_start();
    key.find(':').map(|colon| &key[..=colon])
}

/// The scalar value of a mapping key on this line, the column the key itself
/// starts at, and whether that key is `run:`. Non-`run:` keys are reported
/// too, so their block scalars can be skipped rather than read as commands.
fn scalar_value(without_comment: &str) -> Option<(String, usize, bool)> {
    let trimmed = without_comment.trim_start();
    let key = trimmed.strip_prefix("- ").unwrap_or(trimmed).trim_start();
    let key_indent = without_comment.len() - key.len();
    let colon = key.find(':')?;
    let (name, rest) = key.split_at(colon);
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let value = rest.strip_prefix(':')?.trim().to_string();
    Some((value, key_indent, name == "run"))
}

/// A command line's runnable text: comment stripped, trimmed, and — only for
/// the inline `run: "…"` scalar form — unquoted. `None` when nothing is left.
///
/// `unquote` is not a convenience. Inside a block scalar the quotes belong to
/// the *shell*, so stripping them turns the line `'cargo fmt --all'` — which
/// asks the shell for a binary with that literal name — into something that
/// reads as a plain, adoptable gate command.
fn command_text(line: &str, unquote: bool) -> Option<String> {
    let text = strip_comment(line).trim();
    let text = if unquote {
        super::strip_matching_quotes(text).trim()
    } else {
        text
    };
    (!text.is_empty()).then(|| text.to_string())
}

/// `line` without its trailing shell line-continuation, or `None` if it does
/// not end in one. An *odd* number of trailing backslashes continues the
/// line; an even number is escaped literal backslashes and ends it.
fn strip_line_continuation(line: &str) -> Option<String> {
    let trailing = line.chars().rev().take_while(|ch| *ch == '\\').count();
    (trailing % 2 == 1).then(|| line[..line.len() - 1].trim_end().to_string())
}

/// Truncate at the first `#` that starts a comment — YAML's own rule (and the
/// shell's inside a block scalar): at line start or preceded by whitespace,
/// and not inside a quoted string. Without the quote tracking, a command like
/// `grep "TODO #1" && cargo clippy …` is truncated and the clippy evidence
/// silently lost.
fn strip_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut after_whitespace = true;
    for (i, ch) in line.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double && after_whitespace => return &line[..i],
            _ => {}
        }
        after_whitespace = ch.is_whitespace();
    }
    line
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The segments of a command line that CI actually requires to succeed, each
/// with whether its text may be adopted as a gate command.
///
/// Two shell facts decide this, and both are the difference between a gate
/// that means something and one that does not:
///
/// * A segment adjacent to `||` is **dropped entirely**. `cargo clippy … ||
///   true` is how a repo runs a check advisorily: CI ignores its exit status,
///   so it is not evidence of enforcement, and promoting it to a gate command
///   would fail every dispatch into a repo CI is perfectly happy with.
/// * A segment after a `cd` is not adoptable. Its text runs somewhere other
///   than the repo root, which a gate command has no way to express — the
///   same reason `working-directory:` disqualifies a step.
fn enforced_segments(command: &str) -> impl Iterator<Item = (&str, bool)> {
    let (pieces, operators) = split_on_operators(command);
    let mut segments = Vec::new();
    let mut moved = false;
    let mut previous_was_or = false;
    for (position, piece) in pieces.iter().enumerate() {
        let piece = piece.trim();
        let followed_by_or = operators.get(position).copied() == Some("||");
        let is_advisory = previous_was_or || followed_by_or;
        previous_was_or = followed_by_or;
        if piece.is_empty() {
            continue;
        }
        if piece.split_whitespace().next() == Some("cd") {
            moved = true;
            continue;
        }
        if is_advisory {
            continue;
        }
        segments.push((piece, !moved));
    }
    segments.into_iter()
}

/// A command line's pieces and the separator that follows each — the last
/// piece has none.
///
/// Separators are read as whole tokens, so `||` is one operator rather than
/// two empty-separated `|`s. The distinction is the point: only `||` makes
/// the command before it advisory. A single `|` does not, because GitHub
/// Actions runs a `run:` step under `bash -eo pipefail`, where a failing
/// left-hand side still fails the step — reading `cargo clippy … | tee log`
/// as advisory would drop the evidence and leave the check silently out of
/// the gate.
fn split_on_operators(command: &str) -> (Vec<&str>, Vec<&str>) {
    let mut pieces = Vec::new();
    let mut operators = Vec::new();
    let bytes = command.as_bytes();
    let (mut start, mut index) = (0, 0);
    while index < bytes.len() {
        let ch = bytes[index];
        if ch != b'&' && ch != b'|' && ch != b';' {
            index += 1;
            continue;
        }
        let mut end = index + 1;
        if ch != b';' && bytes.get(end) == Some(&ch) {
            end += 1;
        }
        pieces.push(&command[start..index]);
        operators.push(&command[index..end]);
        start = end;
        index = end;
    }
    pieces.push(&command[start..]);
    (pieces, operators)
}

/// Whether `segment` invokes `tool`: its words — after any leading
/// `KEY=value` environment assignments, and ignoring a `+toolchain`
/// selector — begin with `tool`'s words. The first word is compared by base
/// name, so `$HOME/.cargo/bin/cargo` counts as `cargo`; CI workflows really
/// do call cargo by absolute path (this repo's own macOS job does).
///
/// This is the loose question — *is this tool enforced here* — and matching
/// text this way is never enough to make it a gate command; see
/// [`Invocation::is_plainly_runnable`] and [`Invocation::adoptable`].
fn segment_invokes(segment: &str, tool: &[&str]) -> bool {
    let mut words = segment
        .split_whitespace()
        .skip_while(|word| is_env_assignment(word))
        .enumerate()
        .filter(|(position, word)| !(*position > 0 && word.starts_with('+')))
        .map(|(_, word)| word);
    tool.iter().enumerate().all(|(position, expected)| {
        words.next().is_some_and(|word| {
            let word = if position == 0 {
                word.rsplit('/').next().unwrap_or(word)
            } else {
                word
            };
            word == *expected
        })
    })
}

/// Whether a word is a leading `KEY=value` environment assignment rather
/// than the command itself.
fn is_env_assignment(word: &str) -> bool {
    let Some((key, _)) = word.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIPPY: &[&str] = &["cargo", "clippy"];
    const FMT: &[&str] = &["cargo", "fmt"];

    fn workflow(dir: &Path, name: &str, content: &str) {
        let workflows = dir.join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join(name), content).unwrap();
    }

    fn read(content: &str) -> CiCommands {
        let dir = tempfile::tempdir().unwrap();
        workflow(dir.path(), "ci.yml", content);
        CiCommands::read(dir.path())
    }

    fn first<'a>(evidence: &'a CiCommands, tool: &'a [&'a str]) -> Option<&'a Invocation> {
        evidence.invocations_of(tool).next()
    }

    fn command_of<'a>(evidence: &'a CiCommands, tool: &'a [&'a str]) -> Option<&'a str> {
        first(evidence, tool).map(|invocation| invocation.command.as_str())
    }

    #[test]
    fn finds_an_inline_run_command() {
        let evidence = read(
            "jobs:\n  check:\n    steps:\n      - run: cargo clippy --workspace -- -D warnings\n",
        );
        assert_eq!(
            command_of(&evidence, CLIPPY),
            Some("cargo clippy --workspace -- -D warnings")
        );
        assert!(first(&evidence, CLIPPY).unwrap().adoptable);
    }

    #[test]
    fn finds_a_command_in_a_literal_block_and_stops_at_the_dedent() {
        let evidence = read(
            "steps:\n  - run: |\n      rustup component add rustfmt\n      cargo fmt --all -- --check\n  - name: after\n    uses: actions/checkout@v4\n",
        );
        assert_eq!(
            command_of(&evidence, FMT),
            Some("cargo fmt --all -- --check")
        );
        assert_eq!(evidence.invocations.len(), 2, "{:?}", evidence.invocations);
    }

    #[test]
    fn a_folded_block_is_joined_into_one_command() {
        // `run: >-` is the common way to wrap a long command. Reading its
        // lines separately would adopt `cargo clippy --workspace
        // --all-targets` — without the `-D warnings` that makes it a check —
        // as the gate, which is defect #334 all over again.
        let evidence = read(
            "steps:\n  - run: >-\n      cargo clippy --workspace --all-targets\n      -- -D warnings\n",
        );
        assert_eq!(
            command_of(&evidence, CLIPPY),
            Some("cargo clippy --workspace --all-targets -- -D warnings")
        );
    }

    #[test]
    fn a_block_scalar_ends_at_its_own_key_column_not_the_sequence_dash() {
        let evidence = read(
            "steps:\n  - run: |\n      make build\n    name: Build\n    env:\n      TOOL: cargo clippy --fix\n",
        );
        assert_eq!(evidence.invocations.len(), 1, "{:?}", evidence.invocations);
        assert_eq!(command_of(&evidence, CLIPPY), None);
    }

    #[test]
    fn a_run_line_inside_another_keys_block_is_not_a_command_this_workflow_runs() {
        let evidence = read(
            "steps:\n  - uses: some/action@v1\n    with:\n      script: |\n        run: cargo clippy --fix --allow-dirty\n",
        );
        assert_eq!(command_of(&evidence, CLIPPY), None);
    }

    #[test]
    fn a_backslash_continued_command_is_joined_before_it_is_matched() {
        let evidence = read(
            "steps:\n  - run: |\n      cargo clippy --workspace --all-targets \\\n        -- -D warnings\n",
        );
        assert_eq!(
            command_of(&evidence, CLIPPY),
            Some("cargo clippy --workspace --all-targets -- -D warnings")
        );
    }

    #[test]
    fn an_escaped_trailing_backslash_does_not_continue_the_line() {
        // The line ends in *two* backslashes — one escaped literal backslash,
        // an even count — so the command ends there. Reading it as a
        // continuation would swallow the next line's real invocation.
        let evidence = read(
            "steps:\n  - run: |\n      printf a\\\\\n      cargo clippy --workspace -- -D warnings\n",
        );
        assert_eq!(
            command_of(&evidence, CLIPPY),
            Some("cargo clippy --workspace -- -D warnings")
        );
        assert_eq!(evidence.invocations.len(), 2, "{:?}", evidence.invocations);
    }

    #[test]
    fn a_working_directory_of_the_repo_root_disqualifies_nothing() {
        let evidence =
            read("steps:\n  - run: cargo fmt --all -- --check\n    working-directory: .\n");
        assert!(first(&evidence, FMT).unwrap().adoptable);
    }

    #[test]
    fn continue_on_error_false_is_the_default_spelled_out_not_a_qualifier() {
        let evidence =
            read("steps:\n  - run: cargo fmt --all -- --check\n    continue-on-error: false\n");
        assert!(first(&evidence, FMT).unwrap().adoptable);
    }

    #[test]
    fn quotes_inside_a_block_scalar_belong_to_the_shell_and_are_left_alone() {
        // Stripping them would turn a line asking the shell for a binary
        // literally named `cargo fmt --all` into an adoptable gate command.
        let evidence = read("steps:\n  - run: |\n      'cargo fmt --all'\n");
        let invocation = first(&evidence, FMT);
        assert!(
            invocation.is_none_or(|found| !found.is_plainly_runnable(FMT)),
            "{invocation:?}"
        );
    }

    #[test]
    fn a_piped_invocation_is_still_enforced() {
        // GitHub Actions runs `run:` under `bash -eo pipefail`, so a failing
        // left-hand side fails the step. Reading a single `|` as `||` would
        // drop the evidence and leave the check silently out of the gate.
        let evidence =
            read("steps:\n  - run: cargo clippy --all-targets -- -D warnings | tee lint.log\n");
        let invocation = first(&evidence, CLIPPY).unwrap();
        assert_eq!(
            invocation.command,
            "cargo clippy --all-targets -- -D warnings"
        );
        assert!(invocation.adoptable && invocation.is_plainly_runnable(CLIPPY));
    }

    #[test]
    fn a_job_level_continue_on_error_disqualifies_the_steps_it_covers() {
        // `jobs.<id>.continue-on-error` marks a whole job advisory. Its steps
        // are still evidence the tool runs, but CI does not require them to
        // pass, so lifting one into a gate would enforce something CI does
        // not — and fail every dispatch into a repo CI is happy with.
        let evidence = read(
            "jobs:\n  lint:\n    continue-on-error: true\n    steps:\n      - run: cargo clippy --all-targets -- -D warnings\n",
        );
        let invocation = first(&evidence, CLIPPY).unwrap();
        assert!(!invocation.adoptable, "{invocation:?}");
    }

    #[test]
    fn a_defaults_run_working_directory_disqualifies_the_workflows_commands() {
        // `defaults: run:` is a *mapping*, not a block scalar. Reading it as
        // one swallows the `working-directory:` that says every step in the
        // workflow runs somewhere other than the repo root — and emits that
        // key as a command the workflow supposedly runs.
        let evidence = read(
            "defaults:\n  run:\n    working-directory: crates/engine\njobs:\n  lint:\n    steps:\n      - run: cargo clippy --all-targets -- -D warnings\n",
        );
        let invocation = first(&evidence, CLIPPY).unwrap();
        assert!(!invocation.adoptable, "{invocation:?}");
        assert_eq!(evidence.invocations.len(), 1, "{:?}", evidence.invocations);
    }

    #[test]
    fn a_step_run_block_with_no_indicator_is_still_read_as_commands() {
        // The mapping-shaped `defaults: run:` above must not cost the plain
        // multi-line scalar form a step legitimately uses.
        let evidence = read("steps:\n  - run:\n      cargo fmt --all -- --check\n");
        assert_eq!(
            command_of(&evidence, FMT),
            Some("cargo fmt --all -- --check")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_workflow_is_read_rather_than_silently_skipped() {
        // `DirEntry::metadata` is `lstat` on Unix, so filtering on it drops a
        // symlinked workflow with no warning — the silent narrowing this
        // module exists to prevent.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("shared-ci.yml");
        std::fs::write(&real, "steps:\n  - run: cargo clippy --all-targets\n").unwrap();
        let workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::os::unix::fs::symlink(&real, workflows.join("ci.yml")).unwrap();

        let evidence = CiCommands::read(dir.path());

        assert_eq!(
            command_of(&evidence, CLIPPY),
            Some("cargo clippy --all-targets")
        );
        assert!(evidence.warnings.is_empty(), "{:?}", evidence.warnings);
    }

    #[test]
    fn an_advisory_invocation_is_not_evidence_at_all() {
        // `|| true` is how a repo runs a check it does not enforce. CI
        // ignores the exit status; a gate would not, and every dispatch
        // into a repo CI is happy with would fail.
        let evidence = read("steps:\n  - run: cargo clippy --all-targets -- -D warnings || true\n");
        assert_eq!(command_of(&evidence, CLIPPY), None);
    }

    #[test]
    fn a_command_after_cd_is_evidence_but_not_adoptable() {
        let evidence = read(
            "steps:\n  - run: cd crates/engine && cargo clippy --all-targets -- -D warnings\n",
        );
        let invocation = first(&evidence, CLIPPY).unwrap();
        assert!(!invocation.adoptable);
    }

    #[test]
    fn a_step_with_working_directory_is_evidence_but_not_adoptable() {
        let evidence = read(
            "steps:\n  - run: cargo clippy --all-targets -- -D warnings\n    working-directory: crates/engine\n",
        );
        let invocation = first(&evidence, CLIPPY).unwrap();
        assert!(!invocation.adoptable, "{invocation:?}");
    }

    #[test]
    fn a_continue_on_error_step_is_evidence_but_not_adoptable() {
        // The key sits *after* the `run:` it qualifies, which is why a step's
        // commands are held until the whole step has been read.
        let evidence = read(
            "steps:\n  - run: cargo clippy --all-targets -- -D warnings\n    continue-on-error: true\n",
        );
        assert!(!first(&evidence, CLIPPY).unwrap().adoptable);
    }

    #[test]
    fn a_qualifying_key_does_not_leak_into_the_next_step() {
        let evidence = read(
            "steps:\n  - run: cargo fmt --all -- --check\n    working-directory: sub\n  - run: cargo clippy --all-targets -- -D warnings\n",
        );
        assert!(!first(&evidence, FMT).unwrap().adoptable);
        assert!(first(&evidence, CLIPPY).unwrap().adoptable);
    }

    #[test]
    fn unquotes_a_quoted_run_scalar_and_matches_cargo_by_base_name() {
        let evidence = read("steps:\n  - run: \"$HOME/.cargo/bin/cargo clippy --all-targets\"\n");
        let invocation = first(&evidence, CLIPPY).unwrap();
        assert_eq!(
            invocation.command,
            "$HOME/.cargo/bin/cargo clippy --all-targets"
        );
        // Evidence that clippy is enforced — but not text a gate can run.
        assert!(!invocation.is_plainly_runnable(CLIPPY));
    }

    #[test]
    fn a_toolchain_selector_is_evidence_but_not_plainly_runnable() {
        let evidence = read("steps:\n  - run: cargo +nightly fmt --all -- --check\n");
        assert!(!first(&evidence, FMT).unwrap().is_plainly_runnable(FMT));
    }

    #[test]
    fn an_expression_or_env_prefixed_command_is_not_plainly_runnable() {
        let evidence = read(
            "steps:\n  - run: RUSTFLAGS=-Dwarnings cargo clippy --all-targets\n  - run: cargo fmt --all -- --check ${{ matrix.extra }}\n",
        );
        assert!(!first(&evidence, CLIPPY)
            .unwrap()
            .is_plainly_runnable(CLIPPY));
        assert!(!first(&evidence, FMT).unwrap().is_plainly_runnable(FMT));
    }

    #[test]
    fn a_step_name_that_merely_mentions_a_tool_is_not_evidence() {
        let evidence = read("steps:\n  - name: cargo clippy\n    uses: some/action@v1\n");
        assert_eq!(command_of(&evidence, CLIPPY), None);
    }

    #[test]
    fn a_commented_out_run_line_is_not_evidence() {
        let evidence = read("steps:\n  # - run: cargo clippy --workspace\n  - run: cargo test\n");
        assert_eq!(command_of(&evidence, CLIPPY), None);
        assert_eq!(
            command_of(&evidence, &["cargo", "test"]),
            Some("cargo test")
        );
    }

    #[test]
    fn a_trailing_comment_is_stripped_from_the_command() {
        let evidence =
            read("steps:\n  - run: cargo fmt --all -- --check # keep the tree formatted\n");
        assert_eq!(
            command_of(&evidence, FMT),
            Some("cargo fmt --all -- --check")
        );
    }

    #[test]
    fn a_hash_inside_quotes_does_not_truncate_the_command() {
        // Truncating here would lose the clippy invocation entirely and read
        // as "this repo enforces nothing" — narrower than CI, silently.
        let evidence = read(
            "steps:\n  - run: |\n      grep -n \"TODO #1\" . && cargo clippy --all-targets -- -D warnings\n",
        );
        assert_eq!(
            command_of(&evidence, CLIPPY),
            Some("cargo clippy --all-targets -- -D warnings")
        );
    }

    #[test]
    fn finds_a_tool_invoked_after_a_shell_separator() {
        let evidence = read("steps:\n  - run: cargo build && cargo clippy --all-targets\n");
        let invocation = first(&evidence, CLIPPY).unwrap();
        assert_eq!(invocation.command, "cargo clippy --all-targets");
        assert!(invocation.is_plainly_runnable(CLIPPY) && invocation.adoptable);
    }

    #[test]
    fn a_tool_the_workflow_never_runs_is_not_found() {
        assert_eq!(
            command_of(&read("steps:\n  - run: cargo test\n"), CLIPPY),
            None
        );
    }

    #[test]
    fn every_invocation_of_a_tool_is_reported_in_file_name_order() {
        // Which one a gate should use is the caller's policy, and it cannot
        // choose between two disagreeing commands it never sees.
        let dir = tempfile::tempdir().unwrap();
        workflow(
            dir.path(),
            "a-nightly.yml",
            "steps:\n  - run: cargo clippy --all-targets -- -D warnings -W clippy::pedantic\n",
        );
        workflow(
            dir.path(),
            "ci.yml",
            "steps:\n  - run: cargo clippy --workspace -- -D warnings\n",
        );
        let evidence = CiCommands::read(dir.path());
        let found: Vec<(&str, &str)> = evidence
            .invocations_of(CLIPPY)
            .map(|invocation| (invocation.workflow.as_str(), invocation.command.as_str()))
            .collect();
        assert_eq!(
            found,
            vec![
                (
                    "a-nightly.yml",
                    "cargo clippy --all-targets -- -D warnings -W clippy::pedantic"
                ),
                ("ci.yml", "cargo clippy --workspace -- -D warnings"),
            ]
        );
    }

    #[test]
    fn no_workflow_directory_is_no_evidence_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let evidence = CiCommands::read(dir.path());
        assert_eq!(command_of(&evidence, FMT), None);
        assert!(evidence.warnings.is_empty());
    }

    #[test]
    fn a_workflow_directory_that_cannot_be_listed_warns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".github")).unwrap();
        std::fs::write(dir.path().join(".github").join("workflows"), "not a dir").unwrap();
        let evidence = CiCommands::read(dir.path());
        assert_eq!(evidence.warnings.len(), 1, "{:?}", evidence.warnings);
        assert!(
            evidence.warnings[0].contains("narrower than CI"),
            "got: {:?}",
            evidence.warnings
        );
    }

    #[test]
    fn the_workflow_directory_is_resolved_by_exact_name() {
        // Case-insensitive path resolution on a default macOS filesystem
        // would otherwise infer a different gate than Linux does from the
        // identical commit.
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join(".GitHub").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join("ci.yml"), "steps:\n  - run: cargo clippy\n").unwrap();
        let evidence = CiCommands::read(dir.path());
        assert_eq!(command_of(&evidence, CLIPPY), None);
        assert!(evidence.warnings.is_empty());
    }

    #[test]
    fn a_non_workflow_file_in_the_directory_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        workflow(dir.path(), "ci.yml", "steps:\n  - run: cargo test\n");
        workflow(dir.path(), "README.md", "run: cargo clippy\n");
        assert_eq!(command_of(&CiCommands::read(dir.path()), CLIPPY), None);
    }

    #[test]
    fn an_oversized_workflow_is_skipped_with_a_warning() {
        let evidence = read(&format!(
            "steps:\n  - run: cargo clippy\n# {}\n",
            "x".repeat(MAX_WORKFLOW_BYTES as usize)
        ));
        assert_eq!(command_of(&evidence, CLIPPY), None);
        assert_eq!(evidence.warnings.len(), 1);
        assert!(
            evidence.warnings[0].contains("ci.yml"),
            "expected the warning to name the workflow, got: {:?}",
            evidence.warnings
        );
    }
}

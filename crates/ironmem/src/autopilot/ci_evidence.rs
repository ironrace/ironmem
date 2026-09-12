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
//! An invocation this module reads becomes a gate command **only when it is
//! plainly runnable as written** — see [`Invocation::is_plainly_runnable`].
//! That is the faithful thing to propose: a command CI runs on every merge to
//! the default branch is, by construction, one the repo can satisfy, and
//! matching it exactly is the whole point (a gate stricter than CI blocks work
//! CI would have accepted; a gate looser than CI is the defect above).
//!
//! When the invocation is *not* plainly runnable — multi-line shell, a `${{ }}`
//! expression only a workflow runner resolves, an absolute toolchain path that
//! exists only on the runner — the caller falls back to its own canonical
//! command and says so on the proposal, because inventing a local equivalent
//! of runner-specific text is exactly the guess that produces a gate nobody
//! can pass.
//!
//! # Bounded reader, not a YAML parser
//!
//! This finds command text and stops: no anchors, no matrices, no job graph,
//! no `env`/expression resolution, no knowledge of which jobs are required,
//! `if:`-conditional or `continue-on-error:`. A tool enforced only through a
//! third-party action (`uses: actions-rs/clippy-check@v1`) or behind an
//! indirection (`make lint`, `cargo xtask ci`) is therefore invisible here,
//! and such a repo onboards exactly as it did before this module existed. The
//! human approval step is the backstop the spec's own risk table names for a
//! wrongly inferred gate command.
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
pub(super) const MAX_WORKFLOW_BYTES: u64 = 1024 * 1024;

/// Characters a command may contain and still be run as written. Everything
/// outside this set — `$`, quotes, braces, backslashes, redirections, globs
/// — means the text depends on a shell or a workflow runner to become the
/// command it describes, which is precisely when it must not be copied into
/// a gate. Conservative on purpose: a false "not plain" costs a warning and
/// a canonical fallback, while a false "plain" puts unrunnable text in a
/// gate every dispatch then fails on.
const PLAIN_COMMAND_CHARS: &[char] = &['_', '.', '/', ':', '=', '+', ',', '-', '@'];

/// One command a repo's CI runs, and the workflow it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// A single shell segment, comment-stripped and trimmed.
    pub command: String,
    /// File name of the workflow it was read from, for a warning that has to
    /// tell a human where to look.
    pub workflow: String,
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
    /// `onboard::exact_file_exists` documents: the OS path layer is
    /// case-insensitive on a default macOS filesystem and case-sensitive on
    /// the Linux runners these commands actually run on, so joining would let
    /// the same commit infer different gates on different machines.
    pub fn read(repo_path: &Path) -> Self {
        let mut evidence = Self::default();

        let workflows = match exact_child(repo_path, ".github")
            .and_then(|github| github.map_or(Ok(None), |dir| exact_child(&dir, "workflows")))
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
            .filter(|entry| {
                let is_workflow_name = matches!(
                    entry.path().extension().and_then(|ext| ext.to_str()),
                    Some("yml") | Some("yaml")
                );
                is_workflow_name && entry.metadata().map(|m| m.is_file()).unwrap_or(false)
            })
            .map(|entry| entry.path())
            .collect();
        files.sort();

        for file in files {
            let workflow = file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            match crate::error::read_to_string_capped(&file, MAX_WORKFLOW_BYTES, "CI workflow") {
                Ok(content) => {
                    for command in run_commands(&content) {
                        evidence
                            .invocations
                            .extend(shell_segments(&command).map(|segment| Invocation {
                                command: segment.to_string(),
                                workflow: workflow.clone(),
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

    /// The CI invocation of `tool` (a word sequence, e.g.
    /// `["cargo", "clippy"]`) a gate should be built from, or `None` if CI
    /// never runs it.
    ///
    /// Prefers a plainly runnable invocation over one that merely proves the
    /// tool is enforced, wherever each sits: a repo whose nightly workflow
    /// sorts before `ci.yml` must not be judged by the nightly wording when
    /// the required job's command could have been used verbatim.
    pub fn invocation_of(&self, tool: &[&str]) -> Option<&Invocation> {
        let mut first_match = None;
        for invocation in self
            .invocations
            .iter()
            .filter(|invocation| segment_invokes(&invocation.command, tool))
        {
            if invocation.is_plainly_runnable(tool) {
                return Some(invocation);
            }
            first_match.get_or_insert(invocation);
        }
        first_match
    }
}

/// `<parent>/<name>`, but only when an entry with *exactly* that name is in
/// `parent`'s listing (see [`CiCommands::read`] on why the path layer is not
/// trusted to answer this). A missing `parent` is `Ok(None)`: nothing to
/// read, nothing to report. Any other listing failure is the caller's to
/// report, because a directory that exists and cannot be read is not the
/// same as one that does not exist.
fn exact_child(parent: &Path, name: &str) -> Result<Option<PathBuf>, String> {
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("failed to read '{}': {err}", parent.display())),
    };
    Ok(entries
        .filter_map(Result::ok)
        .find(|entry| entry.file_name() == std::ffi::OsStr::new(name))
        .map(|entry| entry.path()))
}

/// Every command line a workflow's `run:` steps execute, in file order.
///
/// Handles the two forms a `run:` value takes: an inline scalar
/// (`run: cargo test`, optionally quoted) and a block scalar (`run: |`,
/// `run: >-`, …) whose more-indented lines are the script. A block ends at
/// the first non-blank line indented no further than the **`run:` key
/// itself** — not the `- ` that may precede it, which sits two columns
/// further left and would swallow the step's sibling keys (`name:`, `env:`)
/// as commands.
///
/// A line ending in `\` continues onto the next, as the shell reads it:
/// without that, a wrapped invocation is captured in truncated form and then
/// reported as disagreeing with the command it is character-for-character
/// identical to.
fn run_commands(content: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut block_key_indent: Option<usize> = None;
    let mut continued: Option<String> = None;

    for line in content.lines() {
        if let Some(key_indent) = block_key_indent {
            if line.trim().is_empty() {
                continue;
            }
            if indent_of(line) > key_indent {
                if let Some(command) = command_text(line) {
                    match continued.take() {
                        Some(head) => continued = Some(format!("{head} {command}")),
                        None => continued = Some(command),
                    }
                    let joined = continued.as_ref().expect("just assigned");
                    if joined.ends_with('\\') {
                        continued = Some(joined.trim_end_matches('\\').trim_end().to_string());
                        continue;
                    }
                    commands.push(continued.take().expect("just checked"));
                }
                continue;
            }
            commands.extend(continued.take());
            // Dedented out of the block — fall through and read this line as
            // an ordinary one; it may itself be the next `run:` key.
            block_key_indent = None;
        }

        let Some((value, key_indent)) = run_value(line) else {
            continue;
        };
        if value.is_empty() || value.starts_with('|') || value.starts_with('>') {
            block_key_indent = Some(key_indent);
        } else if let Some(command) = command_text(&value) {
            commands.push(command);
        }
    }
    commands.extend(continued);

    commands
}

/// The value of a `run:` mapping key on this line and the column the key
/// itself starts at, if it is one. Accepts the `- run:` sequence-item form
/// as well as a bare `run:`; anything else (including a `name: cargo clippy`
/// step *label*, which names a tool without running it) is not a command.
fn run_value(line: &str) -> Option<(String, usize)> {
    let without_comment = strip_comment(line);
    let trimmed = without_comment.trim_start();
    let key = trimmed.strip_prefix("- ").unwrap_or(trimmed).trim_start();
    let key_indent = without_comment.len() - key.len();
    key.strip_prefix("run:")
        .map(|value| (value.trim().to_string(), key_indent))
}

/// A command line's runnable text: comment stripped, unquoted, trimmed.
/// `None` when nothing is left.
fn command_text(line: &str) -> Option<String> {
    let text = strip_comment(line).trim();
    let text = super::strip_matching_quotes(text).trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Truncate at the first `#` that starts a comment — YAML's own rule (and
/// the shell's inside a block scalar): at line start, or preceded by
/// whitespace. A `#` mid-word (`--foo=#1`) is not a comment.
fn strip_comment(line: &str) -> &str {
    line.char_indices()
        .find(|(i, ch)| *ch == '#' && (*i == 0 || line[..*i].ends_with(char::is_whitespace)))
        .map_or(line, |(i, _)| &line[..i])
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Split a command line on shell separators so a tool invoked after `&&`,
/// `||`, `;` or a pipe is found. Splitting on the bare characters (rather
/// than the two-character operators) is deliberate: it costs an empty
/// segment per `&&`, which never matches anything, and needs no operator
/// table.
fn shell_segments(command: &str) -> impl Iterator<Item = &str> {
    command.split(['&', '|', ';']).map(str::trim)
}

/// Whether `segment` invokes `tool`: its words — after any leading
/// `KEY=value` environment assignments, and ignoring a `+toolchain`
/// selector — begin with `tool`'s words. The first word is compared by base
/// name, so `$HOME/.cargo/bin/cargo` counts as `cargo`; CI workflows really
/// do call cargo by absolute path (this repo's own macOS job does).
///
/// This is the loose question — *is this tool enforced here* — and matching
/// text this way is never enough to make it a gate command; see
/// [`Invocation::is_plainly_runnable`].
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

    fn command_of<'a>(evidence: &'a CiCommands, tool: &[&str]) -> Option<&'a str> {
        evidence
            .invocation_of(tool)
            .map(|invocation| invocation.command.as_str())
    }

    #[test]
    fn finds_an_inline_run_command() {
        let dir = tempfile::tempdir().unwrap();
        workflow(
            dir.path(),
            "ci.yml",
            "jobs:\n  check:\n    steps:\n      - run: cargo clippy --workspace -- -D warnings\n",
        );
        assert_eq!(
            command_of(&CiCommands::read(dir.path()), CLIPPY),
            Some("cargo clippy --workspace -- -D warnings")
        );
    }

    #[test]
    fn finds_a_command_in_a_block_scalar_and_stops_at_the_dedent() {
        let dir = tempfile::tempdir().unwrap();
        workflow(
            dir.path(),
            "ci.yml",
            "steps:\n  - run: |\n      rustup component add rustfmt\n      cargo fmt --all -- --check\n  - name: after\n    uses: actions/checkout@v4\n",
        );
        let evidence = CiCommands::read(dir.path());
        assert_eq!(
            command_of(&evidence, FMT),
            Some("cargo fmt --all -- --check")
        );
        assert_eq!(evidence.invocations.len(), 2, "{:?}", evidence.invocations);
    }

    #[test]
    fn a_block_scalar_ends_at_its_own_key_column_not_the_sequence_dash() {
        // `- run: |` puts the key two columns right of the dash. Comparing
        // against the dash swallows the step's sibling keys as commands,
        // which are junk evidence at best and a tool CI never runs at worst.
        let dir = tempfile::tempdir().unwrap();
        workflow(
            dir.path(),
            "ci.yml",
            "steps:\n  - run: |\n      make build\n    name: Build\n    env:\n      TOOL: cargo clippy --fix\n",
        );
        let evidence = CiCommands::read(dir.path());
        assert_eq!(evidence.invocations.len(), 1, "{:?}", evidence.invocations);
        assert_eq!(command_of(&evidence, CLIPPY), None);
    }

    #[test]
    fn a_backslash_continued_command_is_joined_before_it_is_matched() {
        let dir = tempfile::tempdir().unwrap();
        workflow(
            dir.path(),
            "ci.yml",
            "steps:\n  - run: |\n      cargo clippy --workspace --all-targets \\\n        -- -D warnings\n",
        );
        assert_eq!(
            command_of(&CiCommands::read(dir.path()), CLIPPY),
            Some("cargo clippy --workspace --all-targets -- -D warnings")
        );
    }

    #[test]
    fn unquotes_a_quoted_run_scalar_and_matches_cargo_by_base_name() {
        // This repo's own macOS job calls cargo by absolute path.
        let dir = tempfile::tempdir().unwrap();
        workflow(
            dir.path(),
            "ci.yml",
            "steps:\n  - run: \"$HOME/.cargo/bin/cargo clippy --all-targets\"\n",
        );
        let evidence = CiCommands::read(dir.path());
        let invocation = evidence.invocation_of(CLIPPY).unwrap();
        assert_eq!(
            invocation.command,
            "$HOME/.cargo/bin/cargo clippy --all-targets"
        );
        // Evidence that clippy is enforced — but not text a gate can run.
        assert!(!invocation.is_plainly_runnable(CLIPPY));
    }

    #[test]
    fn a_plainly_runnable_invocation_is_preferred_over_one_that_is_not() {
        // File-name order puts the nightly workflow first; the required job's
        // command is the one a gate can actually use.
        let dir = tempfile::tempdir().unwrap();
        workflow(
            dir.path(),
            "a-nightly.yml",
            "steps:\n  - run: cargo +nightly clippy --all-targets\n",
        );
        workflow(
            dir.path(),
            "ci.yml",
            "steps:\n  - run: cargo clippy --workspace -- -D warnings\n",
        );
        let evidence = CiCommands::read(dir.path());
        let invocation = evidence.invocation_of(CLIPPY).unwrap();
        assert_eq!(
            invocation.command,
            "cargo clippy --workspace -- -D warnings"
        );
        assert_eq!(invocation.workflow, "ci.yml");
    }

    #[test]
    fn a_toolchain_selector_is_evidence_but_not_plainly_runnable() {
        let dir = tempfile::tempdir().unwrap();
        workflow(
            dir.path(),
            "ci.yml",
            "steps:\n  - run: cargo +nightly fmt --all -- --check\n",
        );
        let evidence = CiCommands::read(dir.path());
        let invocation = evidence.invocation_of(FMT).unwrap();
        assert!(!invocation.is_plainly_runnable(FMT));
    }

    #[test]
    fn an_expression_or_env_prefixed_command_is_not_plainly_runnable() {
        let dir = tempfile::tempdir().unwrap();
        workflow(
            dir.path(),
            "ci.yml",
            "steps:\n  - run: RUSTFLAGS=-Dwarnings cargo clippy --all-targets\n  - run: cargo fmt --all -- --check ${{ matrix.extra }}\n",
        );
        let evidence = CiCommands::read(dir.path());
        assert!(!evidence
            .invocation_of(CLIPPY)
            .unwrap()
            .is_plainly_runnable(CLIPPY));
        assert!(!evidence
            .invocation_of(FMT)
            .unwrap()
            .is_plainly_runnable(FMT));
    }

    #[test]
    fn a_step_name_that_merely_mentions_a_tool_is_not_evidence() {
        // `- name: cargo clippy` labels a step; the real invocation is the
        // `run:` below it. A reader that matched any line would call a
        // workflow that only *documents* a tool evidence that it runs one.
        let dir = tempfile::tempdir().unwrap();
        workflow(
            dir.path(),
            "ci.yml",
            "steps:\n  - name: cargo clippy\n    uses: some/action@v1\n",
        );
        assert_eq!(command_of(&CiCommands::read(dir.path()), CLIPPY), None);
    }

    #[test]
    fn a_commented_out_run_line_is_not_evidence() {
        let dir = tempfile::tempdir().unwrap();
        workflow(
            dir.path(),
            "ci.yml",
            "steps:\n  # - run: cargo clippy --workspace\n  - run: cargo test\n",
        );
        let evidence = CiCommands::read(dir.path());
        assert_eq!(command_of(&evidence, CLIPPY), None);
        assert_eq!(
            command_of(&evidence, &["cargo", "test"]),
            Some("cargo test")
        );
    }

    #[test]
    fn a_trailing_comment_is_stripped_from_the_command() {
        let dir = tempfile::tempdir().unwrap();
        workflow(
            dir.path(),
            "ci.yml",
            "steps:\n  - run: cargo fmt --all -- --check # keep the tree formatted\n",
        );
        assert_eq!(
            command_of(&CiCommands::read(dir.path()), FMT),
            Some("cargo fmt --all -- --check")
        );
    }

    #[test]
    fn finds_a_tool_invoked_after_a_shell_separator() {
        let dir = tempfile::tempdir().unwrap();
        workflow(
            dir.path(),
            "ci.yml",
            "steps:\n  - run: cargo build && cargo clippy --all-targets\n",
        );
        let evidence = CiCommands::read(dir.path());
        let invocation = evidence.invocation_of(CLIPPY).unwrap();
        assert_eq!(invocation.command, "cargo clippy --all-targets");
        assert!(invocation.is_plainly_runnable(CLIPPY));
    }

    #[test]
    fn a_tool_the_workflow_never_runs_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        workflow(dir.path(), "ci.yml", "steps:\n  - run: cargo test\n");
        assert_eq!(command_of(&CiCommands::read(dir.path()), CLIPPY), None);
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
        // Missing is silent; present-and-unreadable must not be, or the
        // human approves a gate with no idea CI was never read.
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
    fn reads_every_workflow_in_file_name_order() {
        // Fixed order so the same checkout always produces the same
        // evidence, and therefore the same proposed gate.
        let dir = tempfile::tempdir().unwrap();
        workflow(dir.path(), "z-lint.yaml", "steps:\n  - run: cargo clippy\n");
        workflow(dir.path(), "a-test.yml", "steps:\n  - run: cargo test\n");
        let evidence = CiCommands::read(dir.path());
        let commands: Vec<&str> = evidence
            .invocations
            .iter()
            .map(|invocation| invocation.command.as_str())
            .collect();
        assert_eq!(commands, vec!["cargo test", "cargo clippy"]);
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
        // Silence here is the defect this module exists to close: a skipped
        // workflow is exactly how a gate ends up narrower than CI.
        let dir = tempfile::tempdir().unwrap();
        workflow(
            dir.path(),
            "ci.yml",
            &format!(
                "steps:\n  - run: cargo clippy\n# {}\n",
                "x".repeat(MAX_WORKFLOW_BYTES as usize)
            ),
        );
        let evidence = CiCommands::read(dir.path());
        assert_eq!(command_of(&evidence, CLIPPY), None);
        assert_eq!(evidence.warnings.len(), 1);
        assert!(
            evidence.warnings[0].contains("ci.yml"),
            "expected the warning to name the workflow, got: {:?}",
            evidence.warnings
        );
    }
}

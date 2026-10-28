//! Per-issue git worktree provisioning (build-ladder rung 4).
//!
//! The spec's *Data flow* puts "create git worktree for this issue" between
//! risk classification and the first IC dispatch, and its error table gives
//! this module two hard requirements:
//!
//! - *"Two ICs on the same repo — each has its own git worktree; no shared
//!   checkout, no interference."* Hence one worktree per **issue**, on its
//!   own branch, keyed by [`super::IssueRef::slug`].
//! - *"Worktree left dirty by a dead IC — Lead quarantines it and creates a
//!   fresh one rather than reusing dirty state."* Hence [`ensure_worktree`]
//!   never hands back a checkout with uncommitted changes.
//!
//! Rung 2 deliberately stopped short of this: `dispatch::run_dispatch` takes
//! an already-resolved `repo: &Path` and documents that "the caller (a later
//! rung's worktree-management code) is expected to have already created and
//! resolved the IC's git worktree before dispatching into it". This is that
//! code.
//!
//! # Why shell out to `git` rather than link a git library
//!
//! Every other git interaction in this crate (`build.rs`, the review-diff
//! path, the collab checkpoint tooling) shells out to the `git` binary, and
//! worktree management in particular is a place where matching the user's
//! own git — its version, its config, its hooks, its `safe.directory`
//! rules — matters more than avoiding a subprocess. A linked library would
//! reimplement worktree registration against a different set of assumptions
//! than the `git worktree list` a human debugging this will run by hand.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::MemoryError;

use super::IssueRef;

/// Branch prefix for every worktree this module creates. Distinct enough
/// that a human scanning `git branch` can tell Autopilot's branches from
/// their own, and stable enough that [`branch_name`] is a pure function of
/// the issue — a resumed run must land on the same branch its earlier
/// dispatches pushed to.
pub const BRANCH_PREFIX: &str = "autopilot";

/// Marker infix used when a dirty worktree is moved aside. Includes a
/// timestamp so quarantining twice never collides.
const QUARANTINE_INFIX: &str = "quarantine";

/// Resolve `rev` to a full commit SHA inside `repo_dir`.
///
/// Returns `None` rather than an error — the signature has no error channel
/// at all — when the revision does not exist,
/// because every caller of this is answering "which commit did we review?"
/// and the answer "we could not tell" is a legitimate one that rung 6 fails
/// closed on ([`super::review::RecordedReviewSummary::head_sha`]). A hard
/// error here would instead abort a review that was otherwise about to
/// produce a usable verdict.
pub fn resolve_commit(repo_dir: &Path, rev: &str) -> Option<String> {
    let sha = git(
        repo_dir,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )
    .ok()?;
    let sha = sha.trim();
    // `rev-parse --verify` prints exactly one full object name on success: 40
    // hex characters, or 64 in a SHA-256 repository. Anything else means we
    // are not looking at what we think we are, and a half-parsed SHA compared
    // against a PR head would silently never match.
    if matches!(sha.len(), 40 | 64) && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(sha.to_string())
    } else {
        None
    }
}

/// The branch an issue's worktree checks out: `autopilot/<repo-slug>-<n>`.
///
/// Uses [`IssueRef::slug`] (the full `owner-repo-number` form) rather than a
/// bare issue number for the same reason [`super::dispatch::ic_name`] does:
/// two repos with the same short name must never collide.
pub fn branch_name(issue: &IssueRef) -> String {
    format!("{BRANCH_PREFIX}/{}", issue.slug())
}

/// Whether a remote `url` names the GitHub repository `repo` (`owner/name`),
/// compared without regard to case. The forms GitHub hands out:
/// `https://github.com/O/N`, `ssh://git@github.com/O/N` and the scp-like
/// `git@github.com:O/N`, each with or without `.git` (and a trailing `/`).
/// Any userinfo (`user@`, or `user:token@`) and any port (`:22`) is not part
/// of the host. Any other host, scheme or form names no repository here; an
/// SSH host alias (`git@github-work:O/N`) is one, so it falls through to the
/// next rule of [`pick_push_remote`].
pub(crate) fn url_names_github_repo(url: &str, repo: &str) -> bool {
    github_repo_of(url).is_some_and(|named| named == repo.to_ascii_lowercase())
}

/// The `owner/name` (lowercased) a remote `url` names on GitHub, in the
/// forms [`url_names_github_repo`] reads; `None` for any URL it cannot read
/// as one (another host, an SSH alias, a local path).
fn github_repo_of(url: &str) -> Option<String> {
    let url = url.trim().to_ascii_lowercase();
    let without_user = |authority: &str| -> String {
        authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host)
            .to_string()
    };
    let (host, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            if !matches!(scheme, "https" | "ssh") {
                return None;
            }
            let (authority, path) = rest.split_once('/')?;
            let host = without_user(authority);
            let host = host
                .split_once(':')
                .map_or(host.as_str(), |(host, _)| host)
                .to_string();
            (host, path.to_string())
        }
        None => {
            // scp-like: `[user@]host:path`; a local path has no `host:`.
            let (authority, path) = url.split_once(':')?;
            if authority.contains('/') || path.starts_with('/') {
                return None;
            }
            (without_user(authority), path.to_string())
        }
    };
    if host != "github.com" {
        return None;
    }
    let path = path.strip_suffix('/').unwrap_or(&path);
    let path = path.strip_suffix(".git").unwrap_or(path);
    match path.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
            Some(path.to_string())
        }
        _ => None,
    }
}

/// Whether every one of `urls` is a GitHub URL for a repository other than
/// `repo`: a push there provably goes elsewhere. No URLs, or any this cannot
/// read as GitHub's, is not proof.
fn pushes_elsewhere(urls: &[&str], repo: &str) -> bool {
    let repo = repo.to_ascii_lowercase();
    !urls.is_empty()
        && urls
            .iter()
            .all(|url| github_repo_of(url).is_some_and(|named| named != repo))
}

/// The remote a push of an issue branch names, from the configured
/// `remotes` (each name with its effective push URLs, [`remote_push_urls`]),
/// those already `tracking` the branch (having
/// `refs/remotes/<remote>/<branch>`), and the issue's `repo`:
///
/// 1. the remote whose effective push URL names `repo`
///    ([`url_names_github_repo`]), so the push lands where
///    `gh pr create --head` looks for the branch (`origin` first when
///    several do); a fetch URL naming it is not enough;
/// 2. else one already tracking the branch (`origin` first);
/// 3. else `origin`, only when it is configured;
/// 4. else the only remaining one;
/// 5. else none: no remote is named rather than one that may not exist.
///
/// Steps 2 to 4 skip a remote whose every push URL is a GitHub URL for
/// another repository ([`pushes_elsewhere`]): a push there provably misses
/// the issue's repo, and `git push` to a fork still creates its
/// `refs/remotes/<remote>/<branch>`, so tracking proves nothing. A push URL
/// that cannot be read as GitHub's (an SSH alias, a GitHub Enterprise host,
/// a local path) keeps its remote eligible. With no `repo`, none is skipped.
///
/// The issue branch has no upstream, so the goal condition's push names
/// its remote when one is known.
pub(crate) fn pick_push_remote<'a>(
    remotes: &[(&'a str, &[&str])],
    tracking: &[&'a str],
    repo: Option<&str>,
) -> Option<&'a str> {
    let origin_first = |names: Vec<&'a str>| {
        names
            .iter()
            .copied()
            .find(|name| *name == "origin")
            .or_else(|| names.first().copied())
    };
    let naming_repo = repo.map_or_else(Vec::new, |repo| {
        remotes
            .iter()
            .filter(|(_, urls)| urls.iter().any(|url| url_names_github_repo(url, repo)))
            .map(|(name, _)| *name)
            .collect()
    });
    let eligible: Vec<&'a str> = remotes
        .iter()
        .filter(|(_, urls)| !repo.is_some_and(|repo| pushes_elsewhere(urls, repo)))
        .map(|(name, _)| *name)
        .collect();
    origin_first(naming_repo)
        .or_else(|| {
            origin_first(
                tracking
                    .iter()
                    .copied()
                    .filter(|name| eligible.contains(name))
                    .collect(),
            )
        })
        .or_else(|| eligible.iter().copied().find(|name| *name == "origin"))
        .or_else(|| (eligible.len() == 1).then(|| eligible[0]))
}

/// The remotes that push to another GitHub repository
/// ([`pushes_elsewhere`]), when no remote's push URL names `repo`; empty
/// otherwise. Names only: the URLs may hold a token.
pub(crate) fn remotes_pushing_elsewhere<'a>(
    remotes: &[(&'a str, &[&str])],
    repo: &str,
) -> Vec<&'a str> {
    if remotes
        .iter()
        .any(|(_, urls)| urls.iter().any(|url| url_names_github_repo(url, repo)))
    {
        return Vec::new();
    }
    remotes
        .iter()
        .filter(|(_, urls)| pushes_elsewhere(urls, repo))
        .map(|(name, _)| *name)
        .collect()
}

/// Warn, at run start, when no remote of the checkout at `dir` pushes to
/// `repo` while some push to another GitHub repository: the IC's push may
/// then miss the repository its pull request is opened in. It names the
/// remotes, never their URLs, which may hold a token.
pub(crate) fn warn_if_no_remote_pushes_to(dir: &Path, repo: &str) {
    let listed = git_or_warn(
        dir,
        &["remote", "-v"],
        "so whether one pushes to the issue's repository is not checked",
    );
    let remotes = remote_push_urls(&listed);
    let remotes: Vec<(&str, &[&str])> = remotes
        .iter()
        .map(|(name, urls)| (*name, urls.as_slice()))
        .collect();
    let elsewhere = remotes_pushing_elsewhere(&remotes, repo);
    if !elsewhere.is_empty() {
        tracing::warn!(
            repo = %repo,
            remotes = %elsewhere.join(", "),
            "no git remote's push URL names the issue's repository, and these push to \
             another GitHub repository; the implementer's push may miss the repository \
             its pull request is opened in"
        );
    }
}

/// Each remote in `git remote -v` output, in order, with its effective push
/// URLs: the URLs `git push <remote>` sends to. Those are its `pushurl`s
/// when it has any, else its `url`, after any `pushInsteadOf` or
/// `insteadOf` rewrite; the "(push)" lines show exactly those (measured, git
/// 2.55). Fetch URLs are ignored: a remote that fetches the issue's repo
/// but pushes to a fork must not be taken for the issue's repo.
pub(crate) fn remote_push_urls(listing: &str) -> Vec<(&str, Vec<&str>)> {
    let mut remotes: Vec<(&str, Vec<&str>)> = Vec::new();
    for line in listing.lines() {
        let Some((name, rest)) = line.split_once('\t') else {
            continue;
        };
        let push = rest.strip_suffix(" (push)");
        let urls = match remotes.iter_mut().find(|(known, _)| *known == name) {
            Some((_, urls)) => urls,
            None => {
                remotes.push((name, Vec::new()));
                &mut remotes.last_mut().expect("just pushed").1
            }
        };
        urls.extend(push);
    }
    remotes
}

/// The trimmed stdout of git `args` in `dir`, or, when git fails, nothing,
/// with a warning that names the command, git's complaint (scrubbed and
/// bounded, [`bounded_stderr`]) and `consequence`. For the remote reads
/// below, where an answer that cannot be read degrades to none rather than
/// stopping the run, but must not pass for an empty list unseen.
fn git_or_warn(dir: &Path, args: &[&str], consequence: &str) -> String {
    git(dir, args).unwrap_or_else(|e| {
        tracing::warn!(
            command = %format!("git {}", args.join(" ")),
            error = %e,
            "autopilot could not read the checkout's remotes, {consequence}"
        );
        String::new()
    })
}

/// [`pick_push_remote`] for `branch` of `repo` in the repository at `dir`,
/// from each remote's effective push URLs ([`remote_push_urls`]). A remote
/// list that cannot be read counts as none, with a warning
/// ([`git_or_warn`]). The URLs, which may hold a token, are never logged.
pub(crate) fn push_remote(dir: &Path, branch: &str, repo: &str) -> Option<String> {
    let listed = git_or_warn(
        dir,
        &["remote", "-v"],
        "so the implementer's push names no remote",
    );
    let remotes = remote_push_urls(&listed);
    let remotes: Vec<(&str, &[&str])> = remotes
        .iter()
        .map(|(name, urls)| (*name, urls.as_slice()))
        .collect();
    if remotes.is_empty() {
        return None;
    }
    let refs = git_or_warn(
        dir,
        &["for-each-ref", "--format=%(refname)", "refs/remotes/"],
        "so no remote counts as already tracking the issue branch",
    );
    let refs: std::collections::HashSet<&str> = refs.lines().collect();
    let tracking: Vec<&str> = remotes
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| refs.contains(format!("refs/remotes/{name}/{branch}").as_str()))
        .collect();
    pick_push_remote(&remotes, &tracking, Some(repo)).map(str::to_string)
}

/// Where an issue's worktree lives under `worktree_root`.
pub fn worktree_path(worktree_root: &Path, issue: &IssueRef) -> PathBuf {
    worktree_root.join(issue.slug())
}

/// A full commit object name: 40 hex digits, or 64 in a SHA-256 repository.
///
/// Checked when it is made, so a value that git would read as an option or a
/// revision expression (`--all`, `HEAD~3`, an abbreviation) can never stand in
/// for a commit, for example on the right of a `git log --not`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommitSha(String);

impl CommitSha {
    /// `text` as a commit name, or a validation error quoting it (bounded).
    pub fn parse(text: &str) -> Result<Self, MemoryError> {
        if matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(Self(text.to_string()));
        }
        Err(MemoryError::Validation(format!(
            "{:?} is not a full commit object name (40 or 64 hex digits)",
            text.chars().take(80).collect::<String>()
        )))
    }

    /// Any text, unchecked, for tests of what refuses a bad one.
    #[cfg(test)]
    pub(crate) fn unchecked(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for CommitSha {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for CommitSha {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<std::ffi::OsStr> for CommitSha {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_ref()
    }
}

impl std::fmt::Display for CommitSha {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<String> for CommitSha {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

impl PartialEq<str> for CommitSha {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

/// A provisioned worktree, ready to dispatch an IC into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Absolute path to the checkout.
    pub path: PathBuf,
    /// The branch checked out there.
    pub branch: String,
    /// Whether this call created the checkout (`false` means a clean
    /// existing worktree was reused, which is the normal resumed-dispatch
    /// case).
    pub created: bool,
    /// The full commit sha the branch was cut from. Resolved from `base` in
    /// the operator's checkout once, before the branch is created, and kept
    /// in `branch.<branch>.ironmemCut` so a resume reads it back rather than
    /// re-resolving a `base` (such as `HEAD`) that has since moved.
    pub cut: CommitSha,
    /// Set when a dirty pre-existing worktree was moved aside first — the
    /// path it was moved to, kept for a human to inspect. The spec requires
    /// quarantine rather than deletion precisely so the dead IC's work is
    /// still recoverable.
    pub quarantined_from: Option<PathBuf>,
}

/// A `git` command with **every inherited `GIT_*` variable stripped**.
///
/// The single constructor for this module's git calls, so the scrub cannot be
/// forgotten at one call site — the shape `collab_checkpoint`'s own helper
/// uses, for the same reason.
///
/// `current_dir` is not enough on its own, and this module is the sharpest
/// case of that in the crate. An inherited `GIT_DIR` / `GIT_WORK_TREE`
/// overrides the working directory, so a `git worktree remove` aimed at an
/// issue's checkout would run against **whatever repository the environment
/// names instead** — a destructive command pointed at a repo nobody in this
/// subsystem chose. `collab_session`'s scrub states that every
/// `Command::new("git")` in the crate must call it; autopilot was the module
/// that never did.
///
/// `core.fsmonitor` is pinned off: it names a program, and the shared
/// `.git/config` that sets it is writable by the unconfined agent, so `git
/// status` here would run whatever the agent put there, in the controller.
/// Hooks are left on (git-lfs needs `post-checkout`); every command is
/// bounded in time instead ([`run_git_output`]).
fn git_command() -> Command {
    let mut command = Command::new("git");
    crate::mcp::tools::scrub_git_environment(&mut command);
    command.args(["-c", "core.fsmonitor=false"]);
    command
}

/// Run a `git` subcommand in `cwd` through [`git_command`]'s scrubbed
/// environment, returning trimmed stdout or a descriptive error.
fn git(cwd: &Path, args: &[&str]) -> Result<String, MemoryError> {
    run_git(git_command(), cwd, args)
}

/// [`git_command`] for a `git config` write, in the C locale: [`config_locked`]
/// recognises a held lock by git's message, which is translated in any other
/// locale. `LANGUAGE` goes too: GNU gettext prefers it over `LC_ALL` outside
/// the C locale.
fn config_write_command() -> Command {
    let mut command = git_command();
    command
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env_remove("LANGUAGE");
    command
}

/// [`git`] for a `git config` write; see [`config_write_command`].
fn git_config_write(cwd: &Path, args: &[&str]) -> Result<String, MemoryError> {
    run_git(config_write_command(), cwd, args)
}

fn run_git(command: Command, cwd: &Path, args: &[&str]) -> Result<String, MemoryError> {
    let out = run_git_output(command, cwd, args)?;
    if out.code != Some(0) {
        return Err(out.failure(cwd, args));
    }
    Ok(out.stdout)
}

/// A finished git command: its exit code and trimmed output, for a caller
/// that reads some non-zero exits as answers rather than failures.
struct GitOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl GitOutput {
    /// The error [`git`] reports for this output.
    fn failure(&self, cwd: &Path, args: &[&str]) -> MemoryError {
        MemoryError::Validation(format!(
            "git {} failed in {} (exit {:?}): {}",
            args.join(" "),
            cwd.display(),
            self.code,
            self.stderr
        ))
    }
}

/// How long one git command may run before it is killed with its process
/// group: long enough for `git worktree add` to check out a large
/// repository, and a bound on a hook or filter (which the agent can install
/// in the shared repository) that never returns, so it cannot stall the
/// Lead for good.
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The longest wait between two checks on a running git; the first checks
/// come sooner, so a quick command is not held back.
const GIT_POLL_MAX: std::time::Duration = std::time::Duration::from_millis(20);

/// Run `command` with `args` in `cwd` within [`GIT_TIMEOUT`]; see
/// [`run_git_output_within`].
fn run_git_output(command: Command, cwd: &Path, args: &[&str]) -> Result<GitOutput, MemoryError> {
    run_git_output_within(command, cwd, args, GIT_TIMEOUT)
}

/// Run `command` with `args` in `cwd`. Any exit is an answer; a failure to
/// spawn, a git still running at `timeout`, or one whose output is still held
/// open then (by a process it started), is an error.
///
/// As the signing guard's git does, the command runs in its own process
/// group, and at the deadline the whole group is killed: a hook's
/// background job dies with it rather than running on outside any bound.
/// git is reaped only once it has exited and its pipes are closed, so its
/// group id cannot name someone else's group when it is killed
/// ([`super::dispatch::exited_unreaped`]).
fn run_git_output_within(
    mut command: Command,
    cwd: &Path,
    args: &[&str],
    timeout: std::time::Duration,
) -> Result<GitOutput, MemoryError> {
    use std::process::Stdio;
    let label = || format!("git {} in {}", args.join(" "), cwd.display());
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = super::dispatch::retry_on_etxtbsy(|| command.spawn())
        .map_err(|e| MemoryError::NotFound(format!("failed to run git {}: {e}", args.join(" "))))?;
    let stdout = read_pipe(child.stdout.take());
    let stderr = read_pipe(child.stderr.take());
    let deadline = std::time::Instant::now() + timeout;
    let mut poll = std::time::Duration::from_millis(1);
    loop {
        let exited = super::dispatch::exited_unreaped(&mut child);
        let closed = stdout.is_finished() && stderr.is_finished();
        match exited {
            Ok(true) if closed => break,
            Ok(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(poll);
                poll = (poll * 2).min(GIT_POLL_MAX);
            }
            waited => {
                super::dispatch::kill_process_group(&mut child);
                let _ = child.wait();
                let secs = timeout.as_secs_f64();
                return Err(MemoryError::Validation(match waited {
                    Err(e) => format!("{} could not be waited on: {e}", label()),
                    Ok(true) => format!(
                        "{} exited but left its output open past {secs}s, held by a process it                          started, and was killed with it",
                        label()
                    ),
                    Ok(false) => format!(
                        "{} did not finish within {secs}s and was killed",
                        label()
                    ),
                }));
            }
        }
    }
    let status = child
        .wait()
        .map_err(|e| MemoryError::Validation(format!("{} could not be waited on: {e}", label())))?;
    let read = |pipe: PipeReader, name: &str| -> Result<String, MemoryError> {
        let bytes = pipe
            .join()
            .map_err(|_| std::io::Error::other("its reader panicked"))
            .and_then(|read| read)
            .map_err(|e| {
                MemoryError::Validation(format!("{}: its {name} could not be read: {e}", label()))
            })?;
        Ok(String::from_utf8_lossy(&bytes).trim().to_string())
    };
    Ok(GitOutput {
        code: status.code(),
        stdout: read(stdout, "stdout")?,
        stderr: read(stderr, "stderr")?,
    })
}

/// A thread reading one of a git command's pipes to its end.
type PipeReader = std::thread::JoinHandle<std::io::Result<Vec<u8>>>;

/// Read `pipe` to its end on its own thread, so a full pipe cannot stall the
/// git [`run_git_output_within`] is polling for exit.
fn read_pipe<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> PipeReader {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            pipe.read_to_end(&mut buf)?;
        }
        Ok(buf)
    })
}

/// Like [`git`], but reports whether the command succeeded instead of
/// treating a non-zero exit as an error. Used for the `rev-parse --verify`
/// existence probes, where "this ref does not exist" is a normal answer and
/// not a fault.
fn git_ok(cwd: &Path, args: &[&str]) -> Result<bool, MemoryError> {
    Ok(run_git_output(git_command(), cwd, args)?.code == Some(0))
}

/// `git config --get key` in `cwd`: `None` when the key is unset (git exits
/// 1), and an error for any other failure, which is not a reading at all.
fn config_get(cwd: &Path, args: &[&str]) -> Result<Option<String>, String> {
    let args: Vec<&str> = std::iter::once("config")
        .chain(args.iter().copied())
        .collect();
    let out = run_git_output(git_command(), cwd, &args).map_err(|e| e.to_string())?;
    match out.code {
        Some(0) => Ok(Some(out.stdout)),
        Some(1) => Ok(None),
        _ => Err(bounded_stderr(out.stderr.as_bytes())),
    }
}

/// How many characters of a git command's stderr a one-line reason quotes.
const STDERR_EXCERPT_CHARS: usize = 300;

/// `git`'s own complaint, trimmed and bounded so it can ride inside a
/// one-line reason.
pub(crate) fn bounded_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .trim()
        .chars()
        .take(STDERR_EXCERPT_CHARS)
        .collect()
}

fn cut_key(branch: &str) -> String {
    format!("branch.{branch}.ironmemCut")
}

/// The token of the controller that recorded `branch`'s cut while creating
/// it ([`create_branch`]), so a take-back removes only a cut it wrote
/// itself: two controllers can record the same value.
fn cut_owner_key(branch: &str) -> String {
    format!("branch.{branch}.ironmemCutOwner")
}

fn signing_key(branch: &str) -> String {
    format!("branch.{branch}.ironmemSigning")
}

/// The effective `commit.gpgsign` in `dir`. `Err` carries git's bounded
/// stderr when the value cannot be read: git exits 1 for "unset" (off), so any
/// other failure (an invalid value exits 128) is not a reading at all.
pub(crate) fn signing_policy(dir: &Path) -> Result<bool, String> {
    config_get(dir, &["--bool", "--get", "commit.gpgsign"])
        .map(|value| value.is_some_and(|v| v == "true"))
}

/// The repository-wide signing flag, in the common `.git/config`.
///
/// `commit.gpgsign` is read from that shared config, which an agent in any
/// sibling worktree can change. A flag kept only per branch would miss every
/// branch created after such a change: it reads the policy as off and records
/// nothing. So the first time any run sees the policy on, the repository
/// remembers it too, and every issue branch in it is held to it.
const REPO_SIGNING_KEY: &str = "ironmem.autopilotSigning";

/// A `--bool` config value: `None` when unset, `Err` when unreadable.
fn config_bool(dir: &Path, key: &str) -> Result<Option<bool>, String> {
    config_get(dir, &["--bool", "--get", key]).map(|value| value.map(|v| v == "true"))
}

/// Remember on `branch` that the signing policy is on, and, when
/// `repo_wide`, on the repository too ([`REPO_SIGNING_KEY`]). The caller has
/// read the policy as on; it passes `repo_wide` only for a policy that read
/// `true`, so an unreadable `commit.gpgsign` (a typo) holds its own branch
/// but never arms every branch in the repository. The flags are sticky: a
/// later run that finds the policy off still enforces it, so an agent that
/// sets `commit.gpgsign false` in the shared `.git/config` cannot switch
/// enforcement off for this dispatch or any later run, nor for branches
/// created after it.
///
/// Each flag is skipped when already recorded. A live agent in another
/// worktree of the same repo may hold `.git/config`'s lock (`push -u` and
/// `checkout -b` both write config), and a needless write would race it.
///
/// A flag that cannot be read is skipped too, with a warning naming it, and
/// left as it is: it may be an operator's mistyped opt-out, which is theirs
/// to correct. It still holds the branch ([`signing_holder`] refuses to read
/// it as "not held"), so nothing is unguarded meanwhile.
pub(crate) fn record_signing(dir: &Path, branch: &str, repo_wide: bool) -> Result<(), MemoryError> {
    // The branch first: a held lock on the second write still leaves the
    // branch held, as before the repository flag existed.
    let mut keys = vec![signing_key(branch)];
    if repo_wide {
        keys.push(REPO_SIGNING_KEY.to_string());
    }
    for key in keys {
        match config_bool(dir, &key) {
            Ok(Some(true)) => continue,
            Err(why) => {
                tracing::warn!(
                    key = %key,
                    error = %why,
                    "autopilot cannot read a commit-signing flag, so it is left as it is and \
                     still holds the branch; set it to true or false"
                );
                continue;
            }
            Ok(_) => {}
        }
        git_config_write(dir, &["config", &key, "true"])?;
    }
    Ok(())
}

/// Whether a branch is held to the signing policy that was once seen on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SigningHold {
    /// Held, by the flag named, for a reason the model and operator read.
    Held(String),
    /// An operator set `branch.<branch>.ironmemSigning false`: this branch
    /// is opted out, and only the policy a still-running run saw at its own
    /// start holds it.
    OptedOut,
    /// No flag holds it.
    NotHeld,
}

/// Whether `branch` is held to the signing policy that was once seen on.
///
/// The branch's own `branch.<branch>.ironmemSigning` decides when set: `true`
/// holds it, and an operator's explicit `false` opts that one branch out.
/// Unset, the repository's [`REPO_SIGNING_KEY`] decides. A value that cannot
/// be read is `Err`, with git's complaint: it is not the model's fault, and
/// the caller must never read it as "not held".
pub(crate) fn signing_holder(dir: &Path, branch: &str) -> Result<SigningHold, String> {
    let branch_key = signing_key(branch);
    let unreadable = |key: &str, why: String| format!("`{key}` could not be read: {why}");
    match config_bool(dir, &branch_key) {
        Ok(Some(true)) => Ok(SigningHold::Held(format!("`{branch_key}`"))),
        Ok(Some(false)) => Ok(SigningHold::OptedOut),
        Ok(None) => match config_bool(dir, REPO_SIGNING_KEY) {
            Ok(Some(true)) => Ok(SigningHold::Held(format!(
                "`{REPO_SIGNING_KEY}` (the repository recorded signing on)"
            ))),
            Ok(_) => Ok(SigningHold::NotHeld),
            Err(why) => Err(unreadable(REPO_SIGNING_KEY, why)),
        },
        Err(why) => Err(unreadable(&branch_key, why)),
    }
}

/// Whether `branch` is held to the signing policy; see [`signing_holder`]. A
/// flag that cannot be read counts: these flags only ever tighten.
#[cfg(test)]
pub(crate) fn signing_recorded(dir: &Path, branch: &str) -> bool {
    matches!(
        signing_holder(dir, branch),
        Ok(SigningHold::Held(_)) | Err(_)
    )
}

/// The cut recorded on an existing `branch`, if any, resolved to a full sha.
/// A recorded value that is no longer a commit is an error rather than a
/// silent re-resolve, which would move the cut.
fn recorded_cut(root: &Path, branch: &str) -> Result<Option<String>, MemoryError> {
    let key = cut_key(branch);
    let recorded = config_get(root, &["--get", &key]).map_err(|why| {
        MemoryError::Validation(format!("cannot read {key} in {}: {why}", root.display()))
    })?;
    let Some(recorded) = recorded else {
        return Ok(None);
    };
    resolve_commit(root, &recorded).map(Some).ok_or_else(|| {
        MemoryError::Validation(format!(
            "{key} is `{}`, which is not a commit in {}; unset it with `git config --unset \
             {key}` to record the cut again from the branch's history",
            quoted_value(&recorded),
            root.display()
        ))
    })
}

/// How many characters of a config value an error quotes.
const QUOTED_VALUE_CHARS: usize = 80;

/// A config value fit to quote in a one-line error: anyone who can write
/// `.git/config` (the agent included) chooses it, so control characters are
/// escaped, secrets scrubbed and the length bounded.
fn quoted_value(value: &str) -> String {
    let escaped: String = value.escape_debug().collect();
    let bounded = super::scrub::scrub_and_bound(&escaped, QUOTED_VALUE_CHARS);
    if bounded.truncated {
        format!("{}…", bounded.text)
    } else {
        bounded.text
    }
}

/// How many times [`write_cut`] tries while `.git/config` is locked, and how
/// long it waits between tries.
const CONFIG_LOCK_ATTEMPTS: u32 = 5;
const CONFIG_LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

/// Record `cut` on `branch`, waiting briefly while another worktree's agent
/// holds `.git/config`'s lock; see [`write_config_waiting`].
fn write_cut(root: &Path, branch: &str, cut: &str) -> Result<(), MemoryError> {
    write_config_waiting(root, &cut_key(branch), cut, |_| {})
}

/// Set `key` to `value`, retrying while `.git/config` is locked. A sibling
/// worktree's agent holds the lock only for a moment (`push -u` and
/// `checkout -b` write config), so a short bounded wait is worth it. Any
/// other failure is returned at once; a lock held past the window is returned
/// naming the lock file ([`lock_held`]). `before_retry` is called with the
/// failed attempt's number before each retry.
fn write_config_waiting(
    root: &Path,
    key: &str,
    value: &str,
    before_retry: impl FnMut(u32),
) -> Result<(), MemoryError> {
    config_waiting(root, &["config", key, value], &[], before_retry)
}

/// Run the `git config` write `args`, retrying while `.git/config` is locked;
/// see [`write_config_waiting`]. An exit code in `answers` is success.
fn config_waiting(
    root: &Path,
    args: &[&str],
    answers: &[i32],
    mut before_retry: impl FnMut(u32),
) -> Result<(), MemoryError> {
    let mut attempt = 1;
    loop {
        let out = run_git_output(config_write_command(), root, args)?;
        if out
            .code
            .is_some_and(|code| code == 0 || answers.contains(&code))
        {
            return Ok(());
        }
        let e = out.failure(root, args);
        if !config_locked(&e) {
            return Err(e);
        }
        if attempt >= CONFIG_LOCK_ATTEMPTS {
            return Err(lock_held(root, e));
        }
        before_retry(attempt);
        attempt += 1;
        std::thread::sleep(CONFIG_LOCK_RETRY);
    }
}

/// A config write's lock error, extended with the lock file's path and age.
///
/// A git killed while holding the lock (the supervisor's cleanup SIGKILLs an
/// agent's `git push -u`, and SIGKILL skips git's lockfile cleanup) leaves it
/// behind, and every later config write fails until someone removes it. The
/// lock is never removed here: a live git may still hold it, and only a
/// person can tell.
fn lock_held(root: &Path, e: MemoryError) -> MemoryError {
    let lock = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map(|dir| canonical_or_self(Path::new(&dir)).join("config.lock"));
    let Ok(lock) = lock else {
        return e;
    };
    let detail = match e {
        MemoryError::Validation(detail) => detail,
        other => other.to_string(),
    };
    let modified = match std::fs::metadata(&lock).and_then(|meta| meta.modified()) {
        Ok(modified) => modified,
        // Gone by now: whoever held it let go after the retries ran out.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return MemoryError::Validation(format!(
                "{detail}; `{}` was released after the retries ran out, so nothing is stuck: \
                 run again",
                lock.display()
            ))
        }
        Err(_) => std::time::SystemTime::now(),
    };
    let age = modified.elapsed().map_or(0, |age| age.as_secs());
    MemoryError::Validation(format!(
        "{detail}; `{}` has been held for {age}s. If no git command is running in any worktree \
         of {}, a killed git left it behind: remove it and run again",
        lock.display(),
        root.display()
    ))
}

/// Remove `branch`'s recorded cut only while it still reads `cut`, so a value
/// another controller wrote meanwhile is kept, waiting out a held lock like
/// [`write_cut`]. Nothing to remove is not an error (git exits 5). Only for a
/// branch that does not exist: an existing branch is never left without one.
fn unset_cut_if(root: &Path, branch: &str, cut: &str) -> Result<(), MemoryError> {
    unset_cut_waiting(root, branch, cut, |_| {})
}

/// [`unset_cut_if`], calling `before_retry` as [`write_config_waiting`] does.
fn unset_cut_waiting(
    root: &Path,
    branch: &str,
    cut: &str,
    before_retry: impl FnMut(u32),
) -> Result<(), MemoryError> {
    let key = cut_key(branch);
    // A full object name is plain hex, so the anchored pattern matches it alone.
    let pattern = format!("^{cut}$");
    config_waiting(
        root,
        &["config", "--unset", &key, &pattern],
        &[5],
        before_retry,
    )
}

/// The tip of `refs/heads/<branch>`, or `None` when there is no such branch.
fn branch_tip(root: &Path, branch: &str) -> Result<Option<String>, MemoryError> {
    let local = format!("refs/heads/{branch}");
    let args = [
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{local}^{{commit}}"),
    ];
    let out = run_git_output(git_command(), root, &args)?;
    match out.code {
        Some(0) => Ok(resolve_commit(root, &out.stdout)),
        Some(1) => Ok(None),
        _ => Err(out.failure(root, &args)),
    }
}

/// Where an inferred cut came from, named in [`backfill_cut`]'s warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CutSource {
    /// The branch reflog's creation entry: the exact commit it was cut from.
    Reflog,
    /// The merge base of the branch and the operator's base: an ancestor of
    /// the branch. It skips the branch's own work only once that work is on
    /// the base, which [`infer_cut`] refuses while it is on no remote.
    MergeBase,
}

impl std::fmt::Display for CutSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CutSource::Reflog => "the branch's creation entry in its reflog",
            CutSource::MergeBase => "the merge base of the branch and the base",
        })
    }
}

/// The cut of an existing `branch` that has none recorded.
///
/// Its reflog's creation entry (`branch: Created from ...`, which git writes
/// untranslated) is the commit it was cut from, so that is preferred, but
/// only while that commit is on `base` (an ancestor of it, or `base` itself).
/// `git branch -D` takes the branch's reflog and its recorded cut with it, so
/// a branch made again (`git checkout -B <branch> <its last commit>`, as the
/// signing guard's own missing-branch advice says) has a creation entry at
/// the agent's own work. Taken as the cut, that would move the signing
/// guard's range past every commit before it.
///
/// Otherwise (no reflog, as with `core.logAllRefUpdates` off or an expired
/// entry, or a creation entry off the base), the merge base of the branch and
/// `base`: never `base` itself, which may have moved on since, or moved to
/// another line of history (an operator's local-only commits would then count
/// as the agent's). A branch that shares no history with `base` is refused
/// rather than guessed. So is one whose merge base with `base` is its own tip
/// (merged into it, or an empty branch with no reflog) while it carries
/// commits on no remote: that cut would leave the signing guard nothing to
/// check.
///
/// A reflog that git cannot read is an error, never "no creation entry": the
/// merge base, recorded for good, would replace the exact cut for a
/// transient failure.
fn infer_cut(root: &Path, branch: &str, base: &str) -> Result<(String, CutSource), MemoryError> {
    let local = format!("refs/heads/{branch}");
    let base_sha = resolve_base(root, base)?;
    let log = git(root, &["reflog", "show", "--format=%H %gs", &local]).map_err(|e| {
        let e = match e {
            MemoryError::Validation(detail) => detail,
            other => other.to_string(),
        };
        MemoryError::Validation(format!(
            "cannot infer the cut of the issue branch `{branch}`: its reflog could not be read \
             ({e}); run again, or record the commit it was cut from with `git config {} <sha>`",
            cut_key(branch)
        ))
    })?;
    let created = log.lines().last().and_then(|oldest| {
        let (sha, subject) = oldest.split_once(' ')?;
        subject
            .starts_with("branch: Created from")
            .then(|| resolve_commit(root, sha))
            .flatten()
    });
    if let Some(sha) = created {
        if is_ancestor(root, &sha, &base_sha)? {
            return Ok((sha, CutSource::Reflog));
        }
        tracing::warn!(
            branch = %branch,
            created = %sha,
            base = %base,
            "the issue branch's reflog creation entry is not on the base, so the branch was made \
             again since it was cut; autopilot infers its cut from the merge base instead"
        );
    }
    let refuse = |why: &str| {
        MemoryError::Validation(format!(
            "cannot infer the cut of the issue branch `{branch}` in {}: it records none, its \
             reflog has no creation entry on the base, and {why}; record the commit it was cut from with \
             `git config {} <sha>` and run again",
            root.display(),
            cut_key(branch)
        ))
    };
    // `merge-base` exits 1 for "no common ancestor"; any other failure is git's.
    let args = ["merge-base", &local, &base_sha];
    let out = run_git_output(git_command(), root, &args)?;
    let merge_base = match out.code {
        Some(0) => resolve_commit(root, &out.stdout).ok_or_else(|| out.failure(root, &args))?,
        Some(1) => {
            return Err(refuse(&format!(
                "it shares no history with the base `{base}`"
            )))
        }
        _ => return Err(out.failure(root, &args)),
    };
    if branch_tip(root, branch)?.as_deref() == Some(merge_base.as_str()) {
        let unpushed = git(root, &["rev-list", "--count", &local, "--not", "--remotes"])?;
        if unpushed != "0" {
            return Err(refuse(&format!(
                "its merge base with the base `{base}` is its own tip, with {unpushed} \
                 commit(s) on no remote, so that merge base would leave the signing guard \
                 nothing to check"
            )));
        }
    }
    Ok((merge_base, CutSource::MergeBase))
}

/// Whether `ancestor` is `descendant` or one of its ancestors (`git
/// merge-base --is-ancestor`, which exits 1 for "no"; any other failure is
/// git's).
fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool, MemoryError> {
    let args = ["merge-base", "--is-ancestor", ancestor, descendant];
    let out = run_git_output(git_command(), root, &args)?;
    match out.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(out.failure(root, &args)),
    }
}

/// Record a cut [`infer_cut`] found for an existing `branch` that had none.
/// An inferred cut is logged rather than silent: a wrong one widens or
/// narrows the range the signing guard checks.
fn backfill_cut(
    root: &Path,
    branch: &str,
    base: &str,
    cut: &str,
    source: CutSource,
) -> Result<(), MemoryError> {
    tracing::warn!(
        branch = %branch,
        base = %base,
        cut = %cut,
        source = %source,
        "autopilot found no recorded cut for an existing issue branch; recording one inferred \
         from its history"
    );
    write_cut(root, branch, cut)
}

/// Create `branch` at `cut` (resolved from `base`) with a worktree at `path`,
/// so the branch never exists without its recorded cut.
///
/// The cut is written first, with this call's owner token
/// ([`cut_owner_key`]): a held config lock then fails before anything exists.
/// A branch that appeared since the caller looked (another controller created
/// it) is refused, never given this call's cut. The add names `cut` itself,
/// never `base`: git would resolve a ref name again, and a `base` that moved
/// since `cut` was resolved would make the branch somewhere else. So the
/// branch is made at `cut` by construction, and nothing is ever rolled back
/// ([`settle_created`]).
///
/// The branch never has an upstream (`--no-track`, and a sha start point
/// sets none under any `branch.autoSetupMerge`, measured), so it no longer
/// tracks the base: with `push.default=upstream`, an upstream of
/// `origin/main` would send a bare `git push` from it to the base. Other
/// push configuration (`push.default=matching` or `current`, a
/// `remote.<name>.push` refspec) can still push it without naming anything,
/// so the goal condition names the remote and branch
/// ([`push_remote`]).
///
/// If `git worktree add` fails, whether the branch exists decides:
///
/// - It does not: the cut is taken back, unless another controller has since
///   written its own, or the same value as its own ([`take_back_cut`]).
/// - It does, at `cut`: the cut stays. Either this add made the branch and
///   then failed, or another controller made it from the same commit; either
///   way `cut` is its cut, and the next run reuses it.
/// - It does, elsewhere: another controller made it from another base. If
///   the recorded cut is still this call's, it is replaced by the branch's own
///   ([`infer_cut`]); a value the other controller wrote is left alone.
fn create_branch(
    root: &Path,
    branch: &str,
    path: &Path,
    base: &str,
    cut: &str,
) -> Result<(), MemoryError> {
    create_branch_with(root, branch, path, base, cut, || {})
}

/// [`create_branch`], calling `before_add` just before `git worktree add`.
fn create_branch_with(
    root: &Path,
    branch: &str,
    path: &Path,
    base: &str,
    cut: &str,
    before_add: impl FnOnce(),
) -> Result<(), MemoryError> {
    if let Some(tip) = branch_tip(root, branch)? {
        return Err(MemoryError::Validation(format!(
            "the issue branch `{branch}` appeared in {} (at {tip}) while this run was setting \
             it up, so another controller is creating it; nothing was changed, run again to \
             reuse it",
            root.display()
        )));
    }
    let key = cut_key(branch);
    if let Ok(Some(stale)) = config_get(root, &["--get", &key]) {
        if stale != cut {
            tracing::warn!(
                branch = %branch,
                stale = %quoted_value(&stale),
                cut = %cut,
                "autopilot is replacing a recorded cut for an issue branch that does not exist \
                 yet (another controller may be creating this branch)"
            );
        }
    }
    let owner = uuid::Uuid::new_v4().to_string();
    // The owner first: a write that fails then leaves no cut behind.
    write_config_waiting(root, &cut_owner_key(branch), &owner, |_| {})?;
    write_cut(root, branch, cut)?;
    before_add();
    let path_str = path.to_string_lossy().to_string();
    if let Err(e) = git(
        root,
        &[
            "worktree",
            "add",
            "--no-track",
            "-b",
            branch,
            &path_str,
            cut,
        ],
    ) {
        return Err(after_failed_add(root, branch, base, cut, &owner, e));
    }
    settle_created(root, branch, cut)
}

/// Finish a `branch` this call's add just made at `cut`: record `cut` again
/// if another controller's take-back removed it
/// ([`record_cut_again_if_taken`]).
///
/// Nothing here deletes or removes anything. The add named `cut`, so a tip
/// that is not `cut` is someone's work on the branch (another controller
/// reusing the fresh worktree), and a tip that cannot be read is no proof of
/// anything: either way the branch, its worktree and its cut are kept, and
/// the run stops so that the next one reuses them.
fn settle_created(root: &Path, branch: &str, cut: &str) -> Result<(), MemoryError> {
    // Recorded again first, whatever the tip reads: this add made the
    // branch at `cut`, so `cut` is its cut, and a take-back racing it must
    // not leave the kept branch without one. It deletes nothing.
    let recorded = record_cut_again_if_taken(root, branch, cut);
    let problem = match resolve_commit(root, &format!("refs/heads/{branch}")) {
        Some(tip) if tip == cut => return recorded,
        Some(tip) => format!("it already reads {tip}, so another controller is working on it"),
        None => "its tip could not be read back".to_string(),
    };
    let kept = match recorded {
        Ok(()) => "the branch, its worktree and its cut are kept; run again to reuse them".into(),
        Err(MemoryError::Validation(e)) => format!("the branch and its worktree are kept, but {e}"),
        Err(e) => format!("the branch and its worktree are kept, but {e}"),
    };
    Err(MemoryError::Validation(format!(
        "the issue branch `{branch}` was just created at {cut} in {}, but {problem}; {kept}",
        root.display()
    )))
}

/// After this call's add made `branch` at `cut`: record `cut` again if it no
/// longer reads so. Another controller that recorded the same value and then
/// failed its own add takes that value back ([`take_back_cut`]), and may do
/// so after this add; the branch is this call's, made at `cut`, so `cut` is
/// its cut.
fn record_cut_again_if_taken(root: &Path, branch: &str, cut: &str) -> Result<(), MemoryError> {
    let current = config_get(root, &["--get", &cut_key(branch)]);
    if matches!(current, Ok(Some(ref current)) if current == cut) {
        return Ok(());
    }
    tracing::warn!(
        branch = %branch,
        cut = %cut,
        "autopilot found the cut of the issue branch it just created changed or taken back by \
         another controller; recording it again"
    );
    write_cut(root, branch, cut).map_err(|e| {
        // With none recorded, the next run backfills it from the branch's
        // reflog, whose creation entry this add just wrote. Another
        // controller's value would be reused as it is, so that one is the
        // operator's to correct.
        let next = match current {
            Ok(None) => "the next run records it again from the branch's reflog".to_string(),
            _ => format!(
                "`{}` may hold another controller's value: set it with `git config {} {cut}` \
                 before running again",
                cut_key(branch),
                cut_key(branch)
            ),
        };
        MemoryError::Validation(format!(
            "the issue branch `{branch}` was created at {cut} in {}, but its cut could not be \
             recorded again after another controller took it back ({e}); {next}",
            root.display()
        ))
    })
}

/// Take back the cut this call recorded for a `branch` its add did not make,
/// describing the outcome. Only while both the value and the owner token
/// still read as this call's: another controller that recorded the same
/// value after it owns it now. Then, if a branch appeared meanwhile (another
/// controller's add at the same cut), the cut is recorded again for it.
fn take_back_cut(root: &Path, branch: &str, cut: &str, owner: &str) -> String {
    let owner_key = cut_owner_key(branch);
    match config_get(root, &["--get", &owner_key]) {
        Ok(Some(current)) if current == owner => {}
        Ok(_) => {
            return "the branch was not created, and another controller has since recorded its \
                    own cut, which is kept"
                .to_string()
        }
        Err(why) => {
            return format!(
                "the branch was not created, and whose cut is recorded could not be read \
                 ({why}), so it is kept; it is replaced when the branch is next created"
            )
        }
    }
    if let Err(unset) = unset_cut_if(root, branch, cut) {
        return format!(
            "the branch was not created, and the cut it was given could not be taken back \
             ({unset}); it is replaced when the branch is next created"
        );
    }
    // Only this call's token: one written since is kept.
    let _ = config_waiting(
        root,
        &["config", "--unset", &owner_key, &format!("^{owner}$")],
        &[5],
        |_| {},
    );
    match branch_tip(root, branch) {
        Ok(None) => "the branch was not created and its cut was taken back".to_string(),
        Ok(Some(tip)) if tip == cut => match record_cut_again_if_taken(root, branch, cut) {
            Ok(()) => format!(
                "another controller created the branch `{branch}` at {cut} meanwhile, so its \
                 cut is kept; run again to reuse it"
            ),
            Err(e) => e.to_string(),
        },
        // Made elsewhere: its own controller recorded a different value,
        // which the take-back above, matching this call's value, left alone.
        Ok(Some(tip)) => format!(
            "another controller created the branch `{branch}` at {tip} meanwhile, with its own \
             cut; run again to reuse it"
        ),
        Err(read) => format!(
            "the cut was taken back, but whether the branch `{branch}` exists now could not be \
             read ({read}); check `{}` before running again",
            cut_key(branch)
        ),
    }
}

/// Settle the recorded cut after `git worktree add -b` failed with `e`, and
/// say how in the error returned; see [`create_branch`]. `owner` is the
/// token this call recorded with its cut.
fn after_failed_add(
    root: &Path,
    branch: &str,
    base: &str,
    cut: &str,
    owner: &str,
    e: MemoryError,
) -> MemoryError {
    let detail = match e {
        MemoryError::Validation(detail) => detail,
        other => other.to_string(),
    };
    let settled = match branch_tip(root, branch) {
        Ok(None) => take_back_cut(root, branch, cut, owner),
        Ok(Some(tip)) if tip == cut => format!(
            "the branch `{branch}` exists at {cut} and its cut is kept; run again to reuse it"
        ),
        Ok(Some(tip)) => keep_the_branchs_own_cut(root, branch, base, cut, &tip),
        Err(read) => format!(
            "whether the branch `{branch}` exists could not be read ({read}), so its cut {cut} \
             is kept"
        ),
    };
    MemoryError::Validation(format!("{detail}; {settled}"))
}

/// Another controller created `branch` at `tip` from another base while this
/// call recorded `cut` for it: put the branch's own cut back, unless the other
/// controller already has. Never leaves the branch without one.
fn keep_the_branchs_own_cut(root: &Path, branch: &str, base: &str, cut: &str, tip: &str) -> String {
    let key = cut_key(branch);
    match config_get(root, &["--get", &key]) {
        Ok(Some(current)) if current == cut => {}
        Ok(_) => {
            return format!(
                "another controller created the branch `{branch}` at {tip} and recorded its \
                 own cut, which is kept; run again to reuse it"
            )
        }
        Err(why) => {
            return format!(
                "another controller created the branch `{branch}` at {tip}, and its cut could \
                 not be read ({why}); check `{key}` before running again"
            )
        }
    }
    match infer_cut(root, branch, base)
        .and_then(|(own, source)| backfill_cut(root, branch, base, &own, source).map(|()| own))
    {
        Ok(own) => format!(
            "another controller created the branch `{branch}` at {tip}, so its cut is recorded \
             as {own}, from its own history; run again to reuse it"
        ),
        Err(why) => {
            tracing::warn!(
                branch = %branch,
                cut = %cut,
                error = %why,
                "autopilot left a cut on an issue branch another controller created, which may \
                 not be that branch's cut"
            );
            format!(
                "another controller created the branch `{branch}` at {tip}, and its own cut could \
                 not be recorded ({why}), so `{key}` still reads {cut}, which may not be where it \
                 was cut: set it with `git config {key} <sha>` before running again"
            )
        }
    }
}

/// Whether a failed [`git`] config write failed only on the config lock.
fn config_locked(e: &MemoryError) -> bool {
    let text = e.to_string();
    text.contains("could not lock config file") || text.contains("config.lock")
}

/// Resolve the operator's `base` to a full sha in `root`, or refuse.
fn resolve_base(root: &Path, base: &str) -> Result<String, MemoryError> {
    resolve_commit(root, base).ok_or_else(|| {
        MemoryError::Validation(format!(
            "cannot resolve the base `{base}` to a commit in {}; pass an existing branch, tag \
             or sha as the base (`--base`)",
            root.display()
        ))
    })
}

/// Resolve `repo_root` to the top level of its git working tree, erroring if
/// it is not inside one at all.
///
/// Canonicalizing here rather than trusting the caller's path is what makes
/// [`Worktree::path`] comparable against `git worktree list`'s own output,
/// which is always absolute and symlink-resolved.
pub fn resolve_repo_root(repo_root: &Path) -> Result<PathBuf, MemoryError> {
    if !repo_root.is_dir() {
        return Err(MemoryError::NotFound(format!(
            "repo path is not a directory: {}",
            repo_root.display()
        )));
    }
    let top = git(repo_root, &["rev-parse", "--show-toplevel"])?;
    if top.is_empty() {
        return Err(MemoryError::Validation(format!(
            "{} is not inside a git working tree",
            repo_root.display()
        )));
    }
    std::fs::canonicalize(&top).map_err(MemoryError::from)
}

/// Whether a checkout has uncommitted changes — tracked modifications *or*
/// untracked files.
///
/// `--porcelain` (not `--short`) because its output format is explicitly
/// stable across git versions; untracked files are included deliberately:
/// a dead IC that wrote a half-finished new file left the checkout just as
/// unusable as one that modified a tracked file, and the spec's quarantine
/// rule does not distinguish the two.
pub fn is_dirty(worktree: &Path) -> Result<bool, MemoryError> {
    Ok(!status_porcelain(worktree)?.is_empty())
}

/// The checkout's `git status --porcelain` output, verbatim.
///
/// [`is_dirty`] answers "is anything uncommitted"; this answers "*what* is
/// uncommitted", which is what a caller needs to tell whether a checkout
/// changed **during** some operation rather than whether it was clean before
/// it. A checkout that was already dirty is the case a boolean cannot serve:
/// dirty before and dirty after says nothing, while the same porcelain text
/// before and after says nothing changed.
///
/// `--no-optional-locks`: the status is only read, so it never refreshes and
/// rewrites the index an agent's git may be using.
pub fn status_porcelain(worktree: &Path) -> Result<String, MemoryError> {
    git(worktree, &["--no-optional-locks", "status", "--porcelain"])
}

/// Resolve `path` through symlinks when it exists, falling back to the path
/// as given when it does not (a worktree that has not been created yet).
///
/// Every path this module returns goes through here, because
/// [`Worktree::path`] is persisted verbatim into the dispatch-state drawer
/// and later compared against `git worktree list`'s own always-resolved
/// output. On macOS in particular, `/var` is a symlink to `/private/var`, so
/// a created worktree and a reused one would otherwise be reported under two
/// different spellings of the same directory and a restart's reconciliation
/// would fail to match them.
pub(crate) fn canonical_or_self(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    // `canonicalize` fails outright on a path that does not exist, which
    // would leave exactly the two spellings this function exists to collapse.
    // A worktree registered under `/var/folders/...` whose directory has
    // since been deleted is listed by git as `/private/var/folders/...`, and
    // comparing the un-resolved caller path against it would report the
    // worktree as unregistered — so resolve the deepest ancestor that *does*
    // exist and re-attach the rest.
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            canonical_or_self(parent).join(name)
        }
        _ => path.to_path_buf(),
    }
}

/// Whether `path` is currently registered as a worktree of `repo_root`.
///
/// Reads `git worktree list --porcelain` rather than merely testing for a
/// `.git` file on disk: a directory can exist with stale contents after a
/// `git worktree remove` that left it behind, and only the registration
/// tells us whether git will actually treat it as a checkout.
fn is_registered_worktree(repo_root: &Path, path: &Path) -> Result<bool, MemoryError> {
    let listing = git(repo_root, &["worktree", "list", "--porcelain"])?;
    for line in listing.lines() {
        let Some(listed) = line.strip_prefix("worktree ") else {
            continue;
        };
        // `git worktree list` prints resolved absolute paths, but resolve
        // both sides anyway: a caller-supplied `worktree_root` under a
        // symlinked home directory would otherwise never compare equal.
        let listed_path = canonical_or_self(Path::new(listed));
        if listed_path == canonical_or_self(path) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Move a dirty worktree aside so a fresh one can be created on the same
/// branch, preserving the dead IC's work for inspection.
///
/// Two steps, and the second is the load-bearing one: after `git worktree
/// move`, the branch is still checked out in the quarantined copy, and git
/// refuses to check the same branch out in two worktrees at once. Detaching
/// HEAD there frees the branch name without touching the working tree's
/// contents — `checkout --detach` at the commit already checked out is a
/// ref-only move, so the uncommitted changes that made it dirty in the first
/// place survive intact, which is the whole point of quarantining rather
/// than deleting.
fn quarantine(repo_root: &Path, path: &Path) -> Result<PathBuf, MemoryError> {
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let file_name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        MemoryError::Validation(format!(
            "worktree path has no file name: {}",
            path.display()
        ))
    })?;
    let destination = path.with_file_name(format!("{file_name}.{QUARANTINE_INFIX}-{timestamp}"));
    let destination_str = destination.to_string_lossy().to_string();
    let path_str = path.to_string_lossy().to_string();

    git(
        repo_root,
        &["worktree", "move", &path_str, &destination_str],
    )?;
    git(&destination, &["checkout", "--detach"])?;
    Ok(canonical_or_self(&destination))
}

/// Provision the worktree for `issue`, creating it if absent and quarantining
/// it first if it exists but is dirty.
///
/// `base` is the committish new branches are cut from (e.g. `"origin/main"`
/// or `"HEAD"`). It is consulted only when the issue's branch does not exist
/// yet, or, as a fallback, to infer a missing cut.
///
/// A resumed run whose branch already carries pushed commits reuses that
/// branch untouched: re-cutting it from `base` would silently discard every
/// prior dispatch's work, which is exactly the history the lineage store
/// exists to keep.
///
/// The cut ([`Worktree::cut`]) is resolved from `base` in `repo_root` and
/// recorded before the branch is created ([`create_branch`]), so a branch is
/// never left without one. An existing branch reads it back; one with none
/// (an older branch) gets one inferred from its own history ([`infer_cut`]),
/// with a warning naming the branch, the source and the sha recorded. A
/// `base` that does not resolve, or a cut that cannot be read, inferred or
/// recorded, is refused before anything on disk changes. At creation the
/// signing policy is recorded too ([`record_signing`]), on a best-effort
/// basis: a failed write only warns.
pub fn ensure_worktree(
    repo_root: &Path,
    worktree_root: &Path,
    issue: &IssueRef,
    base: &str,
) -> Result<Worktree, MemoryError> {
    super::validate_repo(&issue.repo)?;
    if base.trim().is_empty() {
        return Err(MemoryError::Validation(
            "base committish must not be empty".into(),
        ));
    }

    let root = resolve_repo_root(repo_root)?;
    let branch = branch_name(issue);
    let path = worktree_path(worktree_root, issue);
    let branch_exists = git_ok(
        &root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    // Settled before anything on disk changes, so a bad base or an
    // unrecordable cut leaves nothing behind.
    let cut = if branch_exists {
        match recorded_cut(&root, &branch)? {
            Some(sha) => sha,
            None => {
                let (sha, source) = infer_cut(&root, &branch, base)?;
                backfill_cut(&root, &branch, base, &sha, source)?;
                sha
            }
        }
    } else {
        resolve_base(&root, base)?
    };
    let cut = CommitSha::parse(&cut)?;

    let mut quarantined_from = None;
    if is_registered_worktree(&root, &path)? {
        if !path.exists() {
            // Registered but gone from disk — someone reclaimed the space
            // with `rm -rf`, or a `git worktree remove` died partway. Every
            // probe below (`git status`, `git worktree move`) would fail to
            // even spawn with that directory as its cwd, so the whole issue
            // would stay unrunnable behind a "failed to run git" message
            // until a human pruned it by hand. Prune it here and fall
            // through to creating a fresh checkout on the same branch.
            //
            // `--expire now` is required: a bare `git worktree prune` honors
            // `gc.worktreePruneExpire` (three months by default) and would
            // leave the stale registration in place, so `git worktree add`
            // would still refuse the path as "missing but already
            // registered".
            git(&root, &["worktree", "prune", "--expire", "now"])?;
        } else if !is_dirty(&path)? {
            // Clean, but not necessarily still ours: a `git checkout` run
            // inside the worktree (by a human, or by an IC comparing
            // against another branch) leaves it clean on the wrong branch,
            // and handing that back would report `branch` while the next
            // dispatch actually commits onto whatever is checked out.
            let head = git(&path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
            if head != branch {
                return Err(MemoryError::Validation(format!(
                    "worktree {} is on '{head}', not the issue's branch '{branch}' — check the \
                     branch back out or remove the worktree before dispatching",
                    path.display()
                )));
            }
            return Ok(Worktree {
                path: canonical_or_self(&path),
                branch,
                created: false,
                cut,
                quarantined_from: None,
            });
        } else {
            quarantined_from = Some(quarantine(&root, &path)?);
        }
    }

    // A directory can survive at `path` without being a registered worktree
    // (a prior `git worktree remove` that failed partway, or a stale
    // quarantine target). `git worktree add` refuses a non-empty existing
    // path, so surface that as an actionable error rather than letting git's
    // message stand alone.
    if path.exists() {
        return Err(MemoryError::Validation(format!(
            "{} already exists but is not a registered worktree of {} — move or remove it before dispatching",
            path.display(),
            root.display()
        )));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if branch_exists {
        let path_str = path.to_string_lossy().to_string();
        git(&root, &["worktree", "add", &path_str, &branch])?;
    } else {
        create_branch(&root, &branch, &path, base, &cut)?;
    }
    // Warn-only, as at a run's start: another worktree's agent may hold
    // `.git/config`'s lock, and `run_issue` holds the run to the policy it
    // reads at start whether or not this write lands.
    // An unreadable policy holds the branch (fail closed) but is not a
    // policy seen on, so it never arms the repository.
    let policy = signing_policy(&path);
    if policy.clone().unwrap_or(true) {
        if let Err(e) = record_signing(&path, &branch, policy == Ok(true)) {
            tracing::warn!(
                error = %e,
                branch = %branch,
                "autopilot could not record the signing policy on the new worktree's branch"
            );
        }
    }

    Ok(Worktree {
        path: canonical_or_self(&path),
        branch,
        created: true,
        cut,
        quarantined_from,
    })
}

// ── rung 10: giving the worktree back ───────────────────────────────────

/// What [`remove_worktree`] did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WorktreeRemoval {
    /// The worktree was registered and clean, and is now gone.
    Removed { path: String },
    /// There was nothing to remove. Returned both for a path git never knew
    /// about and for one whose registration was stale — the second is
    /// pruned on the way past, so the *next* `ensure_worktree` for this
    /// issue takes the create path rather than the "registered but missing"
    /// repair path.
    Absent,
    /// The worktree has uncommitted changes, so it was left alone.
    ///
    /// **Refusing is the point.** Cleanup runs after a merge, and a dirty
    /// worktree at that moment means work the merge did not include —
    /// generated files, a half-finished follow-up, or an IC killed mid-edit.
    /// Deleting it would destroy the only copy, silently, as a side effect
    /// of a *successful* merge. [`ensure_worktree`] already quarantines a
    /// dirty tree when it needs the path back; nothing here needs it back.
    DirtyRefused { path: String },
}

impl WorktreeRemoval {
    /// Whether the worktree is gone now, however it got that way.
    pub fn cleaned(&self) -> bool {
        matches!(
            self,
            WorktreeRemoval::Removed { .. } | WorktreeRemoval::Absent
        )
    }
}

/// Remove the worktree for `issue`, the spec's *"Lead records outcome, cleans
/// worktree"* step.
///
/// Idempotent by construction: every terminal state is reachable twice with
/// the same answer, because the caller is a cron-restarted pass that will see
/// the same merged PR again and must not fail the second time.
///
/// Deliberately **not** called when an IC goes green. The worktree is the
/// reviewer's input — [`super::review::review_pr`] reads the diff from it —
/// so removing it at success would delete the checkout the next step needs.
/// It is removed after the PR lands, and not before.
///
/// The branch is left alone. Deleting the head branch is
/// [`super::merge::MergeRequest::delete_branch`]'s decision, made against the
/// *remote*; a local branch ref costs nothing and is the last on-disk trace
/// of what an IC did.
pub fn remove_worktree(
    repo_root: &Path,
    worktree_root: &Path,
    issue: &IssueRef,
) -> Result<WorktreeRemoval, MemoryError> {
    super::validate_repo(&issue.repo)?;
    let root = resolve_repo_root(repo_root)?;
    let path = worktree_path(worktree_root, issue);
    let path_str = path.to_string_lossy().to_string();

    if !is_registered_worktree(&root, &path)? {
        // An unregistered directory at this path is not ours to delete: git
        // never made it, so something else did.
        return Ok(WorktreeRemoval::Absent);
    }

    if !path.exists() {
        // Registered but gone from disk. `git status` cannot even spawn with
        // that cwd, so the dirty check below would error rather than answer;
        // prune the stale registration and report the truth.
        git(&root, &["worktree", "prune"])?;
        return Ok(WorktreeRemoval::Absent);
    }

    if is_dirty(&path)? {
        return Ok(WorktreeRemoval::DirtyRefused { path: path_str });
    }

    git(&root, &["worktree", "remove", &path_str])?;
    // `remove` unregisters the worktree it removed; the prune is for the
    // administrative files a concurrent `rm -rf` elsewhere may have
    // orphaned. Cheap, and it keeps `git worktree list` honest for the next
    // pass's registration check.
    git(&root, &["worktree", "prune"])?;
    Ok(WorktreeRemoval::Removed { path: path_str })
}

/// Test support other modules' tests share.
#[cfg(test)]
pub(crate) mod testing {
    /// A `tracing` writer that keeps what it is given, for asserting a warning.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Run `f` with a thread-local subscriber; returns its result and the log.
    pub(crate) fn with_captured_log<T>(f: impl FnOnce() -> T) -> (T, String) {
        // A second live dispatcher turns off tracing-core's one-dispatcher
        // fast path. On it, a callsite first reached on another test's
        // thread, which has no subscriber, caches "never" for every thread,
        // and this capture would silently miss that event.
        let _second = tracing::Dispatch::new(tracing_subscriber::registry());
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let out = tracing::subscriber::with_default(subscriber, f);
        let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        (out, log)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::with_captured_log;
    use super::*;
    use crate::autopilot::test_git;

    /// Fixture git ([`crate::autopilot::test_git`]), asserting success.
    fn run(cwd: &Path, args: &[&str]) {
        test_git::output(cwd, args);
    }

    /// A real single-commit git repo. These tests drive the actual `git`
    /// binary rather than a mock: this module's entire job is to be correct
    /// about git's worktree semantics (branch-checked-out-twice refusal,
    /// registration vs. mere directory existence), and a mock would only
    /// assert that we call the commands we already decided to call.
    fn fixture_repo() -> tempfile::TempDir {
        fixture_repo_of("sha1")
    }

    /// [`fixture_repo`] with the given `--object-format`.
    fn fixture_repo_of(object_format: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        run(
            path,
            &[
                // Tests hold a ref's lock file under `.git/refs/heads`.
                "-c",
                "init.defaultRefFormat=files",
                "init",
                "--initial-branch=main",
                &format!("--object-format={object_format}"),
            ],
        );
        run(path, &["config", "user.email", "autopilot@example.test"]);
        run(path, &["config", "user.name", "Autopilot Test"]);
        // Pinned: the global config signs and runs hooks; worktrees share this.
        run(path, &["config", "commit.gpgsign", "false"]);
        run(path, &["config", "core.hooksPath", "/dev/null"]);
        std::fs::write(path.join("README.md"), "seed\n").unwrap();
        run(path, &["add", "README.md"]);
        run(path, &["commit", "-m", "seed"]);
        dir
    }

    fn issue() -> IssueRef {
        IssueRef::new("ironrace/ironmem", 283)
    }

    /// Fixture repos use the files ref backend, which tests that hold a
    /// ref's lock file rely on.
    #[test]
    fn fixture_repos_use_files_refs() {
        let repo = fixture_repo();
        assert!(repo.path().join(".git/refs/heads").is_dir());
        assert!(!repo.path().join(".git/reftable").exists());
    }

    fn read_config(repo: &Path, key: &str) -> Option<String> {
        test_git::config_get(repo, key)
    }

    fn issue_cut_key() -> String {
        format!("branch.{}.ironmemCut", branch_name(&issue()))
    }

    /// Move the operator checkout's `HEAD` on by one commit; returns it.
    fn advance_operator_head(repo: &Path, file: &str) -> String {
        std::fs::write(repo.join(file), file).unwrap();
        run(repo, &["add", file]);
        run(repo, &["commit", "-qm", file]);
        resolve_commit(repo, "HEAD").unwrap()
    }

    #[test]
    fn the_cut_is_recorded_once_and_survives_a_moved_operator_head() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let head = resolve_commit(repo.path(), "HEAD").unwrap();
        let created = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        assert_eq!(created.cut, head);
        assert_eq!(
            read_config(repo.path(), &issue_cut_key()),
            Some(head.clone())
        );

        advance_operator_head(repo.path(), "later");
        let resumed = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        assert!(!resumed.created);
        assert_eq!(resumed.cut, head, "a resume never re-resolves the cut");
        assert_eq!(read_config(repo.path(), &issue_cut_key()), Some(head));
    }

    #[test]
    fn an_absent_cut_is_backfilled_once_on_reuse() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let created = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        // A worktree created before the cut was recorded.
        run(repo.path(), &["config", "--unset", &issue_cut_key()]);
        // The operator's checkout moves on; the backfill must not follow it.
        advance_operator_head(repo.path(), "later");
        let reused = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        assert_eq!(reused.cut, created.cut, "the branch's creation point");
        assert_eq!(
            read_config(repo.path(), &issue_cut_key()),
            Some(created.cut.to_string())
        );
        advance_operator_head(repo.path(), "later-still");
        let again = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        assert_eq!(again.cut, created.cut);
    }

    /// Drop the issue branch's reflog, as `core.logAllRefUpdates=false` or
    /// an expired reflog leaves it.
    fn drop_issue_reflog(repo: &Path) {
        let log = repo
            .join(".git/logs/refs/heads")
            .join(branch_name(&issue()));
        std::fs::remove_file(log).unwrap();
    }

    #[test]
    fn without_a_reflog_the_cut_is_backfilled_from_the_merge_base_with_the_base() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let created = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        commit_in(&created.path, "agent-work");
        run(repo.path(), &["config", "--unset", &issue_cut_key()]);
        drop_issue_reflog(repo.path());
        advance_operator_head(repo.path(), "later");
        let (reused, log) =
            with_captured_log(|| ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD"));
        assert_eq!(reused.unwrap().cut, created.cut);
        assert!(log.contains("merge base"), "{log}");
    }

    #[test]
    fn a_cut_that_cannot_be_inferred_is_refused_not_guessed() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        run(repo.path(), &["config", "--unset", &issue_cut_key()]);
        drop_issue_reflog(repo.path());
        // A base that shares no history with the branch.
        let tree = test_git::run(repo.path(), &["mktree"]);
        let orphan = test_git::run(repo.path(), &["commit-tree", &tree, "-m", "orphan"]);
        let err = ensure_worktree(repo.path(), roots.path(), &issue(), &orphan)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot infer the cut"), "got: {err}");
        assert!(
            err.contains(&format!("git config {}", issue_cut_key())),
            "got: {err}"
        );
        assert_eq!(read_config(repo.path(), &issue_cut_key()), None);
    }

    /// Commit a new file in `dir`; returns the commit.
    fn commit_in(dir: &Path, file: &str) -> String {
        std::fs::write(dir.join(file), file).unwrap();
        run(dir, &["add", file]);
        run(dir, &["commit", "-qm", file]);
        resolve_commit(dir, "HEAD").unwrap()
    }

    #[test]
    fn a_backfilled_cut_is_logged_as_inferred_from_the_base() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let created = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        run(repo.path(), &["config", "--unset", &issue_cut_key()]);
        advance_operator_head(repo.path(), "later");
        let (reused, log) =
            with_captured_log(|| ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD"));
        assert_eq!(reused.unwrap().cut, created.cut);
        assert!(log.contains("WARN"), "{log}");
        assert!(log.contains(&branch_name(&issue())), "{log}");
        assert!(log.contains("reflog"), "{log}");
        assert!(log.contains(created.cut.as_str()), "{log}");
        // A recorded cut is read back silently.
        let (_, quiet) =
            with_captured_log(|| ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD"));
        assert!(!quiet.contains("WARN"), "{quiet}");
    }

    #[test]
    fn a_recorded_cut_that_is_no_longer_a_commit_is_refused_with_the_unset_hint() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let first = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        run(
            repo.path(),
            &["worktree", "remove", first.path.to_str().unwrap()],
        );
        let bogus = "a".repeat(40);
        run(repo.path(), &["config", &issue_cut_key(), &bogus]);
        let err = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD")
            .unwrap_err()
            .to_string();
        assert!(err.contains(&bogus), "got: {err}");
        assert!(
            err.contains(&format!("git config --unset {}", issue_cut_key())),
            "got: {err}"
        );
        assert!(!first.path.exists(), "nothing may be created");
        // The bad record is left for the operator, never silently replaced.
        assert_eq!(read_config(repo.path(), &issue_cut_key()), Some(bogus));
    }

    #[test]
    fn a_recorded_cut_is_quoted_escaped_and_bounded_in_the_refusal() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let first = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        run(
            repo.path(),
            &["worktree", "remove", first.path.to_str().unwrap()],
        );
        let planted = format!("\u{1b}[31mred\nline two {}", "x".repeat(5000));
        run(repo.path(), &["config", &issue_cut_key(), &planted]);
        let err = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD")
            .unwrap_err()
            .to_string();
        assert!(!err.contains('\u{1b}'), "got: {err}");
        assert!(!err.contains('\n'), "got: {err}");
        assert!(!err.contains(&"x".repeat(200)), "got: {err}");
        assert!(
            err.chars().count() < 800,
            "got {} chars",
            err.chars().count()
        );
        assert!(
            err.contains(&format!("git config --unset {}", issue_cut_key())),
            "got: {err}"
        );
    }

    #[test]
    fn a_cut_record_that_cannot_be_read_is_an_error_not_an_absent_cut() {
        let missing = tempfile::tempdir().unwrap().path().join("gone");
        let err = recorded_cut(&missing, &branch_name(&issue()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&format!("cannot read {}", issue_cut_key())),
            "got: {err}"
        );
    }

    #[test]
    fn a_signing_flag_that_cannot_be_read_counts_as_recorded() {
        let repo = fixture_repo();
        let branch = branch_name(&issue());
        assert!(!signing_recorded(repo.path(), &branch));
        run(
            repo.path(),
            &[
                "config",
                &format!("branch.{branch}.ironmemSigning"),
                "maybe",
            ],
        );
        assert!(signing_recorded(repo.path(), &branch));
        let missing = tempfile::tempdir().unwrap().path().join("gone");
        assert!(signing_recorded(&missing, &branch));
    }

    #[test]
    fn recording_signing_over_an_unreadable_flag_warns_and_leaves_it() {
        let repo = fixture_repo();
        let branch = branch_name(&issue());
        let key = format!("branch.{branch}.ironmemSigning");
        run(repo.path(), &["config", &key, "yes-please"]);
        let (recorded, log) = with_captured_log(|| record_signing(repo.path(), &branch, true));
        recorded.unwrap();
        assert!(log.contains("WARN"), "{log}");
        assert!(log.contains(&key), "{log}");
        // Left for the operator: it may be a mistyped opt-out.
        assert_eq!(
            read_config(repo.path(), &key).as_deref(),
            Some("yes-please")
        );
        // The repository flag is still recorded.
        assert_eq!(
            read_config(repo.path(), "ironmem.autopilotSigning").as_deref(),
            Some("true")
        );
    }

    #[test]
    fn a_sha256_repository_provisions_an_issue_worktree() {
        let repo = fixture_repo_of("sha256");
        let roots = tempfile::tempdir().unwrap();
        let head = resolve_commit(repo.path(), "HEAD").expect("a sha256 HEAD resolves");
        assert_eq!(head.len(), 64);
        let wt = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        assert_eq!(wt.cut, head);
        assert_eq!(read_config(repo.path(), &issue_cut_key()), Some(head));
    }

    #[test]
    fn config_writes_run_in_the_c_locale() {
        use std::ffi::OsStr;
        let command = config_write_command();
        let envs: std::collections::HashMap<&OsStr, Option<&OsStr>> = command.get_envs().collect();
        for var in ["LC_ALL", "LANG"] {
            assert_eq!(
                envs.get(OsStr::new(var)),
                Some(&Some(OsStr::new("C"))),
                "{var} must be C so config_locked can read git's lock message"
            );
        }
        // GNU gettext prefers LANGUAGE over LC_ALL outside the C locale.
        assert_eq!(envs.get(OsStr::new("LANGUAGE")), Some(&None));
    }

    #[test]
    fn an_unreadable_signing_policy_at_creation_holds_the_branch_but_not_the_repo() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        run(repo.path(), &["config", "commit.gpgsign", "maybe"]);
        ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        let key = format!("branch.{}.ironmemSigning", branch_name(&issue()));
        assert_eq!(read_config(repo.path(), &key).as_deref(), Some("true"));
        // A typo is not a policy seen on: it must not arm every branch.
        assert_eq!(read_config(repo.path(), "ironmem.autopilotSigning"), None);
    }

    #[test]
    fn a_policy_seen_on_once_in_the_repo_holds_every_branch_until_opted_out() {
        let repo = fixture_repo();
        let branch = branch_name(&issue());
        let other = "autopilot/ironrace-ironmem-3";
        record_signing(repo.path(), &branch, true).unwrap();
        assert_eq!(
            read_config(repo.path(), "ironmem.autopilotSigning").as_deref(),
            Some("true")
        );
        // A branch that never recorded the flag is held by the repo's.
        assert!(signing_recorded(repo.path(), other));
        // An operator's explicit `false` opts one branch out.
        run(
            repo.path(),
            &["config", &format!("branch.{other}.ironmemSigning"), "false"],
        );
        assert!(!signing_recorded(repo.path(), other));
        assert!(signing_recorded(repo.path(), &branch));
    }

    #[test]
    fn an_unresolvable_base_is_refused_before_any_worktree_exists() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        for base in ["no-such-ref", "@{u}"] {
            let err = ensure_worktree(repo.path(), roots.path(), &issue(), base)
                .unwrap_err()
                .to_string();
            assert!(err.contains("cannot resolve the base"), "got: {err}");
            assert!(err.contains(base), "got: {err}");
            assert!(!worktree_path(roots.path(), &issue()).exists());
            assert!(!test_git::verifies(repo.path(), &branch_name(&issue())));
        }
    }

    #[test]
    fn a_remote_url_names_its_github_repo_in_each_form() {
        for url in [
            "https://github.com/Owner/Repo",
            "https://github.com/owner/repo.git",
            "https://GitHub.com/owner/repo/",
            "git@github.com:owner/repo.git",
            "git@github.com:OWNER/REPO",
            "ssh://git@github.com/owner/repo.git",
            "ssh://git@github.com/owner/repo",
            // Userinfo, a token included, and a port are not the host.
            "https://user@github.com/owner/repo.git",
            "https://x-access-token:ghp_secret@github.com/owner/repo",
            "ssh://git@github.com:22/owner/repo.git",
            "github.com:owner/repo.git",
        ] {
            assert!(url_names_github_repo(url, "owner/repo"), "{url}");
        }
        for url in [
            "https://github.com/owner/other",
            "https://github.com/fork/repo.git",
            "https://gitlab.com/owner/repo.git",
            "git@gitlab.com:owner/repo.git",
            "https://github.com/owner/repo/extra",
            "https://github.com/owner",
            "/srv/git/owner/repo.git",
            // A name that only starts with the repo's is another repo.
            "https://github.com/owner/repo2",
            "https://github.com/owner/rep",
            "https://evilgithub.com/owner/repo",
            "https://github.com.evil.example/owner/repo",
            "https://owner@evil.example/github.com/owner/repo",
            "ftp://github.com/owner/repo",
            // An SSH host alias names no host here: it falls through to the
            // next rule rather than be guessed at.
            "git@github-work:owner/repo.git",
        ] {
            assert!(!url_names_github_repo(url, "owner/repo"), "{url}");
        }
        assert!(!url_names_github_repo(
            "https://github.com/owner/repo",
            "owner/repo2"
        ));
    }

    #[test]
    fn the_push_remote_is_the_issue_repos_then_the_tracking_one_then_a_configured_origin() {
        let repo = Some("owner/repo");
        let fork: &[&str] = &["https://github.com/someone/repo.git"];
        let upstream: &[&str] = &["git@github.com:owner/repo.git"];
        const NONE: &[&str] = &[];
        // A fork `origin` loses to the remote that names the issue's repo,
        // even when the fork already has the branch.
        assert_eq!(
            pick_push_remote(&[("origin", fork), ("up", upstream)], &["origin"], repo),
            Some("up")
        );
        // No URL names it: the remote already tracking the branch.
        assert_eq!(
            pick_push_remote(&[("origin", fork), ("mine", NONE)], &["mine"], repo),
            Some("mine")
        );
        assert_eq!(
            pick_push_remote(&[("a", NONE), ("origin", NONE)], &["a", "origin"], None),
            Some("origin")
        );
        // Then `origin`, only when it is configured; then the only remote.
        assert_eq!(
            pick_push_remote(&[("a", NONE), ("origin", NONE)], &[], None),
            Some("origin")
        );
        assert_eq!(pick_push_remote(&[("fork", NONE)], &[], repo), Some("fork"));
        // Otherwise none: a missing `origin` is never named.
        assert_eq!(
            pick_push_remote(&[("a", NONE), ("b", NONE)], &[], repo),
            None
        );
        assert_eq!(pick_push_remote(&[], &[], repo), None);
        // A remote that provably pushes to another GitHub repo is never a
        // fallback, so the only remaining eligible one is.
        assert_eq!(
            pick_push_remote(&[("github", fork), ("gitlab", NONE)], &[], repo),
            Some("gitlab")
        );

        let (checkout, _origin) = fixture_repo_with_origin();
        let dir = checkout.path();
        let branch = branch_name(&issue());
        run(
            dir,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/fork/ironmem.git",
            ],
        );
        run(
            dir,
            &[
                "remote",
                "add",
                "upstream",
                "git@github.com:IronRace/IronMem.git",
            ],
        );
        assert_eq!(
            push_remote(dir, &branch, &issue().repo).as_deref(),
            Some("upstream")
        );
        // Only the push URL counts: a mirror fetched from, pushed to the
        // issue's repo.
        run(
            dir,
            &[
                "remote",
                "set-url",
                "upstream",
                "https://github.com/mirror/ironmem.git",
            ],
        );
        assert_eq!(
            push_remote(dir, &branch, &issue().repo).as_deref(),
            None,
            "both push to other GitHub repos, so neither is named"
        );
        run(
            dir,
            &[
                "remote",
                "set-url",
                "--push",
                "upstream",
                "https://user:token@github.com/IronRace/IronMem.git",
            ],
        );
        assert_eq!(
            push_remote(dir, &branch, &issue().repo).as_deref(),
            Some("upstream")
        );
        run(dir, &["remote", "remove", "upstream"]);
        run(dir, &["remote", "rename", "origin", "github"]);
        run(
            dir,
            &["remote", "add", "gitlab", "https://gitlab.com/x/y.git"],
        );
        // `github` pushes to a fork; `gitlab` is the one left.
        assert_eq!(
            push_remote(dir, &branch, &issue().repo).as_deref(),
            Some("gitlab")
        );
    }

    /// Steps 2 to 4 never name a remote whose every push URL is a GitHub
    /// URL for another repo: `git push origin` to a fork still creates
    /// `refs/remotes/origin/<branch>` (measured by review), so tracking the
    /// branch proves nothing. A push URL this cannot read (an SSH alias, a
    /// GitHub Enterprise host, a local path) stays eligible.
    #[test]
    fn a_remote_that_pushes_to_another_github_repo_is_never_a_fallback() {
        let repo = Some("owner/repo");
        let fork: &[&str] = &["https://github.com/someone/repo.git"];
        let alias: &[&str] = &["git@github-work:owner/repo.git"];
        let local: &[&str] = &["/srv/git/repo.git"];
        let enterprise: &[&str] = &["https://github.example.com/owner/repo.git"];
        // `origin` fetches owner/repo but pushes to a fork and tracks the
        // branch; `upstream` pushes to owner/repo through an alias.
        assert_eq!(
            pick_push_remote(&[("origin", fork), ("upstream", alias)], &["origin"], repo),
            Some("upstream")
        );
        // A tracking `origin` that pushes to a fork, alone: none.
        assert_eq!(
            pick_push_remote(&[("origin", fork)], &["origin"], repo),
            None
        );
        // Unreadable push URLs stay eligible, by any later step.
        assert_eq!(
            pick_push_remote(&[("origin", alias)], &[], repo),
            Some("origin")
        );
        assert_eq!(
            pick_push_remote(&[("local", local)], &[], repo),
            Some("local")
        );
        assert_eq!(
            pick_push_remote(&[("ghe", enterprise), ("fork", fork)], &["ghe"], repo),
            Some("ghe")
        );
        // One push URL elsewhere is not all of them.
        let both: &[&str] = &["https://github.com/someone/repo.git", "/srv/git/repo.git"];
        assert_eq!(
            pick_push_remote(&[("origin", both)], &[], repo),
            Some("origin")
        );

        // The same with real remotes.
        let checkout = fixture_repo();
        let dir = checkout.path();
        let branch = branch_name(&issue());
        run(
            dir,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/ironrace/ironmem.git",
            ],
        );
        run(
            dir,
            &[
                "config",
                "remote.origin.pushurl",
                "https://github.com/someone/ironmem.git",
            ],
        );
        run(
            dir,
            &[
                "remote",
                "add",
                "upstream",
                "git@github-work:ironrace/ironmem.git",
            ],
        );
        let head = test_git::run(dir, &["rev-parse", "HEAD"]);
        run(
            dir,
            &[
                "update-ref",
                &format!("refs/remotes/origin/{branch}"),
                &head,
            ],
        );
        assert_eq!(
            push_remote(dir, &branch, &issue().repo).as_deref(),
            Some("upstream")
        );
    }

    /// Run start warns, naming remotes only, when no remote's push URL names
    /// the issue's repo and some push to another GitHub repo.
    #[test]
    fn remotes_pushing_elsewhere_are_named_only_when_none_pushes_to_the_issue_repo() {
        let fork: &[&str] = &["https://x-access-token:ghp_secret@github.com/someone/repo.git"];
        let issue_repo: &[&str] = &["git@github.com:owner/repo.git"];
        let alias: &[&str] = &["git@github-work:owner/repo.git"];
        assert_eq!(
            remotes_pushing_elsewhere(&[("origin", fork), ("up", alias)], "owner/repo"),
            vec!["origin"]
        );
        assert!(
            remotes_pushing_elsewhere(&[("origin", fork), ("up", issue_repo)], "owner/repo")
                .is_empty()
        );
        assert!(remotes_pushing_elsewhere(&[("up", alias)], "owner/repo").is_empty());

        // At run start: the remote is named, its URL (and token) never.
        let checkout = fixture_repo();
        let dir = checkout.path();
        run(
            dir,
            &[
                "remote",
                "add",
                "origin",
                "https://x-access-token:ghp_secret@github.com/someone/ironmem.git",
            ],
        );
        let ((), log) = with_captured_log(|| warn_if_no_remote_pushes_to(dir, &issue().repo));
        assert!(log.contains("origin") && log.contains("push"), "{log}");
        assert!(
            !log.contains("ghp_secret") && !log.contains("someone"),
            "{log}"
        );
        run(
            dir,
            &[
                "remote",
                "add",
                "upstream",
                "git@github.com:ironrace/ironmem.git",
            ],
        );
        let ((), log) = with_captured_log(|| warn_if_no_remote_pushes_to(dir, &issue().repo));
        assert!(log.is_empty(), "{log}");
    }

    /// Step 1 goes by where `git push <remote>` really sends the branch:
    /// a remote's `pushurl`s when it has any, else its `url`, after any
    /// `pushInsteadOf`/`insteadOf` rewrite, as `git remote -v`'s push lines
    /// show them (measured). A fetch URL that names the issue's repo, with
    /// pushes going elsewhere, is no match.
    #[test]
    fn the_push_remote_is_chosen_by_where_a_push_really_goes() {
        let repo = &issue().repo;
        let branch = branch_name(&issue());
        let issue_url = "https://github.com/ironrace/ironmem.git";
        let fork_url = "https://github.com/someone/ironmem.git";

        // Fetches the issue's repo but pushes to a fork: not `origin`.
        let checkout = fixture_repo();
        let dir = checkout.path();
        run(dir, &["remote", "add", "origin", issue_url]);
        run(dir, &["config", "remote.origin.pushurl", fork_url]);
        run(dir, &["remote", "add", "upstream", issue_url]);
        assert_eq!(push_remote(dir, &branch, repo).as_deref(), Some("upstream"));

        // Fetches a fork but pushes to the issue's repo: `origin`.
        let checkout = fixture_repo();
        let dir = checkout.path();
        run(dir, &["remote", "add", "origin", fork_url]);
        run(dir, &["config", "remote.origin.pushurl", issue_url]);
        run(dir, &["remote", "add", "other", fork_url]);
        assert_eq!(push_remote(dir, &branch, repo).as_deref(), Some("origin"));

        // A `pushInsteadOf` rewrite decides as well: `up` fetches a fork and
        // pushes to the issue's repo; `origin` fetches the issue's repo and
        // pushes to a fork.
        let checkout = fixture_repo();
        let dir = checkout.path();
        run(dir, &["remote", "add", "origin", issue_url]);
        run(
            dir,
            &["remote", "add", "up", "https://example.invalid/ironmem.git"],
        );
        run(
            dir,
            &[
                "config",
                "url.git@github.com:ironrace/.pushInsteadOf",
                "https://example.invalid/",
            ],
        );
        run(
            dir,
            &[
                "config",
                "url.git@github.com:someone/.pushInsteadOf",
                "https://github.com/ironrace/",
            ],
        );
        assert_eq!(push_remote(dir, &branch, repo).as_deref(), Some("up"));
    }

    /// The issue branch has no upstream, even from a remote-tracking base
    /// and under `branch.autoSetupMerge=always`: with `push.default=upstream`
    /// a bare `git push` would otherwise send the agent's work to the base.
    #[test]
    fn a_remote_tracking_base_records_its_sha_and_sets_no_upstream() {
        for auto_setup_merge in [None, Some("always")] {
            let (repo, _origin) = fixture_repo_with_origin();
            if let Some(mode) = auto_setup_merge {
                run(repo.path(), &["config", "branch.autoSetupMerge", mode]);
            }
            let remote_main = resolve_commit(repo.path(), "origin/main").unwrap();
            // The operator's HEAD moves past origin/main; the cut must not follow.
            advance_operator_head(repo.path(), "local-only");
            let roots = tempfile::tempdir().unwrap();
            let wt = ensure_worktree(repo.path(), roots.path(), &issue(), "origin/main").unwrap();
            assert_eq!(wt.cut, remote_main);
            assert_eq!(
                read_config(repo.path(), &issue_cut_key()),
                Some(remote_main)
            );
            let branch = branch_name(&issue());
            for key in ["remote", "merge"] {
                assert_eq!(
                    read_config(repo.path(), &format!("branch.{branch}.{key}")),
                    None,
                    "branch.{branch}.{key} under autoSetupMerge {auto_setup_merge:?}"
                );
            }
        }
    }

    #[test]
    fn a_signing_policy_on_at_creation_is_recorded_on_the_branch() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        run(repo.path(), &["config", "commit.gpgsign", "true"]);
        ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        let key = format!("branch.{}.ironmemSigning", branch_name(&issue()));
        assert_eq!(read_config(repo.path(), &key).as_deref(), Some("true"));
    }

    /// Hold `.git/config`'s lock the way a sibling worktree's agent does
    /// mid-`git config`; dropped (or released early) by removing the file.
    fn hold_config_lock(repo: &Path) -> PathBuf {
        let lock = repo.join(".git/config.lock");
        std::fs::write(&lock, "").unwrap();
        lock
    }

    #[test]
    fn a_cut_that_cannot_be_recorded_again_is_left_to_the_next_runs_reflog_backfill() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let created = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        let root = resolve_repo_root(repo.path()).unwrap();
        // Taken back under the branch, and the config lock is held.
        run(&root, &["config", "--unset", &issue_cut_key()]);
        let lock = hold_config_lock(&root);
        let err = record_cut_again_if_taken(&root, &branch_name(&issue()), &created.cut)
            .unwrap_err()
            .to_string();
        std::fs::remove_file(lock).unwrap();
        assert!(err.contains("reflog"), "{err}");
        assert!(!err.contains("before running again"), "{err}");
        // As promised: the next run records it from the branch's reflog.
        let reused = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        assert_eq!(reused.cut, created.cut);
    }

    #[test]
    fn a_held_config_lock_never_fails_creation_over_the_signing_flag() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        // The branch and its cut exist, so recreating the worktree writes
        // config only for the signing flag.
        let first = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        run(
            repo.path(),
            &["worktree", "remove", first.path.to_str().unwrap()],
        );
        run(repo.path(), &["config", "commit.gpgsign", "true"]);
        let lock = hold_config_lock(repo.path());
        let created = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD");
        std::fs::remove_file(&lock).unwrap();
        let created = created.expect("a held lock must not fail creation via record_signing");
        assert!(created.created);
        assert_eq!(created.cut, first.cut);
    }

    #[test]
    fn the_cut_write_retries_once_a_held_config_lock_is_released() {
        let repo = fixture_repo();
        let head = resolve_commit(repo.path(), "HEAD").unwrap();
        let lock = hold_config_lock(repo.path());
        let mut retries = Vec::new();
        // Released only after an attempt has failed on it, so the retry is
        // what succeeds, however slow the machine.
        write_config_waiting(repo.path(), &issue_cut_key(), &head, |attempt| {
            retries.push(attempt);
            let _ = std::fs::remove_file(&lock);
        })
        .expect("a lock released within the retry window must not fail");
        assert_eq!(retries, vec![1]);
        assert_eq!(read_config(repo.path(), &issue_cut_key()), Some(head));
    }

    #[test]
    fn a_failed_worktree_add_takes_back_only_its_own_cut() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        // A ref below the branch's name makes `git worktree add -b` refuse it.
        let blocker = format!("{}/blocker", branch_name(&issue()));
        run(repo.path(), &["branch", &blocker]);
        let err = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD");
        assert!(err.is_err(), "the add must fail: {err:?}");
        assert_eq!(read_config(repo.path(), &issue_cut_key()), None);

        // A value another controller wrote over ours is not ours to remove.
        let ours = resolve_commit(repo.path(), "HEAD").unwrap();
        let theirs = advance_operator_head(repo.path(), "later");
        run(repo.path(), &["config", &issue_cut_key(), &theirs]);
        unset_cut_if(repo.path(), &branch_name(&issue()), &ours).unwrap();
        assert_eq!(
            read_config(repo.path(), &issue_cut_key()),
            Some(theirs.clone())
        );
        unset_cut_if(repo.path(), &branch_name(&issue()), &theirs).unwrap();
        assert_eq!(read_config(repo.path(), &issue_cut_key()), None);
    }

    fn branch_tip(repo: &Path) -> Option<String> {
        resolve_commit(repo, &format!("refs/heads/{}", branch_name(&issue())))
    }

    fn issue_owner_key() -> String {
        format!("branch.{}.ironmemCutOwner", branch_name(&issue()))
    }

    #[test]
    fn a_take_back_keeps_the_same_cut_another_controller_recorded_after_it() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let root = resolve_repo_root(repo.path()).unwrap();
        let branch = branch_name(&issue());
        let cut = resolve_commit(&root, "HEAD").unwrap();
        // A ref below the branch's name makes this controller's add fail
        // with no branch made.
        run(&root, &["branch", &format!("{branch}/blocker")]);
        let path = worktree_path(roots.path(), &issue());
        // Another controller records the same cut, as its own, between this
        // one's write and its add, and has not run its own add yet.
        let err = create_branch_with(&root, &branch, &path, "HEAD", &cut, || {
            run(&root, &["config", &issue_cut_key(), &cut]);
            run(
                &root,
                &["config", &issue_owner_key(), "the-other-controller"],
            );
        });
        assert!(err.is_err());
        assert_eq!(read_config(&root, &issue_cut_key()), Some(cut));
        assert_eq!(
            read_config(&root, &issue_owner_key()),
            Some("the-other-controller".into())
        );
    }

    #[test]
    fn a_branch_whose_cut_was_taken_back_under_it_records_it_again() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let root = resolve_repo_root(repo.path()).unwrap();
        let branch = branch_name(&issue());
        let cut = resolve_commit(&root, "HEAD").unwrap();
        let path = worktree_path(roots.path(), &issue());
        let elsewhere = roots.path().join("elsewhere");
        // A held lock on the branch's ref fails the other controller's add
        // before it makes the branch (measured: exit 255, no branch). A
        // directory already there would not: git makes the branch first.
        let ref_lock = root.join(format!(".git/refs/heads/{branch}.lock"));
        std::fs::create_dir_all(ref_lock.parent().unwrap()).unwrap();
        // Between this controller's cut write and its add, another records
        // the same cut as its own, fails its add with no branch made, and
        // takes its cut back: the value this controller's add relies on.
        create_branch_with(&root, &branch, &path, "HEAD", &cut, || {
            std::fs::write(&ref_lock, "").unwrap();
            let other = create_branch(&root, &branch, &elsewhere, "HEAD", &cut);
            std::fs::remove_file(&ref_lock).unwrap();
            assert!(other.is_err());
            assert_eq!(read_config(&root, &issue_cut_key()), None);
        })
        .unwrap();
        assert_eq!(branch_tip(&root), Some(cut.clone()));
        assert_eq!(
            read_config(&root, &issue_cut_key()),
            Some(cut),
            "a branch never exists without its cut"
        );
    }

    #[test]
    fn a_created_branch_records_its_cut_with_this_controllers_owner_token() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        let owner = read_config(repo.path(), &issue_owner_key()).expect("an owner token");
        assert!(uuid::Uuid::parse_str(&owner).is_ok(), "{owner}");
    }

    /// A repo whose `origin/main` is a remote-tracking base.
    fn fixture_repo_with_origin() -> (tempfile::TempDir, tempfile::TempDir) {
        let repo = fixture_repo();
        let origin = tempfile::tempdir().unwrap();
        run(
            origin.path(),
            &["init", "-q", "--bare", "--initial-branch=main"],
        );
        run(
            repo.path(),
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        );
        run(repo.path(), &["push", "-q", "origin", "main"]);
        (repo, origin)
    }

    #[test]
    fn an_add_that_failed_after_making_the_branch_never_takes_its_cut_back() {
        // The state an add that made the branch and then failed leaves (git
        // makes the branch before it checks the worktree path, measured):
        // the branch at the cut, the cut recorded, the add reported failed.
        let repo = fixture_repo();
        let root = resolve_repo_root(repo.path()).unwrap();
        let branch = branch_name(&issue());
        let cut = resolve_commit(&root, "HEAD").unwrap();
        run(&root, &["config", &issue_cut_key(), &cut]);
        run(&root, &["branch", &branch, &cut]);
        let failed = MemoryError::Validation("fatal: '/elsewhere' already exists".into());
        let err = after_failed_add(&root, &branch, "HEAD", &cut, "owner", failed).to_string();
        assert!(err.contains("kept"), "got: {err}");
        assert_eq!(read_config(&root, &issue_cut_key()), Some(cut));
    }

    #[test]
    fn a_controller_that_loses_the_creation_race_keeps_the_winners_cut() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let root = resolve_repo_root(repo.path()).unwrap();
        let branch = branch_name(&issue());
        let cut = resolve_commit(&root, "HEAD").unwrap();
        let path = worktree_path(roots.path(), &issue());
        // The other controller resolved the same cut and creates the branch
        // between this one's check and its `worktree add`.
        let err = create_branch_with(&root, &branch, &path, "HEAD", &cut, || {
            run(&root, &["branch", &branch, &cut]);
        });
        assert!(err.is_err());
        assert_eq!(read_config(&root, &issue_cut_key()), Some(cut));
    }

    #[test]
    fn a_losing_controller_on_another_base_restores_the_branchs_own_cut() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let root = resolve_repo_root(repo.path()).unwrap();
        let branch = branch_name(&issue());
        let theirs = resolve_commit(&root, "HEAD").unwrap();
        let ours = advance_operator_head(&root, "later");
        let path = worktree_path(roots.path(), &issue());
        // Their cut was written before ours replaced it; their branch is at
        // their cut.
        let err = create_branch_with(&root, &branch, &path, "HEAD", &ours, || {
            run(&root, &["branch", &branch, &theirs]);
        });
        assert!(err.is_err());
        assert_eq!(read_config(&root, &issue_cut_key()), Some(theirs));
    }

    #[test]
    fn a_branch_that_appeared_since_the_check_is_never_given_another_cut() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let root = resolve_repo_root(repo.path()).unwrap();
        let created = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        let other = advance_operator_head(&root, "later");
        let elsewhere = roots.path().join("elsewhere");
        let (err, log) = with_captured_log(|| {
            create_branch(&root, &branch_name(&issue()), &elsewhere, "HEAD", &other)
        });
        let err = err.unwrap_err().to_string();
        assert!(err.contains("run again"), "got: {err}");
        assert!(!log.contains("does not exist"), "{log}");
        assert_eq!(
            read_config(&root, &issue_cut_key()),
            Some(created.cut.to_string())
        );
    }

    #[test]
    fn a_branch_already_merged_into_the_base_is_refused_not_given_an_empty_range() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let created = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        commit_in(&created.path, "agent-work");
        run(repo.path(), &["config", "--unset", &issue_cut_key()]);
        drop_issue_reflog(repo.path());
        // The operator merged the unpushed branch into the base.
        run(
            repo.path(),
            &["merge", "-q", "--ff-only", &branch_name(&issue())],
        );
        let err = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&format!("git config {}", issue_cut_key())),
            "got: {err}"
        );
        // Named for what git saw, not a merge it cannot see.
        assert!(
            err.contains("its merge base with the base `HEAD` is its own tip"),
            "got: {err}"
        );
        assert_eq!(read_config(repo.path(), &issue_cut_key()), None);
    }

    #[test]
    fn replacing_a_recorded_cut_allows_that_another_controller_is_creating_the_branch() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let root = resolve_repo_root(repo.path()).unwrap();
        let earlier = resolve_commit(&root, "HEAD").unwrap();
        run(&root, &["config", &issue_cut_key(), &earlier]);
        advance_operator_head(&root, "later");
        let (created, log) =
            with_captured_log(|| ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD"));
        created.unwrap();
        assert!(
            log.contains("another controller may be creating this branch"),
            "{log}"
        );
        assert!(!log.contains("no longer exists"), "{log}");
    }

    /// A worktree `git worktree add -b <branch> <path> <cut>` just made, as
    /// [`create_branch`] makes it, with the cut recorded.
    fn added_at_cut(root: &Path, path: &Path) -> String {
        let cut = resolve_commit(root, "HEAD").unwrap();
        run(root, &["config", &issue_cut_key(), &cut]);
        run(
            root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch_name(&issue()),
                path.to_str().unwrap(),
                &cut,
            ],
        );
        cut
    }

    /// Greptile's race on the old rollback: a second controller reuses the
    /// fresh, clean worktree and commits before the first reads the tip,
    /// and the first then deleted the branch holding that commit. No delete
    /// is left: the add names `cut` itself, so a tip that is not `cut` can
    /// only be someone's work, which is kept. `settle_created` is what runs
    /// between the add and anything else, so calling it after a commit
    /// lands is that window.
    #[test]
    fn a_commit_landing_on_a_new_branch_right_after_its_add_is_never_deleted() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let root = resolve_repo_root(repo.path()).unwrap();
        let path = worktree_path(roots.path(), &issue());
        let branch = branch_name(&issue());
        let cut = added_at_cut(&root, &path);
        let landed = commit_in(&path, "another-run");
        // And another controller's take-back removed the cut meanwhile: the
        // branch is still this add's, made at `cut`, so it is recorded again.
        run(&root, &["config", "--unset", &issue_cut_key()]);
        let err = settle_created(&root, &branch, &cut)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&branch) && err.contains(&landed), "got: {err}");
        assert!(err.contains("run again"), "got: {err}");
        assert_eq!(branch_tip(&root), Some(landed));
        assert!(path.join("another-run").is_file(), "the worktree is kept");
        assert_eq!(read_config(&root, &issue_cut_key()), Some(cut));
    }

    /// A tip that cannot be read back is no licence to delete anything.
    #[test]
    fn a_new_branch_whose_tip_cannot_be_read_back_keeps_everything() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let root = resolve_repo_root(repo.path()).unwrap();
        let path = worktree_path(roots.path(), &issue());
        let branch = branch_name(&issue());
        let cut = added_at_cut(&root, &path);
        std::fs::write(path.join("someone.txt"), "in use\n").unwrap();
        run(
            &root,
            &["update-ref", "-d", &format!("refs/heads/{branch}")],
        );
        run(&root, &["config", "--unset", &issue_cut_key()]);
        let err = settle_created(&root, &branch, &cut)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&branch) && err.contains("read back"),
            "got: {err}"
        );
        assert!(path.join("someone.txt").is_file(), "the worktree is kept");
        assert_eq!(read_config(&root, &issue_cut_key()), Some(cut));
    }

    #[test]
    fn a_lock_released_after_the_last_attempt_is_not_reported_as_held() {
        let repo = fixture_repo();
        let e = MemoryError::Validation("could not lock config file".into());
        let err = lock_held(repo.path(), e).to_string();
        assert!(!err.contains("held for"), "got: {err}");
        assert!(err.contains("released"), "got: {err}");
        assert!(err.contains("run again"), "got: {err}");
    }

    #[test]
    fn taking_a_cut_back_waits_out_a_held_config_lock() {
        let repo = fixture_repo();
        let head = resolve_commit(repo.path(), "HEAD").unwrap();
        run(repo.path(), &["config", &issue_cut_key(), &head]);
        let lock = hold_config_lock(repo.path());
        let mut retries = Vec::new();
        unset_cut_waiting(repo.path(), &branch_name(&issue()), &head, |attempt| {
            retries.push(attempt);
            let _ = std::fs::remove_file(&lock);
        })
        .expect("a lock released within the retry window must not fail");
        assert_eq!(retries, vec![1]);
        assert_eq!(read_config(repo.path(), &issue_cut_key()), None);
    }

    #[test]
    fn a_base_that_moves_after_it_was_resolved_still_cuts_the_branch_at_its_sha() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let root = resolve_repo_root(repo.path()).unwrap();
        let resolved = resolve_commit(&root, "HEAD").unwrap();
        let branch = branch_name(&issue());
        let path = worktree_path(roots.path(), &issue());
        let mut moved = None;
        // `HEAD` moves between its resolution and `git worktree add`.
        create_branch_with(&root, &branch, &path, "HEAD", &resolved, || {
            moved = Some(advance_operator_head(&root, "moved"));
        })
        .unwrap();
        assert_ne!(moved.as_deref(), Some(resolved.as_str()));
        assert_eq!(branch_tip(&root), Some(resolved.clone()), "at the cut");
        assert!(path.is_dir(), "nothing is rolled back");
        assert_eq!(read_config(&root, &issue_cut_key()), Some(resolved));
    }

    #[test]
    fn a_config_lock_held_past_the_retry_window_still_fails_the_cut_write() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let lock = hold_config_lock(repo.path());
        let err = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD");
        std::fs::remove_file(&lock).unwrap();
        let err = err.unwrap_err().to_string();
        assert!(err.contains("could not lock config file"), "got: {err}");
        assert!(err.contains("ironmemCut"), "got: {err}");
        // The stale-lock remedy names the lock file itself.
        let lock = resolve_repo_root(repo.path())
            .unwrap()
            .join(".git/config.lock");
        assert!(err.contains(&lock.display().to_string()), "got: {err}");
        assert!(err.contains("remove it"), "got: {err}");
        assert_eq!(err.matches("Validation error").count(), 1, "got: {err}");
        // The cut is written first, so a branch never exists without one.
        assert!(!worktree_path(roots.path(), &issue()).exists());
        assert!(!test_git::verifies(
            repo.path(),
            &format!("refs/heads/{}", branch_name(&issue()))
        ));
    }

    #[test]
    fn branch_name_is_issue_scoped_and_prefixed() {
        assert_eq!(branch_name(&issue()), "autopilot/ironrace-ironmem-283");
        // Two repos with the same short name must not collide.
        assert_ne!(
            branch_name(&IssueRef::new("other/ironmem", 283)),
            branch_name(&issue())
        );
    }

    #[test]
    fn ensure_worktree_creates_a_fresh_checkout_on_its_own_branch() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let wt = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();

        assert!(wt.created);
        assert!(wt.quarantined_from.is_none());
        assert!(wt.path.join("README.md").is_file());
        let head = test_git::run(&wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(head, "autopilot/ironrace-ironmem-283");
    }

    #[test]
    fn ensure_worktree_reuses_a_clean_existing_checkout() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let first = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        let second = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();

        assert!(first.created);
        assert!(
            !second.created,
            "a clean worktree must be reused, not recreated"
        );
        assert_eq!(first.path, second.path);
        assert!(second.quarantined_from.is_none());
    }

    #[test]
    fn ensure_worktree_preserves_commits_made_on_the_issue_branch() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let first = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        std::fs::write(first.path.join("work.txt"), "a prior dispatch's work\n").unwrap();
        run(&first.path, &["add", "work.txt"]);
        run(&first.path, &["commit", "-m", "prior dispatch"]);

        let second = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        assert!(
            second.path.join("work.txt").is_file(),
            "a resumed run must not re-cut the branch from base and discard prior work"
        );
    }

    #[test]
    fn ensure_worktree_quarantines_a_dirty_checkout_and_provisions_a_clean_one() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let first = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        std::fs::write(first.path.join("half-done.txt"), "a dead IC's leftovers\n").unwrap();
        assert!(is_dirty(&first.path).unwrap());

        let second = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();

        assert!(second.created);
        assert_eq!(
            second.path, first.path,
            "the fresh worktree takes the canonical path"
        );
        assert!(
            !is_dirty(&second.path).unwrap(),
            "the replacement must be clean"
        );
        assert!(
            !second.path.join("half-done.txt").exists(),
            "dirty state must not leak into the replacement checkout"
        );

        let quarantined = second
            .quarantined_from
            .expect("a dirty worktree must be reported as quarantined");
        assert!(
            quarantined.join("half-done.txt").is_file(),
            "quarantine must preserve the dead IC's work, not delete it"
        );
        // And the branch is free: the replacement, not the quarantine, holds it.
        let head = test_git::run(&second.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(head, branch_name(&issue()));
        let quarantined_head = test_git::run(&quarantined, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            quarantined_head, "HEAD",
            "the quarantined copy must be detached"
        );
    }

    #[test]
    fn ensure_worktree_recreates_a_registered_worktree_whose_directory_is_gone() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let first = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        std::fs::remove_dir_all(&first.path).unwrap();

        let second = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        assert!(second.created);
        assert!(second.path.join("README.md").is_file());
        assert!(second.quarantined_from.is_none());
    }

    #[test]
    fn ensure_worktree_refuses_a_clean_checkout_left_on_another_branch() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let first = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        run(&first.path, &["checkout", "--detach"]);

        let err = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap_err();
        assert!(
            err.to_string().contains("not the issue's branch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ensure_worktree_rejects_an_unregistered_directory_in_the_way() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let path = worktree_path(roots.path(), &issue());
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("stray.txt"), "not a worktree\n").unwrap();

        let err = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap_err();
        assert!(
            err.to_string().contains("not a registered worktree"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ensure_worktree_rejects_a_path_outside_any_git_repo() {
        let not_a_repo = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        assert!(ensure_worktree(not_a_repo.path(), roots.path(), &issue(), "HEAD").is_err());
    }

    #[test]
    fn ensure_worktree_rejects_an_empty_base() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        assert!(ensure_worktree(repo.path(), roots.path(), &issue(), "  ").is_err());
    }

    #[test]
    fn is_dirty_reports_tracked_and_untracked_changes() {
        let repo = fixture_repo();
        assert!(!is_dirty(repo.path()).unwrap());
        std::fs::write(repo.path().join("untracked.txt"), "new\n").unwrap();
        assert!(
            is_dirty(repo.path()).unwrap(),
            "untracked files count as dirty"
        );
        std::fs::remove_file(repo.path().join("untracked.txt")).unwrap();
        std::fs::write(repo.path().join("README.md"), "modified\n").unwrap();
        assert!(
            is_dirty(repo.path()).unwrap(),
            "tracked modifications count as dirty"
        );
    }

    // ── rung 10: giving the worktree back ───────────────────────────────

    #[test]
    fn removing_a_clean_worktree_deletes_it_and_unregisters_it() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let wt = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        assert!(wt.path.exists());

        let removal = remove_worktree(repo.path(), roots.path(), &issue()).unwrap();
        assert!(
            matches!(removal, WorktreeRemoval::Removed { .. }),
            "got {removal:?}"
        );
        assert!(!wt.path.exists(), "the directory is gone");
        assert!(
            !is_registered_worktree(&resolve_repo_root(repo.path()).unwrap(), &wt.path).unwrap(),
            "git no longer lists it"
        );
    }

    #[test]
    fn removing_a_worktree_twice_is_the_same_answer_the_second_time() {
        // The caller is a cron-restarted pass that sees the same merged PR
        // again; a second cleanup must not fail.
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();

        assert!(matches!(
            remove_worktree(repo.path(), roots.path(), &issue()).unwrap(),
            WorktreeRemoval::Removed { .. }
        ));
        assert_eq!(
            remove_worktree(repo.path(), roots.path(), &issue()).unwrap(),
            WorktreeRemoval::Absent
        );
    }

    #[test]
    fn removing_a_worktree_that_never_existed_is_absent_not_an_error() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        assert_eq!(
            remove_worktree(repo.path(), roots.path(), &issue()).unwrap(),
            WorktreeRemoval::Absent
        );
    }

    #[test]
    fn a_dirty_worktree_is_refused_and_left_on_disk() {
        // Cleanup runs after a merge. A dirty tree then is work the merge did
        // not include, and deleting it would destroy the only copy as a side
        // effect of a *successful* merge.
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let wt = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        std::fs::write(wt.path.join("uncommitted.txt"), "an IC's unsaved work\n").unwrap();

        let removal = remove_worktree(repo.path(), roots.path(), &issue()).unwrap();
        assert!(
            matches!(removal, WorktreeRemoval::DirtyRefused { .. }),
            "got {removal:?}"
        );
        assert!(wt.path.exists(), "the dirty tree survives");
        assert!(
            wt.path.join("uncommitted.txt").exists(),
            "and so does the uncommitted file"
        );
        assert!(!removal.cleaned());
    }

    #[test]
    fn a_registration_whose_directory_was_deleted_is_pruned_and_reported_absent() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let wt = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        std::fs::remove_dir_all(&wt.path).unwrap();

        assert_eq!(
            remove_worktree(repo.path(), roots.path(), &issue()).unwrap(),
            WorktreeRemoval::Absent
        );
        assert!(
            !is_registered_worktree(&resolve_repo_root(repo.path()).unwrap(), &wt.path).unwrap(),
            "the stale registration is pruned, so the next ensure_worktree creates rather than repairs"
        );
    }

    #[test]
    fn removal_leaves_the_branch_alone() {
        // Deleting the head branch is the merge's decision, made against the
        // remote. The local ref is the last on-disk trace of what an IC did.
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        remove_worktree(repo.path(), roots.path(), &issue()).unwrap();

        let branches = test_git::run(repo.path(), &["branch", "--list", &branch_name(&issue())]);
        assert!(
            branches.contains(&branch_name(&issue())),
            "branch survives removal, got {branches:?}"
        );
    }

    #[test]
    fn an_unregistered_directory_at_the_path_is_not_deleted() {
        // git never made it, so something else did.
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let path = worktree_path(roots.path(), &issue());
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("someones-file.txt"), "not ours\n").unwrap();

        assert_eq!(
            remove_worktree(repo.path(), roots.path(), &issue()).unwrap(),
            WorktreeRemoval::Absent
        );
        assert!(path.join("someones-file.txt").exists());
    }

    #[test]
    #[cfg(unix)]
    fn the_git_environment_is_scrubbed_before_every_call() {
        // `current_dir` does NOT win against `GIT_DIR`: git reads the variable
        // first. Unscrubbed, `remove_worktree` would run `git worktree remove`
        // against whatever repository the environment names — a destructive
        // command aimed at a repo nobody in this subsystem chose.
        //
        // Asserted against a command carrying the variable, rather than by
        // setting it on the process: environment variables are process-global
        // and the test suite is threaded, so a test that exports `GIT_DIR`
        // redirects every *other* test's git call for as long as it holds it.
        // That is a real defect this repository already has elsewhere, and
        // reproducing a hazard by inflicting it is not a test.
        let mut command = Command::new("sh");
        command.env("GIT_DIR", "/decoy/.git");
        command.env("GIT_WORK_TREE", "/decoy");
        crate::mcp::tools::scrub_git_environment(&mut command);
        let out = command
            .args(["-c", "echo \"${GIT_DIR:-unset}/${GIT_WORK_TREE:-unset}\""])
            .output()
            .expect("sh must be runnable");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "unset/unset",
            "the scrub must remove git's redirecting variables"
        );
    }

    #[test]
    fn every_git_call_in_this_module_goes_through_the_one_constructor() {
        // The scrub is only as good as its coverage, and coverage here is a
        // property of the source: one constructor, and no bare
        // `Command::new("git")` beside it. Checked by reading this file
        // rather than by spawning, because a call site that forgot is
        // invisible at runtime until the day an inherited variable exists.
        // Production code only: the test fixtures below build their own git
        // commands, and they are not what an operator's inherited environment
        // can redirect.
        let source = include_str!("worktree.rs");
        let production = source
            .split_once("\n#[cfg(test)]\nmod tests")
            .map(|(before, _)| before)
            .unwrap_or(source);
        let bare = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .filter(|line| line.contains("Command::new(\"git\")"))
            .count();
        assert_eq!(
            bare, 1,
            "exactly one `Command::new(\"git\")`, inside `git_command`; \
             every other call site must use it"
        );
    }

    /// Break `.git/config` so that every git command reading it fails.
    fn break_config(repo: &Path) {
        let config = repo.join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str("[unterminated\n");
        std::fs::write(config, text).unwrap();
    }

    #[test]
    fn an_unreadable_remote_list_is_warned_about_not_taken_for_none() {
        let checkout = fixture_repo();
        let dir = checkout.path();
        run(
            dir,
            &[
                "remote",
                "add",
                "origin",
                "https://x-access-token:ghp_secret@github.com/ironrace/ironmem.git",
            ],
        );
        break_config(dir);
        let branch = branch_name(&issue());
        let (picked, log) = with_captured_log(|| push_remote(dir, &branch, &issue().repo));
        assert_eq!(picked, None, "nothing readable names a remote");
        assert!(log.contains("could not read"), "{log}");
        assert!(log.contains("bad config"), "git's own complaint: {log}");
        assert!(!log.contains("ghp_secret"), "{log}");
        let ((), log) = with_captured_log(|| warn_if_no_remote_pushes_to(dir, &issue().repo));
        assert!(log.contains("could not read"), "{log}");
        assert!(log.contains("bad config"), "{log}");
    }

    /// `git branch -D` takes the branch's config section (its recorded cut)
    /// and its reflog with it, and a branch made again at the agent's last
    /// commit has a reflog whose creation entry is that commit. Taken as the
    /// cut, it would move the signing guard's range past the unsigned
    /// commits before it.
    #[test]
    fn a_branch_recreated_at_its_tip_keeps_the_cut_it_was_first_given() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let created = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        commit_in(&created.path, "one");
        let tip = commit_in(&created.path, "two");
        run(&created.path, &["checkout", "-q", "--detach"]);
        run(&created.path, &["branch", "-D", &created.branch]);
        run(
            &created.path,
            &["checkout", "-q", "-B", &created.branch, &tip],
        );
        assert_eq!(
            read_config(repo.path(), &issue_cut_key()),
            None,
            "the delete took the recorded cut with it"
        );
        let (reused, log) =
            with_captured_log(|| ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD"));
        let reused = reused.unwrap();
        assert!(!reused.created);
        assert_eq!(reused.cut, created.cut, "not the recreation point {tip}");
        assert!(log.contains("merge base"), "{log}");
    }

    /// A reflog git cannot read is not a reflog without a creation entry: the
    /// merge-base fallback, recorded for good, would replace the exact cut.
    #[test]
    fn a_reflog_that_cannot_be_read_is_refused_not_replaced_by_the_merge_base() {
        let repo = fixture_repo();
        let roots = tempfile::tempdir().unwrap();
        let created = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        // With work on it, the merge-base fallback would succeed.
        let tip = commit_in(&created.path, "agent-work");
        run(repo.path(), &["config", "--unset", &issue_cut_key()]);
        // The creation entry names a corrupt loose object, which makes
        // `git reflog show` exit 128; every other read here still works.
        let corrupt = "1".repeat(40);
        let objects = repo.path().join(".git/objects").join(&corrupt[..2]);
        std::fs::create_dir_all(&objects).unwrap();
        std::fs::write(objects.join(&corrupt[2..]), "garbage").unwrap();
        let log = repo
            .path()
            .join(".git/logs/refs/heads")
            .join(branch_name(&issue()));
        std::fs::write(
            &log,
            format!(
                "{zero} {corrupt} A <a@example.test> 1700000000 +0000\tbranch: Created from main\n\
                 {corrupt} {tip} A <a@example.test> 1700000001 +0000\treset: moving\n",
                zero = "0".repeat(40),
            ),
        )
        .unwrap();
        let err = ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD")
            .unwrap_err()
            .to_string();
        assert!(err.contains("its reflog could not be read"), "{err}");
        assert!(err.contains("exit Some(128)"), "git's own failure: {err}");
        assert_eq!(read_config(repo.path(), &issue_cut_key()), None, "{err}");
    }

    /// `core.fsmonitor` is a program the agent can set in the shared
    /// `.git/config`; the controller's own git never runs it.
    #[test]
    #[cfg(unix)]
    fn the_controllers_git_status_runs_no_configured_fsmonitor() {
        use std::os::unix::fs::PermissionsExt;
        let repo = fixture_repo();
        let scratch = tempfile::tempdir().unwrap();
        let sentinel = scratch.path().join("ran");
        let hook = scratch.path().join("fsmonitor.sh");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        run(
            repo.path(),
            &["config", "core.fsmonitor", &hook.to_string_lossy()],
        );
        assert!(!is_dirty(repo.path()).unwrap());
        assert!(!sentinel.exists(), "the fsmonitor hook ran");
        // The fixture's own git does run it, so the check above can fail.
        test_git::output(repo.path(), &["status", "--porcelain"]);
        assert!(sentinel.exists(), "the fixture's fsmonitor never runs");
    }

    /// `git status` refreshes and rewrites the index when a tracked file's
    /// stat data changed; the controller's status takes no optional lock, so
    /// it never writes the index an agent's git may be using.
    #[test]
    #[cfg(unix)]
    fn the_controllers_git_status_does_not_rewrite_the_index() {
        use std::os::unix::fs::MetadataExt;
        let repo = fixture_repo();
        let index = repo.path().join(".git/index");
        let restat = || {
            let readme = std::fs::File::options()
                .write(true)
                .open(repo.path().join("README.md"))
                .unwrap();
            readme
                .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
                .unwrap();
        };
        restat();
        let before = std::fs::metadata(&index).unwrap().ino();
        assert!(!is_dirty(repo.path()).unwrap());
        assert_eq!(std::fs::metadata(&index).unwrap().ino(), before);
        // The fixture's own status does rewrite it, so the check above can fail.
        restat();
        test_git::output(repo.path(), &["status", "--porcelain"]);
        assert_ne!(std::fs::metadata(&index).unwrap().ino(), before);
    }

    /// A git that does not finish (a hook, or a program the shared config
    /// names) is killed at the deadline, with every process it started.
    #[test]
    #[cfg(unix)]
    fn a_git_command_past_its_deadline_is_killed_with_its_process_group() {
        let repo = fixture_repo();
        let scratch = tempfile::tempdir().unwrap();
        let pid_file = scratch.path().join("pid");
        run(
            repo.path(),
            &[
                "config",
                "alias.hang",
                &format!("!sleep 30 & echo $! > '{}'; wait", pid_file.display()),
            ],
        );
        let started = std::time::Instant::now();
        let err = run_git_output_within(
            git_command(),
            repo.path(),
            &["hang"],
            std::time::Duration::from_millis(500),
        )
        .err()
        .expect("a hung git must fail")
        .to_string();
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(err.contains("git hang") && err.contains("killed"), "{err}");
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .status()
            .unwrap()
            .success();
        assert!(!alive, "the alias's `sleep` survived the kill");
    }

    /// A git that exits while something it started still holds its output
    /// open is killed too: its output never ends, and what holds it runs on
    /// outside any bound.
    #[test]
    #[cfg(unix)]
    fn a_git_command_that_leaves_its_output_open_is_killed_at_the_deadline() {
        let repo = fixture_repo();
        run(repo.path(), &["config", "alias.linger", "!sleep 30 &"]);
        let err = run_git_output_within(
            git_command(),
            repo.path(),
            &["linger"],
            std::time::Duration::from_millis(500),
        )
        .err()
        .expect("output held open past the deadline must fail")
        .to_string();
        assert!(err.contains("left its output open"), "{err}");
    }
}

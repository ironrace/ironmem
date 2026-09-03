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
    // `rev-parse --verify` prints exactly one 40-char object name on success.
    // Anything else means we are not looking at what we think we are, and a
    // half-parsed SHA compared against a PR head would silently never match.
    if sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
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

/// Where an issue's worktree lives under `worktree_root`.
pub fn worktree_path(worktree_root: &Path, issue: &IssueRef) -> PathBuf {
    worktree_root.join(issue.slug())
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
fn git_command() -> Command {
    let mut command = Command::new("git");
    crate::mcp::tools::scrub_git_environment(&mut command);
    command
}

/// Run a `git` subcommand in `cwd` through [`git_command`]'s scrubbed
/// environment, returning trimmed stdout or a descriptive error.
fn git(cwd: &Path, args: &[&str]) -> Result<String, MemoryError> {
    let mut command = git_command();
    let output =
        command.args(args).current_dir(cwd).output().map_err(|e| {
            MemoryError::NotFound(format!("failed to run git {}: {e}", args.join(" ")))
        })?;
    if !output.status.success() {
        return Err(MemoryError::Validation(format!(
            "git {} failed in {} (exit {:?}): {}",
            args.join(" "),
            cwd.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Like [`git`], but reports whether the command succeeded instead of
/// treating a non-zero exit as an error. Used for the `rev-parse --verify`
/// existence probes, where "this ref does not exist" is a normal answer and
/// not a fault.
fn git_ok(cwd: &Path, args: &[&str]) -> Result<bool, MemoryError> {
    let mut command = git_command();
    let status = command
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| MemoryError::NotFound(format!("failed to run git {}: {e}", args.join(" "))))?;
    Ok(status.success())
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
    Ok(!git(worktree, &["status", "--porcelain"])?.is_empty())
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
fn canonical_or_self(path: &Path) -> PathBuf {
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
/// or `"HEAD"`); it is only consulted when the issue's branch does not exist
/// yet. A resumed run whose branch already carries pushed commits reuses that
/// branch untouched — re-cutting it from `base` would silently discard every
/// prior dispatch's work, which is exactly the history the lineage store
/// exists to keep.
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

    let path_str = path.to_string_lossy().to_string();
    let branch_exists = git_ok(
        &root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    if branch_exists {
        git(&root, &["worktree", "add", &path_str, &branch])?;
    } else {
        git(&root, &["worktree", "add", "-b", &branch, &path_str, base])?;
    }

    Ok(Worktree {
        path: canonical_or_self(&path),
        branch,
        created: true,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should be runnable in tests");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A real single-commit git repo. These tests drive the actual `git`
    /// binary rather than a mock: this module's entire job is to be correct
    /// about git's worktree semantics (branch-checked-out-twice refusal,
    /// registration vs. mere directory existence), and a mock would only
    /// assert that we call the commands we already decided to call.
    fn fixture_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        run(path, &["init", "--initial-branch=main"]);
        run(path, &["config", "user.email", "autopilot@example.test"]);
        run(path, &["config", "user.name", "Autopilot Test"]);
        std::fs::write(path.join("README.md"), "seed\n").unwrap();
        run(path, &["add", "README.md"]);
        run(path, &["commit", "-m", "seed"]);
        dir
    }

    fn issue() -> IssueRef {
        IssueRef::new("ironrace/ironmem", 283)
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
        let head = git(&wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
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
        let head = git(&second.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(head, branch_name(&issue()));
        let quarantined_head = git(&quarantined, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
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

        let branches = git(repo.path(), &["branch", "--list", &branch_name(&issue())]).unwrap();
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
            .split_once("\n#[cfg(test)]")
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
}

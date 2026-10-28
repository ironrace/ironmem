//! One controller per issue: an advisory lock held for a whole run.
//!
//! Two controllers on one repository (two Leads, or a Lead and a manual
//! `autopilot run`) can both find an issue safe to provision, and the
//! second's `ensure_worktree` then quarantines the first's live worktree:
//! moves it and detaches its branch under a running implementer. The
//! dispatch-state compare-and-set comes too late to stop that, since the
//! worktree is touched before it.
//!
//! So each controller takes `flock(LOCK_EX | LOCK_NB)` on
//! `<git common dir>/ironmem/autopilot/locks/<slug>-<hash>.lock` before it checks
//! the issue, and holds it until the run ends, dispatch included. One that
//! cannot take it does not provision: the Lead skips the issue for the tick,
//! a manual run refuses, and `clear-dispatch` refuses. The kernel releases a
//! crashed controller's lock with its file descriptor, so nothing stale is
//! ever left to remove.
//!
//! The lock lives in the common directory, which every worktree of a clone
//! shares, so it covers controllers working one clone on one host
//! ([`super::local_fs`] refuses a remote filesystem). The file is named by
//! the issue's slug and a hash of `owner/repo#n`, and taken again if it was
//! replaced between its open and its `flock`.
//!
//! Without the lock (two clones of one repository sharing a database, a
//! controller from before the lock existed, or a lock file removed while
//! held) less holds:
//!
//! - the dispatch state's owner token still makes every run's
//!   compare-and-set its own, so two runs never both record an outcome, and
//!   a run that lost the issue launches and writes nothing more;
//! - a Claude run still adopts another run's claim only once its lease
//!   (`dispatch_state::CLAIM_LEASE`) has lapsed and fresh evidence shows its
//!   IC idle; the lease is renewed as soon as a dispatch returns, before
//!   its outcome is recorded, so a lapsed lease in the post-run window
//!   means the controller is gone;
//! - the lease, and track 2's launch window alike, assume the controller
//!   is not suspended (SIGSTOP, or the host asleep) between its last
//!   ownership check and its exec or identity write; a clock jump has the
//!   same exposure, and no lease length closes it;
//! - a Codex or Muse run still adopts a published invocation only on process
//!   evidence, but an invocation published and not yet spawned shows none,
//!   so between another run's claim and its spawn a second implementer can
//!   launch;
//! - and nothing stops two controllers provisioning one worktree, so one may
//!   quarantine the other's (F2).

use std::fs::{File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::MemoryError;

use super::IssueRef;

/// A held issue lock. Dropping it closes the file, which releases the lock.
#[derive(Debug)]
pub struct IssueLock {
    _file: File,
    path: PathBuf,
}

impl IssueLock {
    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// What an attempt to take an issue's lock found.
#[derive(Debug)]
pub enum IssueLockAttempt {
    /// Taken; held until dropped.
    Acquired(IssueLock),
    /// Another controller holds it.
    Held { path: PathBuf },
}

/// The lock file for `issue` in the repository at `repo` (any checkout or
/// worktree of it).
pub fn lock_path(repo: &Path, issue: &IssueRef) -> Result<PathBuf, MemoryError> {
    lock_path_if_repository(repo, issue)?.ok_or_else(|| {
        MemoryError::Validation(format!(
            "cannot find the git common directory of {} for {}'s lock: it is not in a git \
             repository",
            repo.display(),
            issue.canonical()
        ))
    })
}

/// [`lock_path`], or `None` when `repo` is a directory in no git
/// repository, where no controller can hold the issue.
pub(crate) fn lock_path_if_repository(
    repo: &Path,
    issue: &IssueRef,
) -> Result<Option<PathBuf>, MemoryError> {
    Ok(git_common_dir(repo)?.map(|common| lock_path_in(&common, issue)))
}

/// The lock file for `issue` under the git common directory `common`.
///
/// Named by the issue's slug for a reader, plus a hash of its canonical
/// `owner/repo#n`: slugs collide (`a-b/c#1` and `a/b-c#1` are both
/// `a-b-c-1`), and two issues must never share a lock.
pub(crate) fn lock_path_in(common: &Path, issue: &IssueRef) -> PathBuf {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(issue.canonical().as_bytes());
    let hash: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    common
        .join("ironmem/autopilot/locks")
        .join(format!("{}-{hash}.lock", issue.slug()))
}

/// The git common directory of the repository at `repo` (any checkout or
/// worktree of it), or `None` when `repo` is in no git repository.
///
/// A worktree whose link to its repository is broken (its administrative
/// directory was pruned) still names that repository in its `.git` file, and
/// its lock is still the repository's: it is found from there.
///
/// Refused when the common directory is on a remote filesystem
/// ([`super::local_fs`]).
pub fn git_common_dir(repo: &Path) -> Result<Option<PathBuf>, MemoryError> {
    // Through the cached `find_launcher` resolution, like every git
    // spawn; scrubbed here as the scan requires of a held program name.
    let mut git = Command::new(crate::launcher::cached_launcher("git"));
    git.args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(repo);
    crate::mcp::tools::scrub_git_environment(&mut git);
    // Its message is matched below, so it must be git's own English.
    git.env("LC_ALL", "C").env_remove("LANGUAGE");
    let output = git.output().map_err(|e| {
        MemoryError::Validation(format!("cannot run git in {}: {e}", repo.display()))
    })?;
    let common = if output.status.success() {
        let common = std::str::from_utf8(&output.stdout)
            .map_err(|_| MemoryError::Validation("the git common directory is not UTF-8".into()))?
            .trim_end_matches(['\n', '\r']);
        PathBuf::from(common)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("not a git repository") {
            return Err(MemoryError::Validation(format!(
                "cannot find the git common directory of {}: {}",
                repo.display(),
                super::scrub::scrub_and_bound(&stderr, 500).text
            )));
        }
        match common_dir_of_broken_worktree(repo) {
            Some(common) => common,
            None => return Ok(None),
        }
    };
    super::local_fs::require_local(&common, "the git common directory")?;
    Ok(Some(common))
}

/// The common directory a worktree's `.git` file names when git no longer
/// reads it: `gitdir: <common>/worktrees/<name>`, through that directory's
/// `commondir` when it is still there.
fn common_dir_of_broken_worktree(repo: &Path) -> Option<PathBuf> {
    let link = std::fs::read_to_string(repo.join(".git")).ok()?;
    let gitdir = PathBuf::from(link.trim().strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        repo.join(gitdir)
    };
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(named) => gitdir.join(named.trim()),
        Err(_) => {
            let worktrees = gitdir.parent()?;
            (worktrees.file_name()? == "worktrees").then_some(())?;
            worktrees.parent()?.to_path_buf()
        }
    };
    common.join("HEAD").is_file().then_some(common)
}

/// Take `issue`'s lock in the repository at `repo` without waiting.
pub fn try_lock_issue(repo: &Path, issue: &IssueRef) -> Result<IssueLockAttempt, MemoryError> {
    try_lock_path(&lock_path(repo, issue)?)
}

/// Take the lock at `path` without waiting, creating it (0600, in a 0700
/// directory) if needed.
pub(crate) fn try_lock_path(path: &Path) -> Result<IssueLockAttempt, MemoryError> {
    try_lock_path_with(path, &|| {})
}

/// How many times a lock whose file was replaced between its open and its
/// `flock` is taken again before giving up.
const REPLACED_LOCK_RETRIES: usize = 5;

/// [`try_lock_path`], with `after_open` run between the open and the
/// `flock` (a test's window to replace the file).
fn try_lock_path_with(path: &Path, after_open: &dyn Fn()) -> Result<IssueLockAttempt, MemoryError> {
    use std::os::unix::fs::MetadataExt;
    let cannot = |e: std::io::Error| {
        MemoryError::Validation(format!(
            "cannot open the issue lock {}: {e}",
            path.display()
        ))
    };
    for _ in 0..REPLACED_LOCK_RETRIES {
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(cannot)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .map_err(cannot)?;
        after_open();
        // SAFETY: `file` owns a valid descriptor for the duration of the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(IssueLockAttempt::Held {
                    path: path.to_path_buf(),
                });
            }
            return Err(MemoryError::Validation(format!(
                "the issue lock {} cannot be taken ({e}): file locks do not work on this \
                 filesystem, so no controller can coordinate here; Autopilot needs the \
                 repository on a local disk",
                path.display()
            )));
        }
        // A lock on a file no longer at `path` (removed, or replaced, after
        // the open) excludes no one: another controller opening `path` now
        // gets another file. Taken again on whatever is there.
        let held = file.metadata().map_err(cannot)?;
        match std::fs::metadata(path) {
            Ok(now) if now.dev() == held.dev() && now.ino() == held.ino() => {
                return Ok(IssueLockAttempt::Acquired(IssueLock {
                    _file: file,
                    path: path.to_path_buf(),
                }));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(cannot(e)),
        }
    }
    Err(MemoryError::Validation(format!(
        "the issue lock {} kept being replaced while it was taken; something is removing it",
        path.display()
    )))
}

/// `issue`'s lock for a manual `autopilot run`, which refuses, naming the
/// lock, when another controller holds it.
pub fn lock_for_run(repo: &Path, issue: &IssueRef) -> Result<IssueLock, MemoryError> {
    match try_lock_issue(repo, issue)? {
        IssueLockAttempt::Acquired(lock) => Ok(lock),
        IssueLockAttempt::Held { path } => Err(MemoryError::Validation(format!(
            "refusing to run {}: {}; run again once it finishes",
            issue.canonical(),
            held_reason(issue, &path)
        ))),
    }
}

/// Why a controller that found `issue`'s lock held does nothing with it.
pub fn held_reason(issue: &IssueRef, path: &Path) -> String {
    format!(
        "another controller holds issue {} (its lock {} is held); it is left to that controller",
        issue.canonical(),
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acquired(attempt: IssueLockAttempt) -> IssueLock {
        match attempt {
            IssueLockAttempt::Acquired(lock) => lock,
            IssueLockAttempt::Held { path } => panic!("{} was held", path.display()),
        }
    }

    #[test]
    fn a_second_open_of_a_held_lock_is_refused_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locks/owner-repo-7.lock");
        let first = acquired(try_lock_path(&path).unwrap());
        // A separate open file description, as another controller's is.
        match try_lock_path(&path).unwrap() {
            IssueLockAttempt::Held { path: held } => assert_eq!(held, path),
            IssueLockAttempt::Acquired(_) => panic!("a held lock was taken twice"),
        }
        drop(first);
        acquired(try_lock_path(&path).unwrap());
    }

    #[test]
    fn the_lock_is_private_and_under_the_common_dir_of_any_worktree() {
        use std::os::unix::fs::PermissionsExt;
        let repo = tempfile::tempdir().unwrap();
        super::super::test_git::output(repo.path(), &["init", "-q"]);
        let issue = IssueRef::new("owner/repo", 7);
        let lock = acquired(try_lock_issue(repo.path(), &issue).unwrap());
        let common = std::fs::canonicalize(repo.path().join(".git")).unwrap();
        assert!(
            std::fs::canonicalize(lock.path())
                .unwrap()
                .starts_with(&common),
            "{} is not under {}",
            lock.path().display(),
            common.display()
        );
        let mode = std::fs::metadata(lock.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // A subdirectory of the checkout finds the same lock.
        std::fs::create_dir(repo.path().join("sub")).unwrap();
        assert!(matches!(
            try_lock_issue(&repo.path().join("sub"), &issue).unwrap(),
            IssueLockAttempt::Held { .. }
        ));
    }

    #[test]
    fn a_manual_run_refuses_while_another_controller_holds_the_issue() {
        let repo = tempfile::tempdir().unwrap();
        super::super::test_git::output(repo.path(), &["init", "-q"]);
        let issue = IssueRef::new("owner/repo", 7);
        let held = lock_for_run(repo.path(), &issue).unwrap();
        let err = lock_for_run(repo.path(), &issue).unwrap_err().to_string();
        assert!(err.contains("refusing to run owner/repo#7"), "{err}");
        assert!(err.contains(&held.path().display().to_string()), "{err}");
        drop(held);
        lock_for_run(repo.path(), &issue).unwrap();
    }

    /// C-M3: a lock taken on a file that was replaced since it was opened
    /// is taken again on the file now at the path, which another controller
    /// holds.
    #[test]
    fn a_lock_file_replaced_before_its_flock_is_taken_again_on_the_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locks/owner-repo-7.lock");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        let other = std::cell::RefCell::new(None);
        let replace = || {
            if other.borrow().is_none() {
                std::fs::remove_file(&path).unwrap();
                *other.borrow_mut() = Some(acquired(try_lock_path(&path).unwrap()));
            }
        };
        match try_lock_path_with(&path, &replace).unwrap() {
            IssueLockAttempt::Held { .. } => {}
            IssueLockAttempt::Acquired(_) => {
                panic!("a lock on the replaced file was taken as the issue's")
            }
        }
    }

    /// Slugs collide; locks do not.
    #[test]
    fn two_issues_whose_slugs_collide_have_two_locks() {
        let a = IssueRef::new("a-b/c", 1);
        let b = IssueRef::new("a/b-c", 1);
        assert_eq!(a.slug(), b.slug());
        let common = Path::new("/r/.git");
        assert_ne!(lock_path_in(common, &a), lock_path_in(common, &b));
        assert!(lock_path_in(common, &a)
            .to_string_lossy()
            .contains("/ironmem/autopilot/locks/a-b-c-1-"));
    }

    /// C-M4: a worktree whose administrative directory was pruned still
    /// finds its repository's common directory, and so its lock.
    #[test]
    fn a_worktree_with_a_broken_link_finds_its_repositorys_common_dir() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            super::super::test_git::output(dir, args);
        };
        git(&repo, &["init", "-q", "--initial-branch=main"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "T"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "seed"]);
        let wt = root.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "x"],
        );
        let common = git_common_dir(&wt).unwrap().unwrap();
        std::fs::remove_dir_all(common.join("worktrees")).unwrap();
        assert_eq!(
            std::fs::canonicalize(git_common_dir(&wt).unwrap().unwrap()).unwrap(),
            std::fs::canonicalize(&common).unwrap()
        );
    }

    #[test]
    fn a_directory_that_is_not_a_repository_has_no_lock() {
        let dir = tempfile::tempdir().unwrap();
        let err = lock_path(dir.path(), &IssueRef::new("owner/repo", 7)).unwrap_err();
        assert!(err.to_string().contains("not in a git repository"), "{err}");
    }
}

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
//! `<git common dir>/ironmem/autopilot/locks/<slug>.lock` before it checks
//! the issue, and holds it until the run ends, dispatch included. One that
//! cannot take it does not provision: the Lead skips the issue for the tick,
//! a manual run refuses, and `clear-dispatch` refuses. The kernel releases a
//! crashed controller's lock with its file descriptor, so nothing stale is
//! ever left to remove.
//!
//! The lock lives in the common directory, which every worktree of a clone
//! shares, so it covers controllers working one clone. Controllers on two
//! clones of one repository sharing a database are not covered; the
//! dispatch state's owner token is their defence.

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
    let mut git = Command::new("git");
    git.args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(repo);
    crate::mcp::tools::scrub_git_environment(&mut git);
    let output = git.output().map_err(|e| {
        MemoryError::Validation(format!(
            "cannot run git to find {}'s lock in {}: {e}",
            issue.canonical(),
            repo.display()
        ))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not a git repository") {
            return Ok(None);
        }
        return Err(MemoryError::Validation(format!(
            "cannot find the git common directory of {} for {}'s lock: {}",
            repo.display(),
            issue.canonical(),
            super::scrub::scrub_and_bound(&stderr, 500).text
        )));
    }
    let common = std::str::from_utf8(&output.stdout)
        .map_err(|_| MemoryError::Validation("the git common directory is not UTF-8".into()))?
        .trim_end_matches(['\n', '\r']);
    Ok(Some(
        Path::new(common)
            .join("ironmem/autopilot/locks")
            .join(format!("{}.lock", issue.slug())),
    ))
}

/// Take `issue`'s lock in the repository at `repo` without waiting.
pub fn try_lock_issue(repo: &Path, issue: &IssueRef) -> Result<IssueLockAttempt, MemoryError> {
    try_lock_path(&lock_path(repo, issue)?)
}

/// Take the lock at `path` without waiting, creating it (0600, in a 0700
/// directory) if needed.
pub(crate) fn try_lock_path(path: &Path) -> Result<IssueLockAttempt, MemoryError> {
    let cannot = |e: std::io::Error| {
        MemoryError::Validation(format!(
            "cannot open the issue lock {}: {e}",
            path.display()
        ))
    };
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
    // SAFETY: `file` owns a valid descriptor for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(IssueLockAttempt::Acquired(IssueLock {
            _file: file,
            path: path.to_path_buf(),
        }));
    }
    let e = std::io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(IssueLockAttempt::Held {
            path: path.to_path_buf(),
        });
    }
    Err(MemoryError::Validation(format!(
        "cannot lock the issue lock {}: {e}",
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

    #[test]
    fn a_directory_that_is_not_a_repository_has_no_lock() {
        let dir = tempfile::tempdir().unwrap();
        let err = lock_path(dir.path(), &IssueRef::new("owner/repo", 7)).unwrap_err();
        assert!(err.to_string().contains("not in a git repository"), "{err}");
    }
}

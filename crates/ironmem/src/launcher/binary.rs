//! Locate an assistant binary on PATH so we can emit a clear "not found" error
//! before attempting to launch. `find_in_paths` takes the PATH value explicitly
//! so tests never mutate the process-global environment (which would race with
//! other tests).

use std::ffi::OsStr;
use std::path::PathBuf;

use crate::error::MemoryError;

/// `name` resolved through [`find_launcher`] and cached for the
/// process, for the spawn sites (git, ps) that run constantly and must
/// never resolve through a relative PATH entry in a worktree. Only a
/// successful resolution is cached: a failed one returns a nonexistent
/// absolute sentinel — never the bare name, which `Command` would
/// re-resolve through the full PATH, relative entries included — so
/// the spawn fails with the operating system's own not-found error and
/// the failure is retried next time, so a PATH fixed after a first
/// failure is picked up. A cached path that stops being executable
/// (removed or superseded after resolution) is dropped and re-resolved.
pub(crate) fn cached_launcher(name: &str) -> PathBuf {
    static GIT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
    static PS: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
    let cell = match name {
        "git" => &GIT,
        "ps" => &PS,
        _ => return PathBuf::from(name),
    };
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    resolve_cached(cell, name, &path_var)
}

/// [`cached_launcher`]'s resolution against an explicit PATH, so tests
/// can drive it without touching the process environment.
fn resolve_cached(
    cell: &std::sync::Mutex<Option<PathBuf>>,
    name: &str,
    path_var: &OsStr,
) -> PathBuf {
    let mut cached = cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(found) = &*cached {
        if is_executable_file(found) {
            return found.clone();
        }
        // The resolved binary was removed or superseded after it was
        // cached; serving the dead path forever would fail every later
        // spawn until the controller restarts.
        *cached = None;
    }
    match find_launcher(name, path_var) {
        Ok(found) => {
            *cached = Some(found.clone());
            found
        }
        Err(_) => PathBuf::from(format!("/nonexistent/ironmem-launcher/{name}")),
    }
}

/// Locate `name` using the process PATH.
pub(crate) fn find_on_path(name: &str) -> Result<PathBuf, MemoryError> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    find_in_paths(name, &path_var)
}

/// Returns true if `p` is a regular file that the current process can execute.
/// On Unix this checks for at least one execute bit; on other platforms the
/// existence of the file is sufficient (Windows uses extension-based dispatch).
#[cfg(unix)]
fn is_executable_file(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(p: &std::path::Path) -> bool {
    p.is_file()
}

/// Locate `name` by scanning the directories in `path_var`. Returns the first
/// existing executable-file match. Pure with respect to the process environment
/// so it is safe to unit-test in parallel.
pub(crate) fn find_in_paths(name: &str, path_var: &OsStr) -> Result<PathBuf, MemoryError> {
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    Err(MemoryError::NotFound(format!(
        "could not find `{name}` on PATH — install it and make sure it is on your PATH"
    )))
}

/// `name` on `path_var`, skipping relative entries. A supervised harness is
/// spawned in the issue worktree, where a relative entry such as `.` would
/// resolve to a file in the repository being worked on. The Claude
/// implementer and the reviewers resolve through it for the same reason.
pub(crate) fn find_launcher(name: &str, path_var: &OsStr) -> Result<PathBuf, MemoryError> {
    let absolute: Vec<_> = std::env::split_paths(path_var)
        .filter(|dir| dir.is_absolute())
        .collect();
    // An empty PATH reads as one empty entry, which would find a bare
    // relative `name` in the worktree: there must be an absolute entry.
    let not_found = || {
        MemoryError::NotFound(format!(
            "could not find `{name}` on any absolute PATH entry; relative entries are skipped"
        ))
    };
    if absolute.is_empty() {
        return Err(not_found());
    }
    let path_var = std::env::join_paths(absolute)
        .map_err(|e| MemoryError::Validation(format!("unusable PATH: {e}")))?;
    match find_in_paths(name, &path_var) {
        Ok(found) if found.is_absolute() => Ok(found),
        Ok(_) | Err(MemoryError::NotFound(_)) => Err(not_found()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    fn make_executable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn finds_binary_in_first_matching_dir() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("claude");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        make_executable(&bin);

        let path_var = std::env::join_paths([dir.path()]).unwrap();
        let found = find_in_paths("claude", &path_var).unwrap();
        assert_eq!(found, bin);
    }

    #[test]
    fn missing_binary_is_a_clear_not_found_error() {
        let dir = tempfile::tempdir().unwrap();
        let path_var = std::env::join_paths([dir.path()]).unwrap();
        let err = find_in_paths("codex", &path_var).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("codex"), "error should name the binary: {msg}");
        assert!(msg.contains("PATH"), "error should mention PATH: {msg}");
    }

    /// The cached resolution skips relative PATH entries (a
    /// worktree could supply the binary), caches only successes, and
    /// picks a PATH fixed after a failure up on the next call.
    #[test]
    fn ultrareview_cached_names_are_independent_and_poison_is_recoverable() {
        assert_eq!(
            cached_launcher("uncached-tool"),
            PathBuf::from("uncached-tool")
        );
        let git = cached_launcher("git");
        let ps = cached_launcher("ps");
        assert_ne!(git, ps);
        assert_eq!(git.file_name().unwrap(), "git");
        assert_eq!(ps.file_name().unwrap(), "ps");
        let cell = std::sync::Mutex::new(None);
        let _ = std::panic::catch_unwind(|| {
            let _held = cell.lock().unwrap();
            panic!("poison fixture");
        });
        assert!(cell.is_poisoned());
        let path = std::env::var_os("PATH").unwrap_or_default();
        assert_eq!(resolve_cached(&cell, "git", &path), git);
    }

    #[cfg(unix)]
    #[test]
    fn cached_resolution_skips_relative_entries_and_caches_only_successes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("git");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();
        let cell = std::sync::Mutex::new(None);
        // Only a relative entry names it: never resolved through it.
        let relative = std::ffi::OsString::from("relative-dir");
        assert_eq!(
            resolve_cached(&cell, "git", &relative),
            PathBuf::from("/nonexistent/ironmem-launcher/git")
        );
        // The failure was not cached: an absolute entry now resolves,
        // and is then served from the cache.
        let absolute = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(resolve_cached(&cell, "git", &absolute), bin);
        let empty = std::ffi::OsString::new();
        assert_eq!(resolve_cached(&cell, "git", &empty), bin);
    }

    /// A cached path that disappears is re-resolved rather than served
    /// forever, and an uncached name passes through untouched.
    #[cfg(unix)]
    #[test]
    fn cached_resolution_recovers_a_vanished_binary() {
        use std::os::unix::fs::PermissionsExt;
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        for dir in [dir_a.path(), dir_b.path()] {
            let bin = dir.join("git");
            fs::write(&bin, "#!/bin/sh\n").unwrap();
            let mut perms = fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&bin, perms).unwrap();
        }
        let cell = std::sync::Mutex::new(None);
        let first = std::env::join_paths([dir_a.path()]).unwrap();
        assert_eq!(
            resolve_cached(&cell, "git", &first),
            dir_a.path().join("git")
        );
        fs::remove_file(dir_a.path().join("git")).unwrap();
        let second = std::env::join_paths([dir_b.path()]).unwrap();
        assert_eq!(
            resolve_cached(&cell, "git", &second),
            dir_b.path().join("git")
        );
        assert_eq!(
            cached_launcher("some-other-tool"),
            PathBuf::from("some-other-tool")
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_file_is_not_selected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // Create a file named "claude" that is NOT executable (mode 0o644).
        let bin = dir.path().join("claude");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&bin, perms).unwrap();

        let path_var = std::env::join_paths([dir.path()]).unwrap();
        let err = find_in_paths("claude", &path_var).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("claude"),
            "error should name the binary: {msg}"
        );
        assert!(msg.contains("PATH"), "error should mention PATH: {msg}");
    }
}

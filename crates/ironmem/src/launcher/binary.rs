//! Locate an assistant binary on PATH so we can emit a clear "not found" error
//! before attempting to launch. `find_in_paths` takes the PATH value explicitly
//! so tests never mutate the process-global environment (which would race with
//! other tests).

use std::ffi::OsStr;
use std::path::PathBuf;

use crate::error::MemoryError;

/// `name` resolved once through [`find_launcher`] and cached for the
/// process, for the spawn sites (git, ps) that run constantly and must
/// never resolve through a relative PATH entry in a worktree. A failed
/// resolution falls back to the bare name, whose spawn then fails with
/// the operating system's own not-found error, as it did before.
pub(crate) fn cached_launcher(name: &str) -> PathBuf {
    static GIT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    static PS: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let cell = match name {
        "git" => &GIT,
        "ps" => &PS,
        _ => return PathBuf::from(name),
    };
    cell.get_or_init(|| {
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        find_launcher(name, &path_var).unwrap_or_else(|_| PathBuf::from(name))
    })
    .clone()
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

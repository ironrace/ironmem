//! Locate an assistant binary on PATH so we can emit a clear "not found" error
//! before attempting to launch. `find_in_paths` takes the PATH value explicitly
//! so tests never mutate the process-global environment (which would race with
//! other tests).

use std::ffi::OsStr;
use std::path::PathBuf;

use crate::error::MemoryError;

/// Locate `name` using the process PATH.
pub(crate) fn find_on_path(name: &str) -> Result<PathBuf, MemoryError> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    find_in_paths(name, &path_var)
}

/// Locate `name` by scanning the directories in `path_var`. Returns the first
/// existing regular-file match. Pure with respect to the process environment so
/// it is safe to unit-test in parallel.
pub(crate) fn find_in_paths(name: &str, path_var: &OsStr) -> Result<PathBuf, MemoryError> {
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(MemoryError::NotFound(format!(
        "could not find `{name}` on PATH — install it and make sure it is on your PATH"
    )))
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
}

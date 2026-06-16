//! v0 freshness engine for lazy per-area code maps (issue #94).
//! Pure logic over a git shell-out boundary. Typed outcomes so workers never
//! fail open and never receive a raw git error.

use std::path::Path;

use crate::db::CodeMap;

/// Typed freshness outcome for a code-map area.
///
/// Workers must act on the variant — they never receive a raw git error.
/// Fail-safe: anything ambiguous maps to `RescoutRequired`, not `Fresh`.
#[derive(Debug, PartialEq)]
pub enum Freshness {
    /// No source files changed since the map was built — use the map as-is.
    Fresh,
    /// At least one source file changed; re-read exactly these files, then refresh the map.
    Stale { changed_files: Vec<String> },
    /// Map cannot be trusted (bad SHA, git failure, outside-repo path, etc.);
    /// discard the map and do a full re-scout.
    RescoutRequired { reason: String },
}

/// Run `git diff --name-only <build_sha>..HEAD` in `repo_root` and return
/// the list of repo-relative changed file paths.
///
/// All arguments are passed as a `Command` arg vector (no shell interpolation).
/// Returns `Err(reason)` on git failure or non-zero exit; callers map this to
/// `RescoutRequired`.
fn changed_files(repo_root: &Path, build_sha: &str) -> Result<Vec<String>, String> {
    // Strict input validation BEFORE any git call: a build_sha is always a hex
    // object name. Rejecting non-hex (and bounding length) here fails fast,
    // avoids spawning git on garbage, and removes any ambiguity about refs or
    // shell-meaningful bytes reaching the subprocess.
    let is_hex_sha = !build_sha.is_empty()
        && build_sha.len() >= 7
        && build_sha.len() <= 64
        && build_sha.chars().all(|c| c.is_ascii_hexdigit());
    if !is_hex_sha {
        return Err("map cannot be verified; re-scout required".to_string());
    }

    let range = format!("{}..HEAD", build_sha);
    // `-c core.quotepath=false` keeps non-ASCII paths un-C-quoted so they match
    // the stored (forward-slash, unquoted) source_files; otherwise a quoted path
    // would fail to intersect and falsely report Fresh (fail-open).
    let output = std::process::Command::new("git")
        .args(["-c", "core.quotepath=false", "diff", "--name-only", &range])
        .current_dir(repo_root)
        .output()
        .map_err(|e| {
            // Log detail server-side; return a generic, leak-free reason.
            eprintln!("code_maps: git exec failed in {repo_root:?}: {e}");
            "map cannot be verified; re-scout required".to_string()
        })?;

    if !output.status.success() {
        // Raw git stderr can carry repo internals / object names — never forward
        // it to the worker. Log server-side, return a generic fail-safe reason.
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!(
            "code_maps: git diff failed ({}) in {repo_root:?}: {}",
            output.status,
            stderr.trim()
        );
        return Err("map cannot be verified; re-scout required".to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(files)
}

/// Classify the freshness of `map` against the current HEAD of `repo_root`.
///
/// Algorithm (v0 SHA diff):
/// 1. Run `git diff --name-only <map.head_sha>..HEAD`.
/// 2. Intersect with `map.source_files`.
/// 3. Empty intersection → `Fresh`; non-empty → `Stale { changed_files }`.
/// 4. Any git error → `RescoutRequired` (fail-safe: never load as Fresh).
///
/// **Path convention:** `git diff --name-only` always emits forward-slash-
/// separated paths. `CodeMap::source_files` MUST also use forward slashes
/// for the intersection to work correctly. Writers (e.g. `code_map_write`)
/// must normalise paths to forward slashes before storing them.
pub fn classify(map: &CodeMap, repo_root: &Path) -> Freshness {
    match changed_files(repo_root, &map.head_sha) {
        Err(reason) => Freshness::RescoutRequired { reason },
        Ok(diff) => {
            let source_set: std::collections::HashSet<&str> =
                map.source_files.iter().map(|s| s.as_str()).collect();
            let mut intersection: Vec<String> = diff
                .iter()
                .filter(|f| source_set.contains(f.as_str()))
                .cloned()
                .collect();
            intersection.sort();
            if intersection.is_empty() {
                Freshness::Fresh
            } else {
                Freshness::Stale {
                    changed_files: intersection,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CodeMap;
    use std::path::{Path, PathBuf};

    fn make_git_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .output()
            .unwrap();
        (dir, root)
    }

    fn commit_file(root: &Path, path: &str, content: &str) -> String {
        let full_path = root.join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full_path, content).unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "test commit"])
            .current_dir(root)
            .output()
            .unwrap();
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn dummy_code_map(head_sha: &str, source_files: Vec<String>) -> CodeMap {
        CodeMap {
            repo: "test-repo".to_string(),
            area: "core".to_string(),
            drawer_id: "drawer-001".to_string(),
            head_sha: head_sha.to_string(),
            source_files,
            built_by: "test-agent".to_string(),
            built_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    /// 1. No change in source files since build → Fresh.
    ///    Commit "a.rs" as build_sha, then change only "b.rs" (not tracked).
    #[test]
    fn test_fresh_when_no_change_in_source_files() {
        let (_dir, root) = make_git_repo();
        let build_sha = commit_file(&root, "a.rs", "fn main() {}");
        // Change a file NOT in source_files
        commit_file(&root, "b.rs", "fn helper() {}");

        let map = dummy_code_map(&build_sha, vec!["a.rs".to_string()]);
        let result = classify(&map, &root);
        assert_eq!(result, Freshness::Fresh);
    }

    /// 2. Tracked source file changed → Stale with that file listed.
    #[test]
    fn test_stale_when_tracked_file_changed() {
        let (_dir, root) = make_git_repo();
        let build_sha = commit_file(&root, "src/lib.rs", "// v1");
        // Also commit main.rs to simulate realistic multi-file repo
        commit_file(&root, "main.rs", "fn main() {}");
        // Modify the tracked source file
        commit_file(&root, "src/lib.rs", "// v2 changed");

        let map = dummy_code_map(&build_sha, vec!["src/lib.rs".to_string()]);
        let result = classify(&map, &root);
        assert_eq!(
            result,
            Freshness::Stale {
                changed_files: vec!["src/lib.rs".to_string()]
            }
        );
    }

    /// 3. Invalid build SHA → RescoutRequired.
    #[test]
    fn test_rescout_on_invalid_build_sha() {
        let (_dir, root) = make_git_repo();
        // Need at least one commit so HEAD exists
        commit_file(&root, "seed.rs", "// seed");

        let invalid_sha = "deadbeef000000000000000000000000000000de";
        let map = dummy_code_map(invalid_sha, vec!["seed.rs".to_string()]);
        let result = classify(&map, &root);
        assert!(
            matches!(result, Freshness::RescoutRequired { .. }),
            "expected RescoutRequired, got {:?}",
            result
        );
    }

    /// 4. Source file deleted → Stale (deleted file appears in git diff).
    #[test]
    fn test_stale_when_source_file_deleted() {
        let (_dir, root) = make_git_repo();
        let build_sha = commit_file(&root, "foo.rs", "// original");
        // Delete the file and commit
        std::fs::remove_file(root.join("foo.rs")).unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "delete foo.rs"])
            .current_dir(&root)
            .output()
            .unwrap();

        let map = dummy_code_map(&build_sha, vec!["foo.rs".to_string()]);
        let result = classify(&map, &root);
        assert_eq!(
            result,
            Freshness::Stale {
                changed_files: vec!["foo.rs".to_string()]
            }
        );
    }

    /// 5. Only changes outside source_files → Fresh (verifies intersection logic).
    ///    source_files=["a.rs"], only "b.rs" changed → Fresh.
    #[test]
    fn test_fresh_when_only_untracked_change_outside_source_files() {
        let (_dir, root) = make_git_repo();
        let build_sha = commit_file(&root, "a.rs", "// a original");
        // Change b.rs which is NOT in source_files
        commit_file(&root, "b.rs", "// b changed");

        let map = dummy_code_map(&build_sha, vec!["a.rs".to_string()]);
        let result = classify(&map, &root);
        assert_eq!(result, Freshness::Fresh);
    }
}

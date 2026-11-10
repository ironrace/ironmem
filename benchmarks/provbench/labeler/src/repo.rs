//! Pilot repo handling: open an already-cloned repo and walk first-parent
//! commits from T₀ forward. Cloning itself is left to a CLI step (or to
//! the user) — the labeler refuses to mutate git state.

use anyhow::{anyhow, Context, Result};
use gix::ObjectId;
use std::path::{Path, PathBuf};

#[cfg(test)]
static READ_BLOB_AT_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_read_blob_at_call_count() {
    READ_BLOB_AT_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn read_blob_at_call_count() -> usize {
    READ_BLOB_AT_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Pinned pilot repo per SPEC §13.2.
pub const RIPGREP_T0_SHA: &str = "af6b6c543b224d348a8876f0c06245d9ea7929c5";
pub const RIPGREP_URL: &str = "https://github.com/BurntSushi/ripgrep";

/// Stable, repo-relative string form of a git tree path, suitable for
/// embedding in a `fact_id`.
///
/// This helper is **pure**: it performs zero filesystem I/O. The input
/// is treated as a repo-relative path produced by `git ls-tree` (which
/// is always forward-slash separated), but on Windows a `PathBuf` may
/// internally use `\\`; we normalize those to `/` so the resulting
/// `fact_id` is stable across platforms.
///
/// The single allowed `Path::canonicalize` call lives in [`Pilot::open`]
/// and applies only to the repo root; per-fact paths must stay string-only
/// or T₀ replay determinism breaks for files that have since been moved
/// or deleted.
pub(crate) fn normalize_path_for_fact_id(rel_git_path: &Path) -> String {
    let s = rel_git_path.to_string_lossy();
    if s.contains('\\') {
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
}

pub trait PilotRepoSpec {
    fn local_clone_path(&self) -> &Path;
    fn t0_sha(&self) -> &str;
}

#[derive(Debug, Clone)]
pub struct Ripgrep {
    pub clone_path: PathBuf,
}

impl PilotRepoSpec for Ripgrep {
    fn local_clone_path(&self) -> &Path {
        &self.clone_path
    }
    fn t0_sha(&self) -> &str {
        RIPGREP_T0_SHA
    }
}

#[derive(Debug, Clone)]
pub struct CommitRef {
    pub sha: String,
    pub parent_sha: Option<String>,
}

/// An opened pilot repository handle plus its canonicalized root and
/// resolved T₀ object id.
///
/// `repo_path` is the result of the **single** filesystem-resolving
/// canonicalization the labeler performs (see [`Pilot::open`]); every
/// per-fact path used downstream is normalized purely lexically (see
/// the crate-private `normalize_path_for_fact_id` helper) so T₀ replay
/// determinism is preserved for files that have since been moved or
/// deleted.
pub struct Pilot {
    repo: gix::Repository,
    repo_path: PathBuf,
    t0_sha: ObjectId,
}

impl Pilot {
    /// Open an already-cloned pilot repo and resolve its T₀ commit.
    ///
    /// This is the **single** site in the labeler that calls
    /// `Path::canonicalize`; the resulting `repo_path` is stored once
    /// and all per-fact paths are normalized purely lexically thereafter
    /// (via the crate-private `normalize_path_for_fact_id` helper) so
    /// that fact ids stay deterministic across runs and across hosts
    /// even when source files have been moved or deleted since T₀.
    pub fn open<S: PilotRepoSpec>(spec: &S) -> Result<Self> {
        // Canonicalize the user-supplied repo root exactly once. This is the
        // ONLY filesystem-resolving canonicalization in the labeler; per-fact
        // paths are normalized purely lexically by `normalize_path_for_fact_id`
        // so that T₀ replay stays deterministic for moved/deleted files.
        let raw_path = spec.local_clone_path();
        let repo_path = raw_path
            .canonicalize()
            .with_context(|| format!("canonicalize pilot repo root {}", raw_path.display()))?;
        let repo = gix::open(&repo_path)
            .with_context(|| format!("open pilot repo at {}", repo_path.display()))?;
        let t0_sha = ObjectId::from_hex(spec.t0_sha().as_bytes())
            .with_context(|| format!("parse t0 sha {}", spec.t0_sha()))?;
        repo.find_object(t0_sha)
            .with_context(|| format!("t0 commit {} not present in clone", spec.t0_sha()))?;
        Ok(Self {
            repo,
            repo_path,
            t0_sha,
        })
    }

    /// Return the filesystem path the repo was opened from.
    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    /// Walk first-parent linear history from T₀ forward up to HEAD.
    /// Returns commits in chronological order (oldest first).
    pub fn walk_first_parent(&self) -> Result<impl Iterator<Item = CommitRef> + '_> {
        let head = self.repo.head_commit().context("resolve HEAD")?;
        // Store (commit_id, first_parent_id) so we don't re-read from the ODB
        // in the output map and can avoid any silent error swallowing.
        let mut chain: Vec<(ObjectId, Option<ObjectId>)> = Vec::new();
        // head.id is the public ObjectId field on Commit<'_>
        let mut cur = Some(head.id);
        while let Some(id) = cur {
            let commit = self
                .repo
                .find_commit(id)
                .with_context(|| format!("walk: find {id}"))?;
            // parent_ids() yields gix::Id<'_>; detach() extracts the inner ObjectId
            let parent_id = commit.parent_ids().next().map(|p| p.detach());
            chain.push((id, parent_id));
            if id == self.t0_sha {
                break;
            }
            cur = parent_id;
        }
        if chain.last().map(|(id, _)| id) != Some(&self.t0_sha) {
            return Err(anyhow!(
                "first-parent chain from HEAD does not contain T₀ {}; rebased history?",
                self.t0_sha
            ));
        }
        chain.reverse();
        Ok(chain.into_iter().map(|(id, parent)| CommitRef {
            sha: id.to_string(),
            parent_sha: parent.map(|p| p.to_string()),
        }))
    }

    pub fn read_blob_at(&self, commit_sha: &str, path: &Path) -> Result<Option<Vec<u8>>> {
        #[cfg(test)]
        READ_BLOB_AT_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let id = ObjectId::from_hex(commit_sha.as_bytes())
            .with_context(|| format!("read_blob_at: invalid sha {commit_sha}"))?;
        let commit = self
            .repo
            .find_commit(id)
            .with_context(|| format!("read_blob_at: commit {commit_sha} not in repo"))?;
        let tree = commit
            .tree()
            .with_context(|| format!("read_blob_at: tree for {commit_sha}"))?;
        let entry = tree
            .lookup_entry_by_path(path)
            .with_context(|| format!("read_blob_at: lookup {} @ {commit_sha}", path.display()))?;
        match entry {
            None => Ok(None),
            Some(e) => {
                let obj = e.object().with_context(|| {
                    format!(
                        "read_blob_at: load object for {} @ {commit_sha}",
                        path.display()
                    )
                })?;
                Ok(Some(obj.data.clone()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_for_fact_id_preserves_utf8_and_spaces_without_filesystem() {
        // The helper is documented as pure: zero filesystem I/O. Use a path
        // that does not exist anywhere to make sure no canonicalize() syscall
        // is hiding inside.
        let weird = PathBuf::from("does/not/exist/héllo wörld/файл.rs");
        let out = normalize_path_for_fact_id(&weird);
        assert_eq!(out, "does/not/exist/héllo wörld/файл.rs");

        let with_spaces = PathBuf::from("a b/c d.rs");
        assert_eq!(normalize_path_for_fact_id(&with_spaces), "a b/c d.rs");

        // git ls-tree paths are already forward-slashed; the helper must
        // be a no-op on those.
        let unix_like = PathBuf::from("src/lib.rs");
        assert_eq!(normalize_path_for_fact_id(&unix_like), "src/lib.rs");
    }

    #[test]
    fn normalize_path_for_fact_id_normalizes_backslashes_to_forward_slashes() {
        // PathBuf::from on Unix won't produce backslash separators — but the
        // helper is also documented to normalize them so Windows PathBufs
        // round-trip correctly. Verify the string-level transformation.
        let raw = PathBuf::from("src\\sub\\file.rs");
        let out = normalize_path_for_fact_id(&raw);
        assert!(
            !out.contains('\\'),
            "expected no backslash in fact_id segment: {out}"
        );
        assert_eq!(out, "src/sub/file.rs");
    }
}

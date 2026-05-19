//! Single-repo HEAD-only reader: open the repo, read a blob at a commit,
//! check file existence at a commit.

use anyhow::{Context, Result};
use gix::ObjectId;
use std::path::Path;

pub struct Repo {
    inner: gix::Repository,
}

impl Repo {
    pub fn open(path: &Path) -> Result<Self> {
        let inner = gix::open(path).with_context(|| format!("opening repo {}", path.display()))?;
        Ok(Self { inner })
    }

    /// Verify that `commit_sha` resolves to an actual commit in this repo.
    /// Errors loudly if the sha is malformed OR points at a non-existent
    /// or non-commit object. Use this at the top of any pipeline that
    /// reads blobs at `commit_sha` — otherwise `blob_at` would silently
    /// return `Ok(None)` for every read and produce wrong results without
    /// any error signal.
    pub fn verify_commit(&self, commit_sha: &str) -> Result<()> {
        let oid = ObjectId::from_hex(commit_sha.as_bytes())
            .with_context(|| format!("commit sha {commit_sha:?} is not a valid hex sha"))?;
        let obj = self
            .inner
            .find_object(oid)
            .with_context(|| format!("commit {commit_sha} does not exist in this repository"))?;
        obj.try_into_commit()
            .with_context(|| format!("object {commit_sha} exists but is not a commit"))?;
        Ok(())
    }

    pub fn file_exists_at(&self, commit_sha: &str, source_path: &str) -> Result<bool> {
        Ok(self.blob_at(commit_sha, source_path)?.is_some())
    }

    pub fn blob_at(&self, commit_sha: &str, source_path: &str) -> Result<Option<Vec<u8>>> {
        let oid = ObjectId::from_hex(commit_sha.as_bytes())
            .with_context(|| format!("parsing commit sha {}", commit_sha))?;
        let commit = match self.inner.find_object(oid) {
            Ok(o) => o.try_into_commit().context("not a commit")?,
            Err(_) => return Ok(None),
        };
        let tree = commit.tree().context("commit has no tree")?;
        let mut buf = Vec::new();
        let entry = match tree.lookup_entry_by_path(source_path, &mut buf)? {
            Some(e) => e,
            None => return Ok(None),
        };
        let obj = entry.object()?;
        Ok(Some(obj.data.clone()))
    }

    /// Recursively list all blob (file) paths under the commit's tree.
    /// Used by R7 (rename candidate) for whole-tree similarity search.
    pub fn list_tree(&self, commit_sha: &str) -> Result<Vec<String>> {
        let oid = ObjectId::from_hex(commit_sha.as_bytes())
            .with_context(|| format!("parsing commit sha {}", commit_sha))?;
        let commit = match self.inner.find_object(oid) {
            Ok(o) => o.try_into_commit().context("not a commit")?,
            Err(_) => return Ok(Vec::new()),
        };
        let tree = commit.tree().context("commit has no tree")?;
        let mut out = Vec::new();
        walk_tree(&self.inner, &tree, "", &mut out)?;
        out.sort();
        Ok(out)
    }
}

fn walk_tree(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    prefix: &str,
    out: &mut Vec<String>,
) -> Result<()> {
    for entry in tree.iter() {
        let entry = entry?;
        let name = entry.filename().to_string();
        let full = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", prefix, name)
        };
        let mode = entry.mode();
        if mode.is_tree() {
            let child = repo.find_object(entry.object_id())?;
            let child_tree = child.try_into_tree().context("expected tree object")?;
            walk_tree(repo, &child_tree, &full, out)?;
        } else if mode.is_blob() {
            out.push(full);
        }
    }
    Ok(())
}

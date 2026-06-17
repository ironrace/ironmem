//! Corpus schema, deterministic load + validation, canonical content hash.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::constants::{CORPUS_MAX, CORPUS_MIN, SOURCE_PREFIXES};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Basename-safe stable identifier: ASCII alphanumeric, `-`, or `_`.
    pub id: String,
    pub title: String,
    /// Real-reference form: `issue:#NN`, `pr:#NN`, or `backlog:<ref>`.
    pub source: String,
    pub repo_scope: Vec<String>,
    pub prompt: String,
    pub acceptance: Vec<String>,
    pub gates: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_notes: Option<String>,
    /// Pinned git base commit the live workspace is provisioned at (REQUIRED).
    /// Hex object ref, length 7..=40. Reproducibility: prefer a full 40-char SHA.
    pub base_commit: BaseCommit,
}

/// Validated git base-commit ref. The smart constructor [`BaseCommit::parse`] is
/// the SINGLE place the hex/length (7..=40) predicate lives — there is no other
/// validity check for base commits anywhere in the crate.
///
/// On-disk JSON shape is an unadorned string: it deserializes via
/// `#[serde(try_from = "String")]` (validating on load) and serializes
/// transparently as its inner `String`, so the frozen corpus content hash is
/// byte-identical to the plain-string form.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct BaseCommit(String);

// Serialize transparently as the bare inner string. `#[serde(transparent)]`
// cannot be combined with `try_from`, so the serialize half is hand-written to
// keep the on-disk shape (and thus the corpus content hash) a plain string.
impl Serialize for BaseCommit {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl BaseCommit {
    /// Parse and validate a base commit: non-empty hex of git-object-ref length
    /// (7..=40). Surrounding whitespace is trimmed before validation. Network/repo
    /// existence is validated at provision time, not here.
    pub fn parse(s: &str) -> Result<Self> {
        let trimmed = s.trim();
        if !((7..=40).contains(&trimmed.len()) && trimmed.bytes().all(|b| b.is_ascii_hexdigit())) {
            bail!(
                "invalid base_commit {:?} (expected hex git ref of length 7..=40)",
                s
            );
        }
        Ok(BaseCommit(trimmed.to_string()))
    }

    /// The explicit "no pin set" sentinel: an empty inner ref. The on-disk
    /// corpus always carries a real pin (the `try_from`/`validate_corpus` path
    /// rejects empty), so this is only reachable for hand-built `Task`s whose
    /// base is supplied at run time via `--base-sha`. `resolve_base_commit`
    /// treats an empty ref as "no task pin".
    pub fn unset() -> Self {
        BaseCommit(String::new())
    }

    /// The validated inner ref (empty only for [`BaseCommit::unset`]).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BaseCommit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for BaseCommit {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        BaseCommit::parse(&value)
    }
}

/// Load tasks from a JSONL file (one Task object per non-blank line).
pub fn load_corpus(path: impl AsRef<Path>) -> Result<Vec<Task>> {
    let path = resolve_corpus_path(path.as_ref());
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("reading corpus {}", path.display()))?;
    let mut tasks = Vec::new();
    for (i, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let task: Task =
            serde_json::from_str(line).with_context(|| format!("parsing corpus line {}", i + 1))?;
        tasks.push(task);
    }
    Ok(tasks)
}

fn resolve_corpus_path(path: &Path) -> PathBuf {
    if path.is_absolute() || path.exists() {
        return path.to_path_buf();
    }
    let crate_relative = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    if crate_relative.exists() {
        return crate_relative;
    }
    path.to_path_buf()
}

/// Enforce the §11.1 corpus invariants (README invariants 1-5). Returns Err on
/// the first breach.
pub fn validate_corpus(tasks: &[Task]) -> Result<()> {
    if tasks.len() < CORPUS_MIN || tasks.len() > CORPUS_MAX {
        bail!(
            "corpus size {} out of bounds [{CORPUS_MIN}, {CORPUS_MAX}]",
            tasks.len()
        );
    }
    let mut seen = std::collections::HashSet::new();
    for t in tasks {
        if t.id.trim().is_empty() {
            bail!("task has empty id");
        }
        if !is_safe_task_id(&t.id) {
            bail!(
                "task {} has unsafe id (use ASCII letters, digits, '-' or '_')",
                t.id
            );
        }
        if !seen.insert(t.id.as_str()) {
            bail!("duplicate task id: {}", t.id);
        }
        if t.acceptance.is_empty() {
            bail!("task {} has no acceptance criteria", t.id);
        }
        if t.gates.is_empty() {
            bail!("task {} has no gates", t.id);
        }
        // `t.base_commit` is a validated `BaseCommit` newtype: validity is
        // enforced at construction (`BaseCommit::parse`) / on-disk deserialization
        // (`#[serde(try_from = "String")]`), so no re-derivation is needed here.
        if !SOURCE_PREFIXES.iter().any(|p| t.source.starts_with(p)) {
            bail!(
                "task {} has non-real source {:?} (must start with one of {:?})",
                t.id,
                t.source,
                SOURCE_PREFIXES
            );
        }
    }
    Ok(())
}

/// Basename-safe id check: ASCII letters, digits, `-`, or `_` only.
/// Used both at corpus validation and as a defense-in-depth guard before any
/// id is interpolated into an on-disk output path (prevents `..`/separator
/// traversal from a hand-built `RunArgs` that bypassed `validate_corpus`).
pub(crate) fn is_safe_task_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Deterministic content hash over the canonicalized corpus.
/// Tasks are sorted by id; each is serialized with sorted keys, then hashed in order.
pub fn content_hash(tasks: &[Task]) -> String {
    let mut sorted: Vec<&Task> = tasks.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let mut hasher = Sha256::new();
    for t in sorted {
        let canon = canonical_json(t).expect("Task serializes");
        hasher.update(canon.as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

/// Serialize a Task to canonical JSON with sorted object keys.
fn canonical_json(task: &Task) -> Result<String> {
    let value = serde_json::to_value(task)?;
    let canon = to_canonical(&value);
    serde_json::to_string(&canon).map_err(|e| anyhow!(e))
}

fn to_canonical(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for k in keys {
                sorted.insert(k.clone(), to_canonical(&map[k]));
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(to_canonical).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod base_commit_tests {
    use super::*;

    #[test]
    fn parse_rejects_empty_base_commit() {
        let err = BaseCommit::parse("").unwrap_err().to_string();
        assert!(err.contains("invalid base_commit"), "got: {err}");
    }

    #[test]
    fn parse_rejects_non_hex_base_commit() {
        let err = BaseCommit::parse("not-a-sha-zzzz").unwrap_err().to_string();
        assert!(err.contains("invalid base_commit"), "got: {err}");
    }

    #[test]
    fn parse_rejects_too_short_base_commit() {
        let err = BaseCommit::parse("abc").unwrap_err().to_string();
        assert!(err.contains("invalid base_commit"), "got: {err}");
    }

    #[test]
    fn parse_accepts_valid_full_sha() {
        let bc = BaseCommit::parse("abcdef1234567890abcdef1234567890abcdef12")
            .expect("valid base_commit accepted");
        assert_eq!(bc.as_str(), "abcdef1234567890abcdef1234567890abcdef12");
    }

    #[test]
    fn parse_trims_surrounding_whitespace() {
        let bc = BaseCommit::parse("  abcdef1  ").expect("trimmed valid ref accepted");
        assert_eq!(bc.as_str(), "abcdef1");
    }

    #[test]
    fn deserialize_validates_via_try_from() {
        // On-disk JSON shape is a plain string; an invalid value must fail to
        // deserialize (the `try_from = "String"` path), not silently construct.
        let ok: BaseCommit =
            serde_json::from_str("\"abcdef1234567890abcdef1234567890abcdef12\"").unwrap();
        assert_eq!(ok.as_str(), "abcdef1234567890abcdef1234567890abcdef12");

        let err = serde_json::from_str::<BaseCommit>("\"zzz\"").unwrap_err();
        assert!(
            err.to_string().contains("invalid base_commit"),
            "got: {err}"
        );
    }

    #[test]
    fn serialize_is_transparent_plain_string() {
        // The content hash depends on this: a BaseCommit must serialize as the
        // bare inner string, not as a wrapper object.
        let bc = BaseCommit::parse("abcdef1").unwrap();
        assert_eq!(serde_json::to_string(&bc).unwrap(), "\"abcdef1\"");
    }
}

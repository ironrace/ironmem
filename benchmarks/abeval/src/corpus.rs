//! Corpus schema, deterministic load + validation, canonical content hash.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::constants::{CORPUS_MAX, CORPUS_MIN, SOURCE_PREFIXES};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
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
}

/// Load tasks from a JSONL file (one Task object per non-blank line).
pub fn load_corpus(path: impl AsRef<Path>) -> Result<Vec<Task>> {
    let path = path.as_ref();
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading corpus {}", path.display()))?;
    let mut tasks = Vec::new();
    for (i, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let task: Task = serde_json::from_str(line)
            .with_context(|| format!("parsing corpus line {}", i + 1))?;
        tasks.push(task);
    }
    Ok(tasks)
}

/// Enforce the §11.1 / §2.2 invariants. Returns Err on the first breach.
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
        if !seen.insert(t.id.as_str()) {
            bail!("duplicate task id: {}", t.id);
        }
        if t.acceptance.is_empty() {
            bail!("task {} has no acceptance criteria", t.id);
        }
        if t.gates.is_empty() {
            bail!("task {} has no gates", t.id);
        }
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

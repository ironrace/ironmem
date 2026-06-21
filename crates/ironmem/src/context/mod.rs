//! Task context packs: assemble a compact, bounded view of what an agent
//! should start a coding session with — relevant memory, known decisions, and
//! per-area code-map freshness — from existing memory/code-map primitives.

use std::path::PathBuf;

use serde::Serialize;

use crate::error::MemoryError;
use crate::mcp::app::App;

pub mod render;

/// Default token budget when the caller does not pass `--budget`.
pub const DEFAULT_BUDGET_TOKENS: usize = 2000;

/// Inputs for a single context-pack request.
#[derive(Debug, Clone)]
pub struct ContextPackOptions {
    /// Repository root used for code-map lookup (canonicalized internally).
    pub repo: PathBuf,
    /// Free-form task description driving memory recall.
    pub task: String,
    /// Requested code-map areas (raw, as typed). May be empty.
    pub areas: Vec<String>,
    /// Approximate output token budget.
    pub budget_tokens: usize,
}

/// A single recalled memory drawer, trimmed for display.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemoryHit {
    pub id: String,
    pub wing: String,
    pub room: String,
    pub score: f32,
    pub snippet: String,
}

/// A known decision recalled from the knowledge graph.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DecisionHit {
    pub subject: String,
    pub predicate: String,
    pub object: String,
}

/// Freshness-tagged context for one requested area.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AreaStatus {
    Fresh {
        head_sha: String,
        source_file_count: usize,
        summary: String,
    },
    Stale {
        head_sha: String,
        changed_files: Vec<String>,
        refresh_recommendation: String,
    },
    Missing {
        reason: String,
    },
}

/// One requested area paired with its resolved status.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AreaContext {
    pub area: String,
    #[serde(flatten)]
    pub status: AreaStatus,
}

/// The assembled, bounded context pack.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ContextPack {
    pub task: String,
    pub repo: String,
    pub budget_tokens: usize,
    pub memory_hits: Vec<MemoryHit>,
    pub decisions: Vec<DecisionHit>,
    pub areas: Vec<AreaContext>,
    /// True when budget bounding dropped one or more memory hits.
    pub truncated: bool,
}

/// Assemble a context pack. Later tasks fill in memory/decisions/areas; this
/// skeleton returns an empty-but-well-formed pack.
pub fn run_context(app: &App, opts: &ContextPackOptions) -> Result<ContextPack, MemoryError> {
    let repo = canonical_repo(&opts.repo);
    let _ = app; // used by later tasks
    Ok(ContextPack {
        task: opts.task.clone(),
        repo,
        budget_tokens: opts.budget_tokens,
        memory_hits: Vec::new(),
        decisions: Vec::new(),
        areas: Vec::new(),
        truncated: false,
    })
}

/// Canonicalize the repo path for code-map lookup; fall back to the raw
/// lossy path when canonicalization fails (e.g. path does not exist).
fn canonical_repo(repo: &std::path::Path) -> String {
    std::fs::canonicalize(repo)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| repo.to_string_lossy().into_owned())
}

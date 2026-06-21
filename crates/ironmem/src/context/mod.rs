//! Task context packs: assemble a compact, bounded view of what an agent
//! should start a coding session with — relevant memory, known decisions, and
//! per-area code-map freshness — from existing memory/code-map primitives.

use std::path::PathBuf;

use serde::Serialize;

use crate::code_maps::freshness::{classify, Freshness};
use crate::error::MemoryError;
use crate::mcp::app::App;
use crate::sanitize::sanitize_name;

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
    let repo_path = std::path::Path::new(&repo);
    let areas: Vec<AreaContext> = opts
        .areas
        .iter()
        .map(|raw| AreaContext {
            area: raw.clone(),
            status: resolve_area(app, &repo, repo_path, raw),
        })
        .collect();
    Ok(ContextPack {
        task: opts.task.clone(),
        repo,
        budget_tokens: opts.budget_tokens,
        memory_hits: Vec::new(),
        decisions: Vec::new(),
        areas,
        truncated: false,
    })
}

/// Resolve one requested area into its freshness-tagged status.
fn resolve_area(app: &App, repo: &str, repo_path: &std::path::Path, raw_area: &str) -> AreaStatus {
    let area = match sanitize_name(raw_area, "area") {
        Ok(a) => a,
        Err(_) => {
            return AreaStatus::Missing {
                reason: format!(
                    "invalid area name '{raw_area}'; scout required (areas are short names, no slashes)"
                ),
            }
        }
    };

    let map = match app.db.get_code_map(repo, &area) {
        Ok(Some(m)) => m,
        Ok(None) => {
            return AreaStatus::Missing {
                reason: format!("no code map for area '{area}'; scout required"),
            }
        }
        Err(e) => {
            return AreaStatus::Missing {
                reason: format!("code map lookup failed for '{area}': {e}; scout required"),
            }
        }
    };

    match classify(&map, repo_path) {
        Freshness::Fresh => {
            let summary = app
                .db
                .get_drawer(&map.drawer_id)
                .ok()
                .flatten()
                .map(|d| d.content)
                .unwrap_or_default();
            AreaStatus::Fresh {
                head_sha: short_sha(&map.head_sha),
                source_file_count: map.source_files.len(),
                summary,
            }
        }
        Freshness::Stale { changed_files } => AreaStatus::Stale {
            head_sha: short_sha(&map.head_sha),
            changed_files,
            refresh_recommendation:
                "source files changed since this map was built; re-scout this area before trusting it"
                    .to_string(),
        },
        Freshness::RescoutRequired { reason } => AreaStatus::Missing {
            reason: format!("{reason}; scout required"),
        },
    }
}

/// First 7 characters of a SHA, or the whole string if shorter.
fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Canonicalize the repo path for code-map lookup; fall back to the raw
/// lossy path when canonicalization fails (e.g. path does not exist).
fn canonical_repo(repo: &std::path::Path) -> String {
    std::fs::canonicalize(repo)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| repo.to_string_lossy().into_owned())
}

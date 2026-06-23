//! Immutable data types for the symbol/import graph index.
//!
//! These mirror the DB-layer structs but are the public API types used
//! by the indexer, parser, and MCP/CLI layers.

use serde::{Deserialize, Serialize};

// ── Constants (re-exported from db layer) ─────────────────────────────────────

pub use crate::db::symbol_graph::MAX_SNIPPET_LEN;

// ── Parser output types ───────────────────────────────────────────────────────

/// A parsed symbol declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedSymbol {
    /// Short/local name (e.g. `parse`, `MyClass`).
    pub name: String,
    /// Qualified name within the file (e.g. `outer_mod::inner_fn`).
    /// For v0, this is the nesting path using `::` separators.
    pub qualified_name: String,
    /// Symbol kind: fn | struct | enum | trait | impl | mod | const | static
    ///              | type | macro | class | method
    pub kind: String,
    /// Visibility string (pub, pub(crate), pub(super), private).
    pub visibility: Option<String>,
    /// Declaration header, truncated to `MAX_SNIPPET_LEN`.
    pub signature: Option<String>,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: Option<u32>,
    /// Qualified name of the parent symbol (if nested).
    pub parent_qualified_name: Option<String>,
    /// Heuristic confidence [0.0, 1.0].
    pub confidence: f64,
}

/// A parsed import statement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedImport {
    /// Module path (e.g. `std::collections`, `os`).
    pub module: String,
    /// Specific symbol imported (e.g. `HashMap`). None for glob or module import.
    pub symbol: Option<String>,
    /// Local alias (e.g. `use foo as bar` → `bar`).
    pub alias: Option<String>,
    /// Original import line, truncated to `MAX_SNIPPET_LEN`.
    pub raw: Option<String>,
    pub line: u32,
    pub confidence: f64,
}

/// Output of parsing one source file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedFile {
    /// Detected language ("rust", "python", or "unknown").
    pub language: String,
    pub symbols: Vec<ParsedSymbol>,
    pub imports: Vec<ParsedImport>,
    /// Per-file warnings (e.g. unsupported extension).
    pub warnings: Vec<String>,
}

impl ParsedFile {
    /// Construct an empty result for an unsupported file extension.
    pub fn unsupported(ext: &str) -> Self {
        ParsedFile {
            language: "unknown".to_string(),
            symbols: vec![],
            imports: vec![],
            warnings: vec![format!(
                "unsupported file extension: '{ext}' (v0 supports .rs and .py only)"
            )],
        }
    }
}

// ── Index result types ────────────────────────────────────────────────────────

/// Counts returned from a completed `index_repo` run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexResult {
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub files_purged: usize,
    pub symbols_inserted: usize,
    pub imports_inserted: usize,
    pub edges_inserted: usize,
    /// True when git HEAD was resolved to a real commit SHA.
    pub head_resolved: bool,
    pub head_sha: String,
}

// ── Lookup response types ─────────────────────────────────────────────────────

pub use crate::db::symbol_graph::{CodeImport, CodeSymbol, CodeSymbolEdge, IndexFile};

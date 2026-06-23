//! Local symbol/import graph index for code-aware retrieval.
//!
//! v0 scope: Rust (.rs) and Python (.py) only. TypeScript/JavaScript
//! are explicitly unsupported. Parsers are regex/heuristic.
//!
//! ## Storage
//! SQLite tables: `code_index_files`, `code_symbols`, `code_imports`,
//! `code_symbol_edges` (migration 012). No full source bodies are persisted —
//! only bounded declaration metadata (`signature`/`raw`, ≤ `MAX_SNIPPET_LEN`
//! bytes each).
//!
//! ## Edge scope (v0)
//! - `import`: file → module (from import/use statements)
//! - `contains`: symbol → parent symbol (nesting)
//! Cross-symbol call/reference resolution is deferred (table created now
//! for forward-compatibility).
//!
//! ## Retrieval integration
//! None in this batch — symbol lookup is a standalone surface. Integration
//! with `ironmem context` / search ranking is a documented follow-up.

pub mod index;
pub mod lookup;
pub mod model;
pub mod parse;

/// Public re-exports for CLI and MCP layers.
pub use index::{canonicalize_repo, index_repo, validate_path_within_repo};
pub use lookup::{lookup_imports, lookup_neighbors, lookup_symbols, DEFAULT_LOOKUP_LIMIT};
pub use model::{IndexResult, ParsedFile};

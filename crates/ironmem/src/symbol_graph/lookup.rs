//! Symbol/import/neighbor query helpers for the symbol graph index.
//!
//! All queries are bounded by an explicit limit. Results are returned as
//! slices of the DB-layer structs; no raw source content is leaked.

use crate::db::schema::Database;
use crate::db::symbol_graph::{CodeImport, CodeSymbol, CodeSymbolEdge};
use crate::error::MemoryError;

/// Default result limit for all lookup operations.
pub const DEFAULT_LOOKUP_LIMIT: usize = 50;
/// Hard cap — callers cannot request more than this many results.
pub const MAX_LOOKUP_LIMIT: usize = 200;

fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LOOKUP_LIMIT).min(MAX_LOOKUP_LIMIT)
}

/// Look up symbols by name or qualified name.
pub fn lookup_symbols(
    db: &Database,
    repo: &str,
    query: &str,
    kind: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<CodeSymbol>, MemoryError> {
    db.lookup_symbols(repo, query, kind, clamp_limit(limit))
}

/// Look up imports by file path or module name.
pub fn lookup_imports(
    db: &Database,
    repo: &str,
    query: &str,
    limit: Option<usize>,
) -> Result<Vec<CodeImport>, MemoryError> {
    db.lookup_imports(repo, query, clamp_limit(limit))
}

/// Look up edges (neighbors) by symbol id or file path.
pub fn lookup_neighbors(
    db: &Database,
    repo: &str,
    query: &str,
    limit: Option<usize>,
) -> Result<Vec<CodeSymbolEdge>, MemoryError> {
    db.lookup_neighbors(repo, query, clamp_limit(limit))
}

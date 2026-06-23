//! DB storage and query layer for the local symbol/import graph index.
//!
//! Tables: `code_index_files`, `code_symbols`, `code_imports`,
//! `code_symbol_edges` (migration 012).
//!
//! Design mirrors `db/code_maps.rs`:
//! - Immutable structs with `#[derive(Serialize, Deserialize)]`
//! - All writes inside a `Transaction`
//! - Storage-layer `validate_*` invariants called before every insert
//! - Per-file replacement: delete old rows for (repo, path) then insert new

use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use super::schema::Database;
use crate::error::MemoryError;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum byte length for `signature` and `raw` fields before truncation.
/// Bounded declaration metadata only — no full source bodies are persisted.
pub const MAX_SNIPPET_LEN: usize = 512;

// ── Immutable structs ──────────────────────────────────────────────────────────

/// A row from `code_index_files` — tracks per-file index state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexFile {
    pub repo: String,
    pub path: String,
    pub head_sha: String,
    pub content_hash: String,
    pub language: String,
    pub indexed_at: String,
}

/// A row from `code_symbols`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeSymbol {
    pub id: String,
    pub repo: String,
    pub path: String,
    pub language: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub visibility: Option<String>,
    pub signature: Option<String>,
    pub start_line: i64,
    pub start_col: i64,
    pub end_line: Option<i64>,
    pub parent_id: Option<String>,
    pub confidence: f64,
    pub indexed_at: String,
}

/// A row from `code_imports`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeImport {
    pub id: String,
    pub repo: String,
    pub path: String,
    pub language: String,
    pub module: String,
    pub symbol: Option<String>,
    pub alias: Option<String>,
    pub raw: Option<String>,
    pub line: i64,
    pub confidence: f64,
    pub indexed_at: String,
}

/// A row from `code_symbol_edges`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeSymbolEdge {
    pub id: String,
    pub repo: String,
    pub from_kind: String,
    pub from_id: String,
    pub to_kind: String,
    pub to_ref: String,
    pub edge_kind: String,
    pub path: String,
    pub line: Option<i64>,
    pub confidence: f64,
    pub indexed_at: String,
}

/// Result returned from `index_repo` counts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexCounts {
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub files_purged: usize,
    pub symbols_inserted: usize,
    pub imports_inserted: usize,
    pub edges_inserted: usize,
}

// ── Storage-layer validators ───────────────────────────────────────────────────

/// Validate repo and path: both must be non-empty, path must not contain
/// backslashes or NUL bytes (invariant: forward-slash repo-relative).
pub(crate) fn validate_repo_path(repo: &str, path: &str) -> Result<(), MemoryError> {
    if repo.is_empty() {
        return Err(MemoryError::Validation("repo must not be empty".into()));
    }
    if path.is_empty() {
        return Err(MemoryError::Validation("path must not be empty".into()));
    }
    if path.contains('\0') || path.contains('\\') {
        return Err(MemoryError::Validation(format!(
            "path must be a normalized forward-slash repo-relative path: {path}"
        )));
    }
    Ok(())
}

/// Truncate a string to at most `MAX_SNIPPET_LEN` bytes at a UTF-8 boundary.
fn truncate_snippet(s: &str) -> String {
    if s.len() <= MAX_SNIPPET_LEN {
        s.to_string()
    } else {
        // Find last valid UTF-8 char boundary at or before MAX_SNIPPET_LEN.
        let mut end = MAX_SNIPPET_LEN;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }
}

// ── Row mappers ───────────────────────────────────────────────────────────────

fn map_index_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexFile> {
    Ok(IndexFile {
        repo: row.get(0)?,
        path: row.get(1)?,
        head_sha: row.get(2)?,
        content_hash: row.get(3)?,
        language: row.get(4)?,
        indexed_at: row.get(5)?,
    })
}

fn map_code_symbol(row: &rusqlite::Row<'_>) -> rusqlite::Result<CodeSymbol> {
    Ok(CodeSymbol {
        id: row.get(0)?,
        repo: row.get(1)?,
        path: row.get(2)?,
        language: row.get(3)?,
        name: row.get(4)?,
        qualified_name: row.get(5)?,
        kind: row.get(6)?,
        visibility: row.get(7)?,
        signature: row.get(8)?,
        start_line: row.get(9)?,
        start_col: row.get(10)?,
        end_line: row.get(11)?,
        parent_id: row.get(12)?,
        confidence: row.get(13)?,
        indexed_at: row.get(14)?,
    })
}

fn map_code_import(row: &rusqlite::Row<'_>) -> rusqlite::Result<CodeImport> {
    Ok(CodeImport {
        id: row.get(0)?,
        repo: row.get(1)?,
        path: row.get(2)?,
        language: row.get(3)?,
        module: row.get(4)?,
        symbol: row.get(5)?,
        alias: row.get(6)?,
        raw: row.get(7)?,
        line: row.get(8)?,
        confidence: row.get(9)?,
        indexed_at: row.get(10)?,
    })
}

fn map_code_symbol_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<CodeSymbolEdge> {
    Ok(CodeSymbolEdge {
        id: row.get(0)?,
        repo: row.get(1)?,
        from_kind: row.get(2)?,
        from_id: row.get(3)?,
        to_kind: row.get(4)?,
        to_ref: row.get(5)?,
        edge_kind: row.get(6)?,
        path: row.get(7)?,
        line: row.get(8)?,
        confidence: row.get(9)?,
        indexed_at: row.get(10)?,
    })
}

// ── Database impl ─────────────────────────────────────────────────────────────

impl Database {
    // ── File tracking ──────────────────────────────────────────────────────

    /// Upsert a `code_index_files` row for the given (repo, path).
    pub fn upsert_index_file_tx(
        tx: &Transaction<'_>,
        repo: &str,
        path: &str,
        head_sha: &str,
        content_hash: &str,
        language: &str,
        indexed_at: &str,
    ) -> Result<(), MemoryError> {
        validate_repo_path(repo, path)?;
        tx.execute(
            "INSERT INTO code_index_files
                 (repo, path, head_sha, content_hash, language, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(repo, path) DO UPDATE SET
                 head_sha     = excluded.head_sha,
                 content_hash = excluded.content_hash,
                 language     = excluded.language,
                 indexed_at   = excluded.indexed_at",
            params![repo, path, head_sha, content_hash, language, indexed_at],
        )?;
        Ok(())
    }

    /// Get the `code_index_files` row for `(repo, path)`, if any.
    pub fn get_index_file(&self, repo: &str, path: &str) -> Result<Option<IndexFile>, MemoryError> {
        self.conn
            .query_row(
                "SELECT repo, path, head_sha, content_hash, language, indexed_at
                 FROM code_index_files
                 WHERE repo = ?1 AND path = ?2",
                params![repo, path],
                map_index_file,
            )
            .optional()
            .map_err(MemoryError::from)
    }

    /// List all `code_index_files` rows for a repo.
    pub fn list_index_files(&self, repo: &str) -> Result<Vec<IndexFile>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT repo, path, head_sha, content_hash, language, indexed_at
             FROM code_index_files
             WHERE repo = ?1
             ORDER BY path",
        )?;
        let rows = stmt.query_map(params![repo], map_index_file)?;
        rows.map(|r| r.map_err(MemoryError::from))
            .collect::<Result<Vec<_>, _>>()
    }

    // ── Per-file replacement ───────────────────────────────────────────────

    /// Delete all symbol/import/edge rows for `(repo, path)`. Used before
    /// re-inserting updated rows (per-file replacement pattern) and for purge
    /// of deleted files.
    pub fn delete_file_rows_tx(
        tx: &Transaction<'_>,
        repo: &str,
        path: &str,
    ) -> Result<(), MemoryError> {
        tx.execute(
            "DELETE FROM code_symbols WHERE repo = ?1 AND path = ?2",
            params![repo, path],
        )?;
        tx.execute(
            "DELETE FROM code_imports WHERE repo = ?1 AND path = ?2",
            params![repo, path],
        )?;
        tx.execute(
            "DELETE FROM code_symbol_edges WHERE repo = ?1 AND path = ?2",
            params![repo, path],
        )?;
        Ok(())
    }

    /// Purge ALL rows for `(repo, path)` from all four tables, including the
    /// file-tracking row. Used when a file is deleted from the worktree.
    pub fn purge_file_tx(tx: &Transaction<'_>, repo: &str, path: &str) -> Result<(), MemoryError> {
        Self::delete_file_rows_tx(tx, repo, path)?;
        tx.execute(
            "DELETE FROM code_index_files WHERE repo = ?1 AND path = ?2",
            params![repo, path],
        )?;
        Ok(())
    }

    // ── Symbol insert ──────────────────────────────────────────────────────

    /// Insert a single `code_symbols` row. `signature` is truncated to
    /// `MAX_SNIPPET_LEN` before storage. Returns the number of rows actually
    /// inserted (0 if `INSERT OR IGNORE` skipped an id collision), so callers
    /// count only persisted rows rather than attempted inserts.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_symbol_tx(
        tx: &Transaction<'_>,
        id: &str,
        repo: &str,
        path: &str,
        language: &str,
        name: &str,
        qualified_name: &str,
        kind: &str,
        visibility: Option<&str>,
        signature: Option<&str>,
        start_line: i64,
        start_col: i64,
        end_line: Option<i64>,
        parent_id: Option<&str>,
        confidence: f64,
        indexed_at: &str,
    ) -> Result<usize, MemoryError> {
        validate_repo_path(repo, path)?;
        let signature_stored = signature.map(truncate_snippet);
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO code_symbols
                 (id, repo, path, language, name, qualified_name, kind, visibility,
                  signature, start_line, start_col, end_line, parent_id, confidence, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                id,
                repo,
                path,
                language,
                name,
                qualified_name,
                kind,
                visibility,
                signature_stored,
                start_line,
                start_col,
                end_line,
                parent_id,
                confidence,
                indexed_at,
            ],
        )?;
        Ok(inserted)
    }

    // ── Import insert ──────────────────────────────────────────────────────

    /// Insert a single `code_imports` row. `raw` is truncated to
    /// `MAX_SNIPPET_LEN` before storage. Returns the number of rows actually
    /// inserted (0 if `INSERT OR IGNORE` skipped an id collision).
    #[allow(clippy::too_many_arguments)]
    pub fn insert_import_tx(
        tx: &Transaction<'_>,
        id: &str,
        repo: &str,
        path: &str,
        language: &str,
        module: &str,
        symbol: Option<&str>,
        alias: Option<&str>,
        raw: Option<&str>,
        line: i64,
        confidence: f64,
        indexed_at: &str,
    ) -> Result<usize, MemoryError> {
        validate_repo_path(repo, path)?;
        let raw_stored = raw.map(truncate_snippet);
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO code_imports
                 (id, repo, path, language, module, symbol, alias, raw, line, confidence, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id, repo, path, language, module, symbol, alias, raw_stored, line, confidence,
                indexed_at
            ],
        )?;
        Ok(inserted)
    }

    // ── Edge insert ────────────────────────────────────────────────────────

    /// Insert a single `code_symbol_edges` row. Returns the number of rows
    /// actually inserted (0 if `INSERT OR IGNORE` skipped an id collision).
    #[allow(clippy::too_many_arguments)]
    pub fn insert_edge_tx(
        tx: &Transaction<'_>,
        id: &str,
        repo: &str,
        from_kind: &str,
        from_id: &str,
        to_kind: &str,
        to_ref: &str,
        edge_kind: &str,
        path: &str,
        line: Option<i64>,
        confidence: f64,
        indexed_at: &str,
    ) -> Result<usize, MemoryError> {
        validate_repo_path(repo, path)?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO code_symbol_edges
                 (id, repo, from_kind, from_id, to_kind, to_ref,
                  edge_kind, path, line, confidence, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id, repo, from_kind, from_id, to_kind, to_ref, edge_kind, path, line, confidence,
                indexed_at
            ],
        )?;
        Ok(inserted)
    }

    // ── Query helpers ──────────────────────────────────────────────────────

    /// Look up symbols by name (or qualified_name prefix) within a repo.
    /// `kind` optionally filters by symbol kind (e.g. "fn", "struct").
    /// Results bounded by `limit`.
    pub fn lookup_symbols(
        &self,
        repo: &str,
        query: &str,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodeSymbol>, MemoryError> {
        let limit = limit as i64;
        let like_pat = format!("{query}%");
        let rows: Vec<CodeSymbol> = if let Some(k) = kind {
            let mut stmt = self.conn.prepare(
                "SELECT id, repo, path, language, name, qualified_name, kind, visibility,
                        signature, start_line, start_col, end_line, parent_id, confidence, indexed_at
                 FROM code_symbols
                 WHERE repo = ?1
                   AND (name = ?2 OR qualified_name = ?2
                        OR name LIKE ?3 OR qualified_name LIKE ?3)
                   AND kind = ?4
                 ORDER BY qualified_name
                 LIMIT ?5",
            )?;
            let collected: Vec<CodeSymbol> = stmt
                .query_map(params![repo, query, like_pat, k, limit], map_code_symbol)?
                .map(|r| r.map_err(MemoryError::from))
                .collect::<Result<Vec<_>, _>>()?;
            collected
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT id, repo, path, language, name, qualified_name, kind, visibility,
                        signature, start_line, start_col, end_line, parent_id, confidence, indexed_at
                 FROM code_symbols
                 WHERE repo = ?1
                   AND (name = ?2 OR qualified_name = ?2
                        OR name LIKE ?3 OR qualified_name LIKE ?3)
                 ORDER BY qualified_name
                 LIMIT ?4",
            )?;
            let collected: Vec<CodeSymbol> = stmt
                .query_map(params![repo, query, like_pat, limit], map_code_symbol)?
                .map(|r| r.map_err(MemoryError::from))
                .collect::<Result<Vec<_>, _>>()?;
            collected
        };
        Ok(rows)
    }

    /// List symbols for a specific file path within a repo.
    pub fn symbols_for_path(
        &self,
        repo: &str,
        path: &str,
        limit: usize,
    ) -> Result<Vec<CodeSymbol>, MemoryError> {
        let limit = limit as i64;
        let mut stmt = self.conn.prepare(
            "SELECT id, repo, path, language, name, qualified_name, kind, visibility,
                    signature, start_line, start_col, end_line, parent_id, confidence, indexed_at
             FROM code_symbols
             WHERE repo = ?1 AND path = ?2
             ORDER BY start_line
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![repo, path, limit], map_code_symbol)?
            .map(|r| r.map_err(MemoryError::from))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Look up imports by module or file path within a repo.
    pub fn lookup_imports(
        &self,
        repo: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<CodeImport>, MemoryError> {
        let limit = limit as i64;
        let like_pat = format!("{query}%");
        let mut stmt = self.conn.prepare(
            "SELECT id, repo, path, language, module, symbol, alias, raw, line, confidence, indexed_at
             FROM code_imports
             WHERE repo = ?1
               AND (path = ?2 OR path LIKE ?3
                    OR module = ?2 OR module LIKE ?3
                    OR symbol = ?2)
             ORDER BY path, line
             LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(params![repo, query, like_pat, limit], map_code_import)?
            .map(|r| r.map_err(MemoryError::from))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Look up incoming and outgoing edges for a symbol or file within a repo.
    pub fn lookup_neighbors(
        &self,
        repo: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<CodeSymbolEdge>, MemoryError> {
        let limit = limit as i64;
        let like_pat = format!("{query}%");
        let mut stmt = self.conn.prepare(
            "SELECT id, repo, from_kind, from_id, to_kind, to_ref,
                    edge_kind, path, line, confidence, indexed_at
             FROM code_symbol_edges
             WHERE repo = ?1
               AND (from_id = ?2 OR to_ref = ?2
                    OR from_id LIKE ?3 OR to_ref LIKE ?3
                    OR from_id IN (
                        SELECT id FROM code_symbols
                        WHERE repo = ?1
                          AND (name = ?2 OR qualified_name = ?2
                               OR name LIKE ?3 OR qualified_name LIKE ?3)
                    )
                    OR to_ref IN (
                        SELECT id FROM code_symbols
                        WHERE repo = ?1
                          AND (name = ?2 OR qualified_name = ?2
                               OR name LIKE ?3 OR qualified_name LIKE ?3)
                    ))
             ORDER BY edge_kind, path, line
             LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(params![repo, query, like_pat, limit], map_code_symbol_edge)?
            .map(|r| r.map_err(MemoryError::from))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::Database;

    fn now() -> String {
        "2026-06-22T00:00:00Z".to_string()
    }

    // ── Task 2 acceptance: per-file replacement removes old rows ──────────

    #[test]
    fn per_file_replacement_removes_old_rows() {
        let db = Database::open_in_memory().unwrap();

        // Insert a symbol and import for path a.rs
        db.with_transaction(|tx| {
            Database::upsert_index_file_tx(
                tx,
                "testrepo",
                "a.rs",
                "sha1",
                "hash1",
                "rust",
                &now(),
            )?;
            Database::insert_symbol_tx(
                tx,
                "sym-old",
                "testrepo",
                "a.rs",
                "rust",
                "old_fn",
                "old_fn",
                "fn",
                Some("pub"),
                Some("fn old_fn()"),
                1,
                0,
                Some(5),
                None,
                1.0,
                &now(),
            )?;
            Database::insert_import_tx(
                tx,
                "imp-old",
                "testrepo",
                "a.rs",
                "rust",
                "std::io",
                None,
                None,
                Some("use std::io;"),
                1,
                1.0,
                &now(),
            )?;
            Ok(())
        })
        .unwrap();

        // Verify they exist
        let syms = db.symbols_for_path("testrepo", "a.rs", 100).unwrap();
        assert_eq!(syms.len(), 1, "should have 1 symbol before replacement");
        let imps = db.lookup_imports("testrepo", "a.rs", 100).unwrap();
        assert_eq!(imps.len(), 1, "should have 1 import before replacement");

        // Per-file replacement: delete old rows, insert new
        db.with_transaction(|tx| {
            Database::delete_file_rows_tx(tx, "testrepo", "a.rs")?;
            Database::insert_symbol_tx(
                tx,
                "sym-new",
                "testrepo",
                "a.rs",
                "rust",
                "new_fn",
                "new_fn",
                "fn",
                None,
                Some("fn new_fn()"),
                10,
                0,
                Some(15),
                None,
                1.0,
                &now(),
            )?;
            Ok(())
        })
        .unwrap();

        let syms_after = db.symbols_for_path("testrepo", "a.rs", 100).unwrap();
        assert_eq!(
            syms_after.len(),
            1,
            "should have 1 symbol after replacement"
        );
        assert_eq!(syms_after[0].id, "sym-new", "new symbol should be present");
        assert!(
            syms_after.iter().all(|s| s.id != "sym-old"),
            "old symbol must be gone"
        );

        let imps_after = db.lookup_imports("testrepo", "a.rs", 100).unwrap();
        assert_eq!(imps_after.len(), 0, "old import must be purged");
    }

    // ── Task 2 acceptance: purge-by-path clears all three tables ──────────

    #[test]
    fn purge_file_clears_all_tables() {
        let db = Database::open_in_memory().unwrap();

        db.with_transaction(|tx| {
            Database::upsert_index_file_tx(tx, "repo", "b.py", "sha1", "hash1", "python", &now())?;
            Database::insert_symbol_tx(
                tx,
                "sym-b",
                "repo",
                "b.py",
                "python",
                "MyClass",
                "MyClass",
                "class",
                None,
                Some("class MyClass:"),
                1,
                0,
                None,
                None,
                1.0,
                &now(),
            )?;
            Database::insert_import_tx(
                tx,
                "imp-b",
                "repo",
                "b.py",
                "python",
                "os",
                None,
                None,
                Some("import os"),
                1,
                1.0,
                &now(),
            )?;
            Database::insert_edge_tx(
                tx,
                "edge-b",
                "repo",
                "file",
                "b.py",
                "module",
                "os",
                "import",
                "b.py",
                Some(1),
                1.0,
                &now(),
            )?;
            Ok(())
        })
        .unwrap();

        // Verify file row exists
        assert!(db.get_index_file("repo", "b.py").unwrap().is_some());

        // Purge
        db.with_transaction(|tx| Database::purge_file_tx(tx, "repo", "b.py"))
            .unwrap();

        // All tables cleared
        assert!(
            db.get_index_file("repo", "b.py").unwrap().is_none(),
            "file tracking row must be purged"
        );
        let syms = db.symbols_for_path("repo", "b.py", 100).unwrap();
        assert_eq!(syms.len(), 0, "symbols must be purged");
        let imps = db.lookup_imports("repo", "b.py", 100).unwrap();
        assert_eq!(imps.len(), 0, "imports must be purged");
        let edges = db.lookup_neighbors("repo", "b.py", 100).unwrap();
        assert_eq!(edges.len(), 0, "edges must be purged");
    }

    // ── Task 2 acceptance: lookup helpers return inserted rows ─────────────

    #[test]
    fn lookup_helpers_return_inserted_rows() {
        let db = Database::open_in_memory().unwrap();

        db.with_transaction(|tx| {
            Database::upsert_index_file_tx(tx, "repo", "lib.rs", "sha1", "hash1", "rust", &now())?;
            Database::insert_symbol_tx(
                tx,
                "sym-1",
                "repo",
                "lib.rs",
                "rust",
                "parse",
                "crate::parse",
                "fn",
                Some("pub"),
                Some("pub fn parse(input: &str) -> Result<Ast, Error>"),
                42,
                4,
                Some(80),
                None,
                1.0,
                &now(),
            )?;
            Database::insert_import_tx(
                tx,
                "imp-1",
                "repo",
                "lib.rs",
                "rust",
                "std::collections",
                Some("HashMap"),
                None,
                Some("use std::collections::HashMap;"),
                1,
                1.0,
                &now(),
            )?;
            Database::insert_edge_tx(
                tx,
                "edge-1",
                "repo",
                "file",
                "lib.rs",
                "module",
                "std::collections",
                "import",
                "lib.rs",
                Some(1),
                1.0,
                &now(),
            )?;
            Ok(())
        })
        .unwrap();

        // lookup_symbols by name
        let syms = db.lookup_symbols("repo", "parse", None, 10).unwrap();
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].id, "sym-1");
        assert_eq!(syms[0].qualified_name, "crate::parse");

        // lookup_symbols by qualified_name
        let syms2 = db.lookup_symbols("repo", "crate::parse", None, 10).unwrap();
        assert_eq!(syms2.len(), 1);

        // lookup_symbols with kind filter
        let syms3 = db.lookup_symbols("repo", "parse", Some("fn"), 10).unwrap();
        assert_eq!(syms3.len(), 1);
        let syms4 = db
            .lookup_symbols("repo", "parse", Some("struct"), 10)
            .unwrap();
        assert_eq!(syms4.len(), 0);

        // lookup_imports
        let imps = db.lookup_imports("repo", "lib.rs", 10).unwrap();
        assert_eq!(imps.len(), 1);
        assert_eq!(imps[0].module, "std::collections");

        // lookup_imports by module
        let imps2 = db.lookup_imports("repo", "std::collections", 10).unwrap();
        assert_eq!(imps2.len(), 1);

        // lookup_neighbors
        let edges = db.lookup_neighbors("repo", "lib.rs", 10).unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].to_ref, "std::collections");
    }

    // ── Storage-layer validation: reject empty repo/path ──────────────────

    #[test]
    fn validate_repo_path_rejects_empty() {
        assert!(validate_repo_path("", "a.rs").is_err());
        assert!(validate_repo_path("/repo", "").is_err());
        assert!(validate_repo_path("/repo", "src\\lib.rs").is_err());
        assert!(validate_repo_path("/repo", "a.rs").is_ok());
    }

    // ── Truncation: signature is capped at MAX_SNIPPET_LEN ────────────────

    #[test]
    fn signature_is_truncated_before_storage() {
        let db = Database::open_in_memory().unwrap();
        let long_sig = "a".repeat(MAX_SNIPPET_LEN + 100);

        db.with_transaction(|tx| {
            Database::upsert_index_file_tx(tx, "repo", "t.rs", "sha1", "h1", "rust", &now())?;
            Database::insert_symbol_tx(
                tx,
                "sym-trunc",
                "repo",
                "t.rs",
                "rust",
                "big_fn",
                "big_fn",
                "fn",
                None,
                Some(&long_sig),
                1,
                0,
                None,
                None,
                1.0,
                &now(),
            )?;
            Ok(())
        })
        .unwrap();

        let syms = db.symbols_for_path("repo", "t.rs", 10).unwrap();
        assert_eq!(syms.len(), 1);
        let stored_sig = syms[0].signature.as_deref().unwrap_or("");
        assert!(
            stored_sig.len() <= MAX_SNIPPET_LEN,
            "signature must be truncated to MAX_SNIPPET_LEN, got len {}",
            stored_sig.len()
        );
    }
}

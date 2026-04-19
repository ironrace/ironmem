use rusqlite::{params, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Sanitize a query string for FTS5 MATCH syntax.
///
/// Previous behaviour wrapped every token in double-quotes, making the query a
/// strict AND-of-phrases. This caused empty BM25 hits on verbose questions
/// (any absent token → zero results) and collapsed the hybrid pipeline to
/// vector-only retrieval.
///
/// New behaviour: strip FTS5 operator characters and emit bare stemmed tokens
/// separated by spaces. FTS5 with `tokenize='porter ascii'` will apply the
/// Porter stemmer so morphological variants still match, and the implicit AND
/// applies at the token level (not phrase level) so partial overlap now returns
/// results with appropriate BM25 scores rather than nothing.
fn fts5_sanitize(query: &str) -> String {
    query
        .split_whitespace()
        .filter_map(|token| {
            let clean: String = token
                .chars()
                .filter(|c| c.is_alphanumeric() || matches!(c, '\'' | '-'))
                .collect();
            if clean.is_empty() {
                None
            } else {
                Some(clean)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

use super::schema::Database;
use crate::error::MemoryError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drawer {
    pub id: String,
    pub content: String,
    pub wing: String,
    pub room: String,
    pub source_file: String,
    pub added_by: String,
    pub filed_at: String,
    pub date: String,
}

#[derive(Debug, Clone)]
pub struct ScoredDrawer {
    pub drawer: Drawer,
    pub score: f32,
}

#[derive(Debug, Default)]
pub struct SearchFilters {
    pub wing: Option<String>,
    pub room: Option<String>,
    pub limit: usize,
}

/// Generate a deterministic drawer ID from content + wing + room.
pub fn generate_id(content: &str, wing: &str, room: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hasher.update(wing.as_bytes());
    hasher.update(room.as_bytes());
    format!("{:x}", hasher.finalize())[..32].to_string()
}

impl Database {
    /// Insert a drawer with its embedding.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_drawer(
        &self,
        id: &str,
        content: &str,
        embedding: &[f32],
        wing: &str,
        room: &str,
        source_file: &str,
        added_by: &str,
    ) -> Result<(), MemoryError> {
        Self::insert_drawer_conn(
            &self.conn,
            id,
            content,
            embedding,
            wing,
            room,
            source_file,
            added_by,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert_drawer_tx(
        tx: &Transaction<'_>,
        id: &str,
        content: &str,
        embedding: &[f32],
        wing: &str,
        room: &str,
        source_file: &str,
        added_by: &str,
    ) -> Result<(), MemoryError> {
        Self::insert_drawer_conn(
            tx,
            id,
            content,
            embedding,
            wing,
            room,
            source_file,
            added_by,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_drawer_conn(
        conn: &rusqlite::Connection,
        id: &str,
        content: &str,
        embedding: &[f32],
        wing: &str,
        room: &str,
        source_file: &str,
        added_by: &str,
    ) -> Result<(), MemoryError> {
        let blob: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();

        conn.execute(
            "INSERT INTO drawers (id, content, embedding, wing, room, source_file, added_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                content = excluded.content,
                embedding = excluded.embedding,
                wing = excluded.wing,
                room = excluded.room,
                source_file = excluded.source_file,
                added_by = excluded.added_by",
            params![id, content, blob, wing, room, source_file, added_by],
        )?;

        // Keep FTS5 index in sync (delete-then-insert for upsert semantics).
        // Silently skip if the FTS table doesn't exist yet (pre-migration DBs).
        let fts_ok = conn
            .execute("DELETE FROM drawers_fts WHERE drawer_id = ?1", params![id])
            .and_then(|_| {
                conn.execute(
                    "INSERT INTO drawers_fts(content, drawer_id) VALUES (?1, ?2)",
                    params![content, id],
                )
            });
        if let Err(e) = fts_ok {
            // FTS table may not exist on first startup before migration runs.
            // Log at debug level and continue — HNSW-only search remains functional.
            tracing::debug!("FTS5 sync skipped (table may not exist yet): {e}");
        }

        Ok(())
    }

    /// Delete a drawer by ID.
    pub fn delete_drawer(&self, id: &str) -> Result<bool, MemoryError> {
        let count = Self::delete_drawer_conn(&self.conn, id)?;
        Ok(count > 0)
    }

    /// Delete all drawers associated with a source file.
    pub fn delete_drawers_by_source_file(&self, source_file: &str) -> Result<usize, MemoryError> {
        Self::delete_drawers_by_source_file_conn(&self.conn, source_file)
    }

    pub(crate) fn delete_drawer_tx(tx: &Transaction<'_>, id: &str) -> Result<bool, MemoryError> {
        let count = Self::delete_drawer_conn(tx, id)?;
        Ok(count > 0)
    }

    pub(crate) fn delete_drawers_by_source_file_tx(
        tx: &Transaction<'_>,
        source_file: &str,
    ) -> Result<usize, MemoryError> {
        Self::delete_drawers_by_source_file_conn(tx, source_file)
    }

    /// Get a drawer by ID.
    pub fn get_drawer(&self, id: &str) -> Result<Option<Drawer>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, wing, room, source_file, added_by, filed_at, date
             FROM drawers WHERE id = ?1",
        )?;

        let mut rows = stmt.query_map(params![id], Self::row_to_drawer)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Get multiple drawers by IDs. Chunks queries to stay under SQLite's
    /// SQLITE_MAX_VARIABLE_NUMBER limit (default 999).
    pub fn get_drawers_by_ids(
        &self,
        ids: &[&str],
    ) -> Result<std::collections::HashMap<String, Drawer>, MemoryError> {
        self.get_drawers_by_ids_filtered(ids, None, None)
    }

    /// Get multiple drawers by IDs with optional metadata filters applied in SQL.
    pub fn get_drawers_by_ids_filtered(
        &self,
        ids: &[&str],
        wing: Option<&str>,
        room: Option<&str>,
    ) -> Result<std::collections::HashMap<String, Drawer>, MemoryError> {
        const CHUNK_SIZE: usize = 900; // Stay well under SQLite's 999 limit

        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let mut result = std::collections::HashMap::new();

        for chunk in ids.chunks(CHUNK_SIZE) {
            let placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut sql = format!(
                "SELECT id, content, wing, room, source_file, added_by, filed_at, date
                 FROM drawers WHERE id IN ({})",
                placeholders
            );
            let mut owned_params: Vec<String> = Vec::new();
            if let Some(w) = wing {
                sql.push_str(" AND wing = ?");
                owned_params.push(w.to_string());
            }
            if let Some(r) = room {
                sql.push_str(" AND room = ?");
                owned_params.push(r.to_string());
            }
            let mut stmt = self.conn.prepare(&sql)?;
            let mut params: Vec<&dyn rusqlite::types::ToSql> = chunk
                .iter()
                .map(|id| id as &dyn rusqlite::types::ToSql)
                .collect();
            for value in &owned_params {
                params.push(value as &dyn rusqlite::types::ToSql);
            }
            let rows = stmt.query_map(params.as_slice(), Self::row_to_drawer)?;
            for row in rows {
                let drawer = row?;
                result.insert(drawer.id.clone(), drawer);
            }
        }

        Ok(result)
    }

    /// Get drawers matching optional wing/room filters.
    pub fn get_drawers(
        &self,
        wing: Option<&str>,
        room: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Drawer>, MemoryError> {
        let limit_i64 = limit as i64;

        let mut result = Vec::new();
        match (wing, room) {
            (Some(w), Some(r)) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, content, wing, room, source_file, added_by, filed_at, date
                     FROM drawers WHERE wing = ?1 AND room = ?2 ORDER BY filed_at DESC LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![w, r, limit_i64], Self::row_to_drawer)?;
                for row in rows {
                    result.push(row?);
                }
            }
            (Some(w), None) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, content, wing, room, source_file, added_by, filed_at, date
                     FROM drawers WHERE wing = ?1 ORDER BY filed_at DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![w, limit_i64], Self::row_to_drawer)?;
                for row in rows {
                    result.push(row?);
                }
            }
            (None, Some(r)) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, content, wing, room, source_file, added_by, filed_at, date
                     FROM drawers WHERE room = ?1 ORDER BY filed_at DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![r, limit_i64], Self::row_to_drawer)?;
                for row in rows {
                    result.push(row?);
                }
            }
            (None, None) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, content, wing, room, source_file, added_by, filed_at, date
                     FROM drawers ORDER BY filed_at DESC LIMIT ?1",
                )?;
                let rows = stmt.query_map(params![limit_i64], Self::row_to_drawer)?;
                for row in rows {
                    result.push(row?);
                }
            }
        }
        Ok(result)
    }

    /// Count drawers, optionally filtered by wing.
    pub fn count_drawers(&self, wing: Option<&str>) -> Result<usize, MemoryError> {
        let count: i64 = match wing {
            Some(w) => self.conn.query_row(
                "SELECT COUNT(*) FROM drawers WHERE wing = ?1",
                params![w],
                |row| row.get(0),
            )?,
            None => self
                .conn
                .query_row("SELECT COUNT(*) FROM drawers", [], |row| row.get(0))?,
        };
        Ok(count as usize)
    }

    /// BM25 full-text search via SQLite FTS5. Returns (drawer_id, score) pairs
    /// ordered by relevance descending. Score is positive (negated bm25() output).
    ///
    /// Returns an empty vec if the FTS table doesn't exist yet or the query is
    /// syntactically invalid — callers fall back to vector-only search gracefully.
    pub fn bm25_search(
        &self,
        query: &str,
        limit: usize,
        wing: Option<&str>,
        room: Option<&str>,
    ) -> Result<Vec<(String, f32)>, MemoryError> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let fts_query = fts5_sanitize(query);
        let limit_i64 = limit as i64;

        let sql = match (wing, room) {
            (Some(_), Some(_)) => {
                "SELECT f.drawer_id, -bm25(f) AS score
                 FROM drawers_fts f
                 JOIN drawers d ON d.id = f.drawer_id
                 WHERE f MATCH ?1 AND d.wing = ?3 AND d.room = ?4
                 ORDER BY score DESC LIMIT ?2"
            }
            (Some(_), None) => {
                "SELECT f.drawer_id, -bm25(f) AS score
                 FROM drawers_fts f
                 JOIN drawers d ON d.id = f.drawer_id
                 WHERE f MATCH ?1 AND d.wing = ?3
                 ORDER BY score DESC LIMIT ?2"
            }
            (None, Some(_)) => {
                "SELECT f.drawer_id, -bm25(f) AS score
                 FROM drawers_fts f
                 JOIN drawers d ON d.id = f.drawer_id
                 WHERE f MATCH ?1 AND d.room = ?4
                 ORDER BY score DESC LIMIT ?2"
            }
            (None, None) => {
                "SELECT drawer_id, -bm25(drawers_fts) AS score
                 FROM drawers_fts
                 WHERE drawers_fts MATCH ?1
                 ORDER BY score DESC LIMIT ?2"
            }
        };

        let mut stmt = match self.conn.prepare(sql) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("BM25 search skipped (FTS table may not exist): {e}");
                return Ok(vec![]);
            }
        };

        let query_fn = |row: &rusqlite::Row<'_>| -> rusqlite::Result<(String, f32)> {
            let id: String = row.get(0)?;
            let score: f64 = row.get(1)?;
            Ok((id, score as f32))
        };

        let rows = match (wing, room) {
            (Some(w), Some(r)) => {
                stmt.query_map(rusqlite::params![fts_query, limit_i64, w, r], query_fn)
            }
            (Some(w), None) => stmt.query_map(rusqlite::params![fts_query, limit_i64, w], query_fn),
            (None, Some(r)) => stmt.query_map(rusqlite::params![fts_query, limit_i64, r], query_fn),
            (None, None) => stmt.query_map(rusqlite::params![fts_query, limit_i64], query_fn),
        };

        match rows {
            Err(e) => {
                // FTS5 query syntax error or missing table — degrade gracefully.
                tracing::debug!("BM25 query failed (will use vector-only): {e}");
                Ok(vec![])
            }
            Ok(rows) => {
                let mut result = Vec::new();
                for row in rows {
                    match row {
                        Ok(pair) => result.push(pair),
                        Err(e) => {
                            tracing::debug!("BM25 row error: {e}");
                            break;
                        }
                    }
                }
                Ok(result)
            }
        }
    }

    /// Get wing -> count mapping.
    pub fn wing_counts(&self) -> Result<Vec<(String, usize)>, MemoryError> {
        let mut stmt = self
            .conn
            .prepare("SELECT wing, COUNT(*) FROM drawers GROUP BY wing ORDER BY wing")?;

        let rows = stmt.query_map([], |row| {
            let wing: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((wing, count as usize))
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Get room -> count mapping within a wing.
    pub fn room_counts(&self, wing: Option<&str>) -> Result<Vec<(String, usize)>, MemoryError> {
        let (sql, param): (&str, Vec<String>) = match wing {
            Some(w) => (
                "SELECT room, COUNT(*) FROM drawers WHERE wing = ?1 GROUP BY room ORDER BY room",
                vec![w.to_string()],
            ),
            None => (
                "SELECT room, COUNT(*) FROM drawers GROUP BY room ORDER BY room",
                vec![],
            ),
        };

        let mut stmt = self.conn.prepare(sql)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = param
            .iter()
            .map(|p| p as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            let room: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((room, count as usize))
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Return all (wing, room) pairs present in the drawers table.
    ///
    /// Used by the graph module to build the room adjacency graph without
    /// requiring direct access to `conn`.
    pub fn wing_room_pairs(&self) -> Result<Vec<(String, String)>, MemoryError> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT wing, room FROM drawers")?;
        let rows = stmt.query_map([], |row| {
            let wing: String = row.get(0)?;
            let room: String = row.get(1)?;
            Ok((wing, room))
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Get full taxonomy: wing -> room -> count.
    pub fn taxonomy(
        &self,
    ) -> Result<std::collections::HashMap<String, Vec<(String, usize)>>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT wing, room, COUNT(*) FROM drawers GROUP BY wing, room ORDER BY wing, room",
        )?;

        let rows = stmt.query_map([], |row| {
            let wing: String = row.get(0)?;
            let room: String = row.get(1)?;
            let count: i64 = row.get(2)?;
            Ok((wing, room, count as usize))
        })?;

        let mut result: std::collections::HashMap<String, Vec<(String, usize)>> =
            std::collections::HashMap::new();
        for row in rows {
            let (wing, room, count) = row?;
            result.entry(wing).or_default().push((room, count));
        }
        Ok(result)
    }

    fn row_to_drawer(row: &rusqlite::Row<'_>) -> rusqlite::Result<Drawer> {
        Ok(Drawer {
            id: row.get(0)?,
            content: row.get(1)?,
            wing: row.get(2)?,
            room: row.get(3)?,
            source_file: row.get(4)?,
            added_by: row.get(5)?,
            filed_at: row.get(6)?,
            date: row.get(7)?,
        })
    }

    fn delete_drawer_conn(conn: &rusqlite::Connection, id: &str) -> Result<usize, MemoryError> {
        // Remove from FTS index first (best-effort; ignore if table doesn't exist).
        let _ = conn.execute("DELETE FROM drawers_fts WHERE drawer_id = ?1", params![id]);
        Ok(conn.execute("DELETE FROM drawers WHERE id = ?1", params![id])?)
    }

    fn delete_drawers_by_source_file_conn(
        conn: &rusqlite::Connection,
        source_file: &str,
    ) -> Result<usize, MemoryError> {
        // Remove matching drawers from FTS index first (best-effort).
        let _ = conn.execute(
            "DELETE FROM drawers_fts WHERE drawer_id IN (
                 SELECT id FROM drawers WHERE source_file = ?1
             )",
            params![source_file],
        );
        Ok(conn.execute(
            "DELETE FROM drawers WHERE source_file = ?1",
            params![source_file],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::Database;

    fn dummy_embedding() -> Vec<f32> {
        vec![0.1; ironrace_embed::embedder::EMBED_DIM]
    }

    #[test]
    fn test_generate_id_deterministic() {
        let id1 = generate_id("hello", "wing1", "room1");
        let id2 = generate_id("hello", "wing1", "room1");
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 32);
        assert!(id1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_generate_id_varies_with_input() {
        let id1 = generate_id("hello", "wing1", "room1");
        let id2 = generate_id("world", "wing1", "room1");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_insert_and_get_drawer() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();

        db.insert_drawer(
            "abc123def456abc123def456abc123de",
            "test content",
            &emb,
            "w1",
            "r1",
            "",
            "mcp",
        )
        .unwrap();
        let drawer = db
            .get_drawer("abc123def456abc123def456abc123de")
            .unwrap()
            .unwrap();
        assert_eq!(drawer.content, "test content");
        assert_eq!(drawer.wing, "w1");
        assert_eq!(drawer.room, "r1");
    }

    #[test]
    fn test_upsert_preserves_filed_at() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();
        let id = "abc123def456abc123def456abc123de";

        db.insert_drawer(id, "v1", &emb, "w1", "r1", "", "mcp")
            .unwrap();
        let d1 = db.get_drawer(id).unwrap().unwrap();

        db.insert_drawer(id, "v2", &emb, "w1", "r1", "", "mcp")
            .unwrap();
        let d2 = db.get_drawer(id).unwrap().unwrap();

        assert_eq!(d2.content, "v2");
        assert_eq!(d1.filed_at, d2.filed_at); // filed_at preserved by ON CONFLICT
    }

    #[test]
    fn test_delete_drawer() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();
        let id = "abc123def456abc123def456abc123de";

        db.insert_drawer(id, "content", &emb, "w1", "r1", "", "mcp")
            .unwrap();
        assert!(db.delete_drawer(id).unwrap());
        assert!(!db.delete_drawer(id).unwrap()); // already deleted
        assert!(db.get_drawer(id).unwrap().is_none());
    }

    #[test]
    fn test_delete_drawers_by_source_file() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();

        db.insert_drawer(
            "abc123def456abc123def456abc123de",
            "content one",
            &emb,
            "w1",
            "r1",
            "/tmp/file-a.txt",
            "mcp",
        )
        .unwrap();
        db.insert_drawer(
            "bbb123def456abc123def456abc123de",
            "content two",
            &emb,
            "w1",
            "r1",
            "/tmp/file-a.txt",
            "mcp",
        )
        .unwrap();
        db.insert_drawer(
            "ccc123def456abc123def456abc123de",
            "content three",
            &emb,
            "w1",
            "r1",
            "/tmp/file-b.txt",
            "mcp",
        )
        .unwrap();

        let deleted = db.delete_drawers_by_source_file("/tmp/file-a.txt").unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(db.count_drawers(None).unwrap(), 1);
    }

    #[test]
    fn test_get_drawers_with_filters() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();

        db.insert_drawer(
            "id01aabbccddeeff01aabbccddeeff01",
            "c1",
            &emb,
            "alpha",
            "general",
            "",
            "mcp",
        )
        .unwrap();
        db.insert_drawer(
            "id02aabbccddeeff01aabbccddeeff02",
            "c2",
            &emb,
            "alpha",
            "notes",
            "",
            "mcp",
        )
        .unwrap();
        db.insert_drawer(
            "id03aabbccddeeff01aabbccddeeff03",
            "c3",
            &emb,
            "beta",
            "general",
            "",
            "mcp",
        )
        .unwrap();

        let all = db.get_drawers(None, None, 100).unwrap();
        assert_eq!(all.len(), 3);

        let alpha = db.get_drawers(Some("alpha"), None, 100).unwrap();
        assert_eq!(alpha.len(), 2);

        let general = db.get_drawers(None, Some("general"), 100).unwrap();
        assert_eq!(general.len(), 2);

        let alpha_notes = db.get_drawers(Some("alpha"), Some("notes"), 100).unwrap();
        assert_eq!(alpha_notes.len(), 1);
    }

    #[test]
    fn test_get_drawers_limit() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();

        for i in 0..5 {
            let id = format!("{:032x}", i);
            db.insert_drawer(&id, &format!("content {i}"), &emb, "w", "r", "", "mcp")
                .unwrap();
        }

        let limited = db.get_drawers(None, None, 2).unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn test_get_drawers_by_ids() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();

        db.insert_drawer(
            "aaaa0000bbbb1111cccc2222dddd3333",
            "c1",
            &emb,
            "w",
            "r",
            "",
            "mcp",
        )
        .unwrap();
        db.insert_drawer(
            "eeee4444ffff5555aaaa6666bbbb7777",
            "c2",
            &emb,
            "w",
            "r",
            "",
            "mcp",
        )
        .unwrap();

        let result = db
            .get_drawers_by_ids(&["aaaa0000bbbb1111cccc2222dddd3333", "missing_id"])
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("aaaa0000bbbb1111cccc2222dddd3333"));
    }

    #[test]
    fn test_get_drawers_by_ids_filtered_applies_sql_filters() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();

        db.insert_drawer(
            "aaaa0000bbbb1111cccc2222dddd3333",
            "c1",
            &emb,
            "alpha",
            "r1",
            "",
            "mcp",
        )
        .unwrap();
        db.insert_drawer(
            "eeee4444ffff5555aaaa6666bbbb7777",
            "c2",
            &emb,
            "beta",
            "r1",
            "",
            "mcp",
        )
        .unwrap();

        let result = db
            .get_drawers_by_ids_filtered(
                &[
                    "aaaa0000bbbb1111cccc2222dddd3333",
                    "eeee4444ffff5555aaaa6666bbbb7777",
                ],
                Some("alpha"),
                Some("r1"),
            )
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("aaaa0000bbbb1111cccc2222dddd3333"));
    }

    #[test]
    fn test_get_drawers_by_ids_empty() {
        let db = Database::open_in_memory().unwrap();
        let result = db.get_drawers_by_ids(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_count_drawers() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();

        assert_eq!(db.count_drawers(None).unwrap(), 0);

        db.insert_drawer(
            "id01aabbccddeeff01aabbccddeeff01",
            "c1",
            &emb,
            "w1",
            "r",
            "",
            "mcp",
        )
        .unwrap();
        db.insert_drawer(
            "id02aabbccddeeff01aabbccddeeff02",
            "c2",
            &emb,
            "w2",
            "r",
            "",
            "mcp",
        )
        .unwrap();

        assert_eq!(db.count_drawers(None).unwrap(), 2);
        assert_eq!(db.count_drawers(Some("w1")).unwrap(), 1);
    }

    #[test]
    fn test_wing_counts() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();

        db.insert_drawer(
            "id01aabbccddeeff01aabbccddeeff01",
            "c1",
            &emb,
            "alpha",
            "r",
            "",
            "mcp",
        )
        .unwrap();
        db.insert_drawer(
            "id02aabbccddeeff01aabbccddeeff02",
            "c2",
            &emb,
            "alpha",
            "r",
            "",
            "mcp",
        )
        .unwrap();
        db.insert_drawer(
            "id03aabbccddeeff01aabbccddeeff03",
            "c3",
            &emb,
            "beta",
            "r",
            "",
            "mcp",
        )
        .unwrap();

        let counts = db.wing_counts().unwrap();
        assert_eq!(counts.len(), 2);
        assert!(counts.iter().any(|(w, c)| w == "alpha" && *c == 2));
        assert!(counts.iter().any(|(w, c)| w == "beta" && *c == 1));
    }

    #[test]
    fn test_taxonomy() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();

        db.insert_drawer(
            "id01aabbccddeeff01aabbccddeeff01",
            "c1",
            &emb,
            "w1",
            "r1",
            "",
            "mcp",
        )
        .unwrap();
        db.insert_drawer(
            "id02aabbccddeeff01aabbccddeeff02",
            "c2",
            &emb,
            "w1",
            "r2",
            "",
            "mcp",
        )
        .unwrap();

        let tax = db.taxonomy().unwrap();
        assert_eq!(tax.get("w1").unwrap().len(), 2);
    }

    #[test]
    fn test_load_all_vectors() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();

        db.insert_drawer(
            "id01aabbccddeeff01aabbccddeeff01",
            "c",
            &emb,
            "w",
            "r",
            "",
            "mcp",
        )
        .unwrap();

        let vectors = db.load_all_vectors().unwrap();
        assert_eq!(vectors.len(), 1);
        assert_eq!(vectors[0].0, "id01aabbccddeeff01aabbccddeeff01");
        assert_eq!(vectors[0].1, emb);
    }

    #[test]
    fn test_drawer_insert_rolls_back_if_wal_fails() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();
        let id = "abc123def456abc123def456abc123de";

        db.conn
            .execute_batch(
                "CREATE TRIGGER wal_fail
                 BEFORE INSERT ON wal_log
                 WHEN NEW.operation = 'fail'
                 BEGIN
                     SELECT RAISE(ABORT, 'forced wal failure');
                 END;",
            )
            .unwrap();

        let err = db
            .with_transaction(|tx| {
                Database::insert_drawer_tx(tx, id, "content", &emb, "w1", "r1", "", "mcp")?;
                Database::wal_log_tx(tx, "fail", &serde_json::json!({"id": id}), None)?;
                Ok(())
            })
            .unwrap_err();

        assert!(err.to_string().contains("forced wal failure"));
        assert!(db.get_drawer(id).unwrap().is_none());
    }
}

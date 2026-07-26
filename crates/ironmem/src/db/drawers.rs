use rusqlite::{params, OptionalExtension, Transaction};
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

/// Sentinel prefix that marks a drawer as a synthetic preference sibling.
/// A synthetic drawer has `source_file = "pref:<parent_drawer_id>"`.
pub(crate) const PREF_SENTINEL: &str = "pref:";

/// Transport-only collab message bodies are retrieved by their opaque drawer
/// references and must not be discoverable through generic memory search.
const COLLAB_MESSAGE_ROOM: &str = "collab-messages";

/// Cosine-similarity floor for treating two current drawers in one wing/room as
/// near duplicates. Tuned for the `EMBED_DIM`-wide vectors the bundled embedder
/// produces; swapping the embedding model invalidates this constant.
const NEAR_DUPLICATE_THRESHOLD: f32 = 0.92;

/// Most recent current drawers a single near-duplicate check will scan within
/// one wing/room. Bounds the advisory check's cost in rooms far larger than the
/// scan was designed for; see `find_near_duplicate`.
const NEAR_DUPLICATE_SCAN_CAP: usize = 2_000;

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
    pub superseded_by: Option<String>,
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
    /// Retained historical rows are omitted from normal search, but callers
    /// may opt in when reconstructing temporal state.
    pub include_superseded: bool,
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

    /// Read a drawer's `(wing, room, superseded_by)` lineage triple, if it exists.
    fn drawer_lineage_tx(
        tx: &Transaction<'_>,
        id: &str,
    ) -> Result<Option<(String, String, Option<String>)>, MemoryError> {
        Ok(tx
            .query_row(
                "SELECT wing, room, superseded_by FROM drawers WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?)
    }

    /// Mark a current predecessor as superseded inside the caller's transaction.
    ///
    /// The successor must already exist in this transaction — callers insert it
    /// first, which keeps the write atomic. Both rows are re-checked here so a
    /// missing, cross-scope, or already-superseded successor can never be
    /// linked: a successor that is itself superseded would close the lineage
    /// into a cycle, and because every `superseded_by IS NULL` filter would then
    /// reject every row in that cycle, the whole chain would vanish from
    /// default search with no way to restore it through the public API.
    ///
    /// Re-marking the identical predecessor/successor pair is idempotent, so a
    /// replayed `add_drawer` succeeds; a *different* successor is rejected.
    pub(crate) fn mark_drawer_superseded_tx(
        tx: &Transaction<'_>,
        predecessor_id: &str,
        successor_id: &str,
        wing: &str,
        room: &str,
    ) -> Result<(), MemoryError> {
        if predecessor_id == successor_id {
            return Err(MemoryError::Validation(
                "successor drawer must differ from predecessor drawer".into(),
            ));
        }

        let Some((actual_wing, actual_room, predecessor_superseded_by)) =
            Self::drawer_lineage_tx(tx, predecessor_id)?
        else {
            return Err(MemoryError::NotFound("predecessor drawer not found".into()));
        };
        if actual_wing != wing || actual_room != room {
            return Err(MemoryError::Validation(
                "predecessor drawer does not belong to requested wing/room".into(),
            ));
        }
        // An identical replay settles here regardless of what has happened to
        // the successor since, so a retried `add_drawer` stays idempotent.
        if predecessor_superseded_by.as_deref() == Some(successor_id) {
            return Ok(());
        }
        if predecessor_superseded_by.is_some() {
            return Err(MemoryError::Validation(
                "predecessor drawer is already superseded by a different successor".into(),
            ));
        }

        let Some((successor_wing, successor_room, successor_superseded_by)) =
            Self::drawer_lineage_tx(tx, successor_id)?
        else {
            return Err(MemoryError::NotFound("successor drawer not found".into()));
        };
        if successor_wing != wing || successor_room != room {
            return Err(MemoryError::Validation(
                "successor drawer does not belong to requested wing/room".into(),
            ));
        }
        if successor_superseded_by.is_some() {
            return Err(MemoryError::Validation(
                "successor drawer is itself superseded; supersede the current drawer instead"
                    .into(),
            ));
        }

        // Still a guarded compare-and-set: the predecessor read above shares
        // this transaction, but the `IS NULL` guard keeps the write correct
        // even if that snapshot were stale.
        let updated = tx.execute(
            "UPDATE drawers
             SET superseded_by = ?1
             WHERE id = ?2 AND wing = ?3 AND room = ?4 AND superseded_by IS NULL",
            params![successor_id, predecessor_id, wing, room],
        )?;
        if updated == 1 {
            Ok(())
        } else {
            Err(MemoryError::Validation(
                "predecessor drawer could not be superseded".into(),
            ))
        }
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

        // Re-filing content resurrects the row as current. Drawer ids are
        // derived from content (or logical_key) + wing + room, so a re-add or a
        // logical_key rewrite lands back on a row that may already be marked
        // superseded; leaving the marker set would commit the new body into a
        // row every `superseded_by IS NULL` filter hides, reporting success for
        // a write nothing can retrieve.
        let resurrect_current = if Self::has_superseded_by_column(conn)? {
            ",\n                superseded_by = NULL"
        } else {
            ""
        };

        conn.execute(
            &format!(
                "INSERT INTO drawers (id, content, embedding, wing, room, source_file, added_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                content = excluded.content,
                embedding = excluded.embedding,
                wing = excluded.wing,
                room = excluded.room,
                source_file = excluded.source_file,
                added_by = excluded.added_by{resurrect_current}"
            ),
            params![id, content, blob, wing, room, source_file, added_by],
        )?;

        if room == COLLAB_MESSAGE_ROOM {
            return Ok(());
        }

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
        // Cascade: any synthetic sibling drawer points back via source_file = "pref:<id>".
        Self::delete_drawers_by_parent_tx(tx, id)?;
        let count = Self::delete_drawer_conn(tx, id)?;
        Ok(count > 0)
    }

    pub(crate) fn delete_drawers_by_parent_tx(
        tx: &Transaction<'_>,
        parent_id: &str,
    ) -> Result<usize, MemoryError> {
        let sentinel = format!("{PREF_SENTINEL}{parent_id}");
        // Remove matching drawers from FTS index first (best-effort).
        // Silently skip if the FTS table doesn't exist yet (pre-migration DBs).
        if let Err(e) = tx.execute(
            "DELETE FROM drawers_fts WHERE drawer_id IN \
             (SELECT id FROM drawers WHERE source_file = ?1)",
            params![sentinel],
        ) {
            tracing::debug!("FTS5 sync skipped (table may not exist yet): {e}");
        }
        let n = tx.execute(
            "DELETE FROM drawers WHERE source_file = ?1",
            params![sentinel],
        )?;
        Ok(n)
    }

    pub(crate) fn delete_drawers_by_source_file_tx(
        tx: &Transaction<'_>,
        source_file: &str,
    ) -> Result<usize, MemoryError> {
        Self::delete_drawers_by_source_file_conn(tx, source_file)
    }

    /// Get a drawer by ID.
    pub fn get_drawer(&self, id: &str) -> Result<Option<Drawer>, MemoryError> {
        let columns = Self::drawer_projection(&self.conn)?;
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {columns} FROM drawers WHERE id = ?1"))?;

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
        self.get_drawers_by_ids_filtered(ids, None, None, true)
    }

    /// Get multiple drawers by IDs with optional metadata filters applied in SQL.
    pub fn get_drawers_by_ids_filtered(
        &self,
        ids: &[&str],
        wing: Option<&str>,
        room: Option<&str>,
        include_superseded: bool,
    ) -> Result<std::collections::HashMap<String, Drawer>, MemoryError> {
        const CHUNK_SIZE: usize = 900; // Stay well under SQLite's 999 limit

        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let mut result = std::collections::HashMap::new();
        let columns = Self::drawer_projection(&self.conn)?;

        for chunk in ids.chunks(CHUNK_SIZE) {
            let placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut sql = format!("SELECT {columns} FROM drawers WHERE id IN ({placeholders})");
            let mut owned_params: Vec<String> = Vec::new();
            if let Some(w) = wing {
                sql.push_str(" AND wing = ?");
                owned_params.push(w.to_string());
            }
            if let Some(r) = room {
                sql.push_str(" AND room = ?");
                owned_params.push(r.to_string());
            }
            if !include_superseded && Self::has_superseded_by_column(&self.conn)? {
                sql.push_str(" AND superseded_by IS NULL");
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
        let columns = Self::drawer_projection(&self.conn)?;

        let mut result = Vec::new();
        match (wing, room) {
            (Some(w), Some(r)) => {
                let mut stmt = self.conn.prepare(&format!(
                    "SELECT {columns} FROM drawers WHERE wing = ?1 AND room = ?2 ORDER BY filed_at DESC, id ASC LIMIT ?3"
                ))?;
                let rows = stmt.query_map(params![w, r, limit_i64], Self::row_to_drawer)?;
                for row in rows {
                    result.push(row?);
                }
            }
            (Some(w), None) => {
                let mut stmt = self.conn.prepare(&format!(
                    "SELECT {columns} FROM drawers WHERE wing = ?1 ORDER BY filed_at DESC, id ASC LIMIT ?2"
                ))?;
                let rows = stmt.query_map(params![w, limit_i64], Self::row_to_drawer)?;
                for row in rows {
                    result.push(row?);
                }
            }
            (None, Some(r)) => {
                let mut stmt = self.conn.prepare(&format!(
                    "SELECT {columns} FROM drawers WHERE room = ?1 ORDER BY filed_at DESC, id ASC LIMIT ?2"
                ))?;
                let rows = stmt.query_map(params![r, limit_i64], Self::row_to_drawer)?;
                for row in rows {
                    result.push(row?);
                }
            }
            (None, None) => {
                let mut stmt = self.conn.prepare(&format!(
                    "SELECT {columns} FROM drawers ORDER BY filed_at DESC, id ASC LIMIT ?1"
                ))?;
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

    /// Find the best current drawer in one wing/room whose embedding is close
    /// enough to `embedding` to be treated as a near duplicate.
    ///
    /// Advisory only: the result decorates an `add_drawer` response and never
    /// changes what was stored, so a miss caused by [`NEAR_DUPLICATE_SCAN_CAP`]
    /// is acceptable where a slow write would not be.
    pub(crate) fn find_near_duplicate(
        &self,
        embedding: &[f32],
        wing: &str,
        room: &str,
        exclude_id: &str,
    ) -> Result<Option<(String, f32)>, MemoryError> {
        let Some(query_norm) = vector_norm(embedding) else {
            return Ok(None);
        };
        if !Self::has_superseded_by_column(&self.conn)? {
            return Ok(None);
        }

        // ponytail: linear scan of the most recent NEAR_DUPLICATE_SCAN_CAP current
        // drawers in one wing/room, decoding an EMBED_DIM f32 blob per row — an
        // uncapped scan cost ~56MB of blob reads in this instance's largest room
        // (~18k drawers) on every add_drawer. The cap bounds that to a constant at
        // the price of missing duplicates older than the newest N; replace with
        // filtered ANN retrieval over the existing HNSW index to remove both limits.
        let mut stmt = self.conn.prepare(
            "SELECT id, embedding FROM drawers
             WHERE wing = ?1 AND room = ?2 AND id != ?3 AND superseded_by IS NULL
               AND source_file NOT LIKE ?4
             ORDER BY filed_at DESC, id ASC
             LIMIT ?5",
        )?;
        let synthetic_pattern = format!("{PREF_SENTINEL}%");
        let rows = stmt.query_map(
            params![
                wing,
                room,
                exclude_id,
                synthetic_pattern,
                NEAR_DUPLICATE_SCAN_CAP as i64
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )?;

        // Malformed rows are counted, not logged individually: swapping the
        // embedding model makes every stored row mismatch, which would
        // otherwise emit one warning per row on every add_drawer.
        let mut skipped_malformed = 0_usize;
        let mut best: Option<(String, f32)> = None;
        for row in rows {
            let (id, blob) = row?;
            let Some(candidate) = decode_embedding(&blob) else {
                skipped_malformed += 1;
                continue;
            };
            if candidate.len() != embedding.len() {
                skipped_malformed += 1;
                continue;
            }
            let Some(candidate_norm) = vector_norm(&candidate) else {
                skipped_malformed += 1;
                continue;
            };
            let dot_product: f32 = embedding
                .iter()
                .zip(&candidate)
                .map(|(query, stored)| query * stored)
                .sum();
            let score = dot_product / (query_norm * candidate_norm);
            if !score.is_finite() {
                skipped_malformed += 1;
                continue;
            }
            if score < NEAR_DUPLICATE_THRESHOLD {
                continue;
            }

            let is_better = best.as_ref().is_none_or(|(best_id, best_score)| {
                score > *best_score || (score == *best_score && id < *best_id)
            });
            if is_better {
                best = Some((id, score));
            }
        }
        if skipped_malformed > 0 {
            tracing::warn!(
                skipped = skipped_malformed,
                wing,
                room,
                "skipped drawers with unusable embeddings while checking near duplicates"
            );
        }
        Ok(best)
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
        self.bm25_search_with_history(query, limit, wing, room, false)
    }

    /// BM25 full-text search with optional retained-history inclusion.
    pub fn bm25_search_with_history(
        &self,
        query: &str,
        limit: usize,
        wing: Option<&str>,
        room: Option<&str>,
        include_superseded: bool,
    ) -> Result<Vec<(String, f32)>, MemoryError> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let fts_query = fts5_sanitize(query);
        let limit_i64 = limit as i64;
        // A pre-v17 database has no physical `superseded_by`, so the filter must
        // be omitted there rather than referencing an absent column. (Reads of
        // that database project the field as NULL — see `drawer_projection`.)
        let current_only = !include_superseded && Self::has_superseded_by_column(&self.conn)?;
        let supersession_filter = if current_only {
            " AND d.superseded_by IS NULL"
        } else {
            ""
        };

        // FTS5 will not resolve `MATCH`/`bm25()` through a table alias — an
        // aliased form fails to prepare with "no such column", which the
        // handler below degrades to "no lexical results". Always name
        // `drawers_fts` in full.
        let sql = match (wing, room) {
            (Some(_), Some(_)) => {
                format!(
                    "SELECT drawers_fts.drawer_id, -bm25(drawers_fts) AS score
                 FROM drawers_fts
                 JOIN drawers d ON d.id = drawers_fts.drawer_id
                 WHERE drawers_fts MATCH ?1 AND d.wing = ?3 AND d.room = ?4{supersession_filter}
                 ORDER BY score DESC LIMIT ?2"
                )
            }
            (Some(_), None) => {
                format!(
                    "SELECT drawers_fts.drawer_id, -bm25(drawers_fts) AS score
                 FROM drawers_fts
                 JOIN drawers d ON d.id = drawers_fts.drawer_id
                 WHERE drawers_fts MATCH ?1 AND d.wing = ?3{supersession_filter}
                 ORDER BY score DESC LIMIT ?2"
                )
            }
            (None, Some(_)) => {
                format!(
                    "SELECT drawers_fts.drawer_id, -bm25(drawers_fts) AS score
                 FROM drawers_fts
                 JOIN drawers d ON d.id = drawers_fts.drawer_id
                 WHERE drawers_fts MATCH ?1 AND d.room = ?3{supersession_filter}
                 ORDER BY score DESC LIMIT ?2"
                )
            }
            (None, None) => {
                format!(
                    "SELECT drawers_fts.drawer_id, -bm25(drawers_fts) AS score
                 FROM drawers_fts
                 JOIN drawers d ON d.id = drawers_fts.drawer_id
                 WHERE drawers_fts MATCH ?1{supersession_filter}
                 ORDER BY score DESC LIMIT ?2"
                )
            }
        };

        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => {
                // Only a genuinely absent FTS table may degrade to "no lexical
                // results". This SQL is generated, so any other prepare failure
                // is a bug in the builder above, and silently returning an
                // empty vec would hide it as a mere recall dip.
                if missing_fts_table(&e) {
                    tracing::debug!("BM25 search skipped (FTS table does not exist): {e}");
                    return Ok(vec![]);
                }
                tracing::warn!("BM25 search failed to prepare (using vector-only): {sql}: {e}");
                return Err(e.into());
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
                // A user query that FTS5 rejects is expected; a parameter-count
                // mismatch is a builder bug and must not look like "no hits".
                if matches!(e, rusqlite::Error::InvalidParameterCount(..)) {
                    tracing::warn!("BM25 query bound the wrong parameter count: {sql}: {e}");
                    return Err(e.into());
                }
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
            superseded_by: row.get(8)?,
        })
    }

    /// Project the v17 supersession field when available, or an explicit NULL
    /// for pre-v17 databases opened through the non-migrating read-only path.
    fn drawer_projection(conn: &rusqlite::Connection) -> Result<&'static str, MemoryError> {
        if Self::has_superseded_by_column(conn)? {
            return Ok(
                "id, content, wing, room, source_file, added_by, filed_at, date, superseded_by",
            );
        }
        Ok("id, content, wing, room, source_file, added_by, filed_at, date, NULL AS superseded_by")
    }

    fn has_superseded_by_column(conn: &rusqlite::Connection) -> Result<bool, MemoryError> {
        let mut stmt = conn.prepare("PRAGMA table_info(drawers)")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == "superseded_by" {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Restore any predecessor that pointed at rows about to be deleted.
    ///
    /// `superseded_by` has no foreign key, so without this a delete would leave
    /// a predecessor pointing at a row that no longer exists — permanently
    /// hidden from every `superseded_by IS NULL` filter even though it is now
    /// the only surviving version of that memory.
    fn clear_dangling_supersession_conn(
        conn: &rusqlite::Connection,
        doomed_ids_sql: &str,
        params: &[&dyn rusqlite::types::ToSql],
    ) -> Result<(), MemoryError> {
        if !Self::has_superseded_by_column(conn)? {
            return Ok(());
        }
        let restored = conn.execute(
            &format!(
                "UPDATE drawers SET superseded_by = NULL WHERE superseded_by IN ({doomed_ids_sql})"
            ),
            params,
        )?;
        if restored > 0 {
            tracing::info!(
                restored,
                "restored superseded predecessors whose successor was deleted"
            );
        }
        Ok(())
    }

    fn delete_drawer_conn(conn: &rusqlite::Connection, id: &str) -> Result<usize, MemoryError> {
        Self::clear_dangling_supersession_conn(conn, "?1", params![id])?;
        // Remove from FTS index first (best-effort; ignore if table doesn't exist).
        let _ = conn.execute("DELETE FROM drawers_fts WHERE drawer_id = ?1", params![id]);
        Ok(conn.execute("DELETE FROM drawers WHERE id = ?1", params![id])?)
    }

    fn delete_drawers_by_source_file_conn(
        conn: &rusqlite::Connection,
        source_file: &str,
    ) -> Result<usize, MemoryError> {
        Self::clear_dangling_supersession_conn(
            conn,
            "SELECT id FROM drawers WHERE source_file = ?1",
            params![source_file],
        )?;
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

/// True when a prepare failure means the FTS5 table itself is absent (a
/// pre-migration database), as opposed to malformed generated SQL.
fn missing_fts_table(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(_, Some(message))
            if message.contains("no such table") && message.contains("drawers_fts")
    )
}

fn decode_embedding(blob: &[u8]) -> Option<Vec<f32>> {
    if !blob.len().is_multiple_of(std::mem::size_of::<f32>()) {
        return None;
    }
    let embedding: Vec<f32> = blob
        .chunks_exact(std::mem::size_of::<f32>())
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    embedding
        .iter()
        .all(|value| value.is_finite())
        .then_some(embedding)
}

fn vector_norm(embedding: &[f32]) -> Option<f32> {
    let squared_sum: f32 = embedding.iter().map(|value| value * value).sum();
    let norm = squared_sum.sqrt();
    (norm.is_finite() && norm > 0.0).then_some(norm)
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
    fn get_drawers_tiebreaks_equal_filed_at_by_id_for_every_filter() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();
        let ids = [
            "aaaa0000bbbb1111cccc2222dddd3333",
            "ffff0000bbbb1111cccc2222dddd3333",
        ];

        for id in ids {
            db.insert_drawer(id, "content", &emb, "wing", "room", "", "mcp")
                .unwrap();
        }
        db.conn
            .execute("UPDATE drawers SET filed_at = '2025-01-01T00:00:00Z'", [])
            .unwrap();

        let assert_ids = |wing, room| {
            let drawers = db.get_drawers(wing, room, 10).unwrap();
            assert_eq!(
                drawers
                    .iter()
                    .map(|drawer| drawer.id.as_str())
                    .collect::<Vec<_>>(),
                ids
            );
        };
        assert_ids(None, None);
        assert_ids(Some("wing"), None);
        assert_ids(None, Some("room"));
        assert_ids(Some("wing"), Some("room"));
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
                true,
            )
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("aaaa0000bbbb1111cccc2222dddd3333"));
    }

    #[test]
    fn bm25_search_hides_superseded_rows_unless_history_is_requested() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();
        let predecessor = "aaaa0000bbbb1111cccc2222dddd3333";
        let successor = "eeee4444ffff5555aaaa6666bbbb7777";
        db.insert_drawer(
            predecessor,
            "searchable temporal record version one",
            &emb,
            "project",
            "state",
            "",
            "mcp",
        )
        .unwrap();
        db.insert_drawer(
            successor,
            "searchable temporal record version two",
            &emb,
            "project",
            "state",
            "",
            "mcp",
        )
        .unwrap();
        db.conn
            .execute(
                "UPDATE drawers SET superseded_by = ?1 WHERE id = ?2",
                params![successor, predecessor],
            )
            .unwrap();

        let current_ids: Vec<String> = db
            .bm25_search("searchable temporal record", 10, None, None)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(current_ids, vec![successor]);

        let history_ids: Vec<String> = db
            .bm25_search_with_history("searchable temporal record", 10, None, None, true)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(history_ids.len(), 2);
        assert!(history_ids.contains(&predecessor.to_string()));
        assert!(history_ids.contains(&successor.to_string()));
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

    #[test]
    fn mark_drawer_superseded_tx_updates_only_a_current_same_scope_predecessor() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();
        let predecessor = "predecessor00000000000000000000001";
        let successor = "successor0000000000000000000000001";
        db.insert_drawer(predecessor, "old", &emb, "wing", "room", "", "test")
            .unwrap();
        db.insert_drawer(successor, "new", &emb, "wing", "room", "", "test")
            .unwrap();

        db.with_transaction(|tx| {
            Database::mark_drawer_superseded_tx(tx, predecessor, successor, "wing", "room")
        })
        .unwrap();

        assert_eq!(
            db.get_drawer(predecessor).unwrap().unwrap().superseded_by,
            Some(successor.to_string())
        );
    }

    #[test]
    fn mark_drawer_superseded_tx_allows_same_successor_replay_but_rejects_a_different_one() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();
        let predecessor = "replaypredecessor000000000000000001";
        let successor = "replaysuccessor000000000000000000001";
        let different_successor = "different-successor00000000000000001";
        for id in [predecessor, successor, different_successor] {
            db.insert_drawer(id, "content", &emb, "wing", "room", "", "test")
                .unwrap();
        }

        db.with_transaction(|tx| {
            Database::mark_drawer_superseded_tx(tx, predecessor, successor, "wing", "room")
        })
        .unwrap();
        db.with_transaction(|tx| {
            Database::mark_drawer_superseded_tx(tx, predecessor, successor, "wing", "room")
        })
        .unwrap();

        let different = db
            .with_transaction(|tx| {
                Database::mark_drawer_superseded_tx(
                    tx,
                    predecessor,
                    different_successor,
                    "wing",
                    "room",
                )
            })
            .unwrap_err();
        assert!(matches!(different, MemoryError::Validation(_)));
        assert!(different.to_string().contains("different successor"));
    }

    #[test]
    fn mark_drawer_superseded_tx_validates_successor_before_marking() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();
        let predecessor = "validatedpredecessor0000000000000001";
        let successor = "validatedsuccessor000000000000000001";
        let cross_scope = "crossscopesuccessor000000000000000001";
        db.insert_drawer(predecessor, "old", &emb, "wing", "room", "", "test")
            .unwrap();
        db.insert_drawer(successor, "new", &emb, "wing", "room", "", "test")
            .unwrap();
        db.insert_drawer(
            cross_scope,
            "wrong scope",
            &emb,
            "other-wing",
            "room",
            "",
            "test",
        )
        .unwrap();

        let missing = db
            .with_transaction(|tx| {
                Database::mark_drawer_superseded_tx(tx, predecessor, "missing", "wing", "room")
            })
            .unwrap_err();
        assert!(matches!(missing, MemoryError::NotFound(_)));
        assert!(missing.to_string().contains("successor drawer not found"));

        let self_reference = db
            .with_transaction(|tx| {
                Database::mark_drawer_superseded_tx(tx, predecessor, predecessor, "wing", "room")
            })
            .unwrap_err();
        assert!(matches!(self_reference, MemoryError::Validation(_)));
        assert!(self_reference.to_string().contains("must differ"));

        let wrong_scope = db
            .with_transaction(|tx| {
                Database::mark_drawer_superseded_tx(tx, predecessor, cross_scope, "wing", "room")
            })
            .unwrap_err();
        assert!(matches!(wrong_scope, MemoryError::Validation(_)));
        assert!(wrong_scope.to_string().contains("does not belong"));
        assert!(db
            .get_drawer(predecessor)
            .unwrap()
            .unwrap()
            .superseded_by
            .is_none());
    }

    #[test]
    fn mark_drawer_superseded_tx_rejects_missing_and_cross_scope_predecessors() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();
        let predecessor = "scopepredecessor00000000000000001";
        let successor = "scopesuccessor00000000000000000001";
        db.insert_drawer(predecessor, "old", &emb, "other-wing", "room", "", "test")
            .unwrap();
        db.insert_drawer(successor, "new", &emb, "wing", "room", "", "test")
            .unwrap();

        let missing = db
            .with_transaction(|tx| {
                Database::mark_drawer_superseded_tx(tx, "missing", successor, "wing", "room")
            })
            .unwrap_err();
        assert!(matches!(missing, MemoryError::NotFound(_)));
        assert!(missing.to_string().contains("not found"));

        let cross_scope = db
            .with_transaction(|tx| {
                Database::mark_drawer_superseded_tx(tx, predecessor, successor, "wing", "room")
            })
            .unwrap_err();
        assert!(matches!(cross_scope, MemoryError::Validation(_)));
        assert!(cross_scope.to_string().contains("does not belong"));
    }

    /// Drawer ids are derived from content, so re-filing a fact lands back on
    /// the row that was superseded. Leaving the marker set would commit the new
    /// body into a row every search filter hides.
    #[test]
    fn readding_superseded_content_restores_the_row_as_current() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();
        let predecessor = generate_id("use tabs", "wing", "room");
        let successor = generate_id("use spaces", "wing", "room");
        db.insert_drawer(&predecessor, "use tabs", &emb, "wing", "room", "", "test")
            .unwrap();
        db.insert_drawer(&successor, "use spaces", &emb, "wing", "room", "", "test")
            .unwrap();
        db.with_transaction(|tx| {
            Database::mark_drawer_superseded_tx(tx, &predecessor, &successor, "wing", "room")
        })
        .unwrap();
        assert!(db
            .get_drawer(&predecessor)
            .unwrap()
            .unwrap()
            .superseded_by
            .is_some());

        // Re-file the original fact: same content -> same id -> upsert.
        db.insert_drawer(&predecessor, "use tabs", &emb, "wing", "room", "", "test")
            .unwrap();

        assert_eq!(
            db.get_drawer(&predecessor).unwrap().unwrap().superseded_by,
            None,
            "a re-filed drawer must be current again, not a durable but hidden row"
        );
        let visible = db
            .bm25_search("use tabs", 10, Some("wing"), Some("room"))
            .unwrap();
        assert!(
            visible.iter().any(|(id, _)| id == &predecessor),
            "the resurrected drawer must be retrievable by default search"
        );
    }

    /// A successor that is itself superseded closes the lineage into a cycle in
    /// which every row is hidden and none can be recovered through the API.
    #[test]
    fn mark_drawer_superseded_tx_rejects_a_successor_that_is_itself_superseded() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();
        let (a, b, c) = ("a".repeat(32), "b".repeat(32), "c".repeat(32));
        for id in [&a, &b, &c] {
            db.insert_drawer(id, "body", &emb, "wing", "room", "", "test")
                .unwrap();
        }

        db.with_transaction(|tx| Database::mark_drawer_superseded_tx(tx, &a, &b, "wing", "room"))
            .unwrap();
        db.with_transaction(|tx| Database::mark_drawer_superseded_tx(tx, &b, &c, "wing", "room"))
            .unwrap();

        // b is now superseded by c, so it must not be accepted as a successor.
        let cycle = db
            .with_transaction(|tx| Database::mark_drawer_superseded_tx(tx, &c, &b, "wing", "room"))
            .unwrap_err();
        assert!(matches!(cycle, MemoryError::Validation(_)));
        assert!(cycle.to_string().contains("itself superseded"));
        assert!(
            db.get_drawer(&c).unwrap().unwrap().superseded_by.is_none(),
            "the head of the chain must stay current"
        );
    }

    /// `superseded_by` has no foreign key, so a delete must clear inbound
    /// pointers or the predecessor stays hidden behind a successor that is gone.
    #[test]
    fn deleting_a_successor_restores_its_predecessor_to_current() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();
        let predecessor = "a".repeat(32);
        let successor = "b".repeat(32);
        db.insert_drawer(&predecessor, "old", &emb, "wing", "room", "", "test")
            .unwrap();
        db.insert_drawer(&successor, "new", &emb, "wing", "room", "", "test")
            .unwrap();
        db.with_transaction(|tx| {
            Database::mark_drawer_superseded_tx(tx, &predecessor, &successor, "wing", "room")
        })
        .unwrap();

        assert!(db.delete_drawer(&successor).unwrap());

        assert_eq!(
            db.get_drawer(&predecessor).unwrap().unwrap().superseded_by,
            None,
            "deleting the successor must not strand the predecessor as permanently hidden"
        );
    }

    /// FTS5 cannot resolve `MATCH`/`bm25()` through a table alias, so every
    /// scoped branch must name `drawers_fts` in full and bind the right number
    /// of parameters. Without this, scoped lexical search returns nothing and
    /// the failure is invisible.
    #[test]
    fn bm25_search_returns_hits_for_every_wing_room_scope() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();
        let id = "a".repeat(32);
        db.insert_drawer(
            &id,
            "zebra quokka narwhal",
            &emb,
            "wing",
            "room",
            "",
            "test",
        )
        .unwrap();

        for (wing, room) in [
            (None, None),
            (Some("wing"), None),
            (None, Some("room")),
            (Some("wing"), Some("room")),
        ] {
            let hits = db.bm25_search("zebra quokka", 10, wing, room).unwrap();
            assert_eq!(
                hits.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
                vec![id.as_str()],
                "scope ({wing:?}, {room:?}) must return the matching drawer"
            );
        }
    }

    /// The supersession filter has to hold in every scoped branch, not just the
    /// unscoped one.
    #[test]
    fn bm25_search_hides_superseded_rows_in_every_scope() {
        let db = Database::open_in_memory().unwrap();
        let emb = dummy_embedding();
        let predecessor = "a".repeat(32);
        let successor = "b".repeat(32);
        db.insert_drawer(
            &predecessor,
            "zebra quokka one",
            &emb,
            "wing",
            "room",
            "",
            "test",
        )
        .unwrap();
        db.insert_drawer(
            &successor,
            "zebra quokka two",
            &emb,
            "wing",
            "room",
            "",
            "test",
        )
        .unwrap();
        db.with_transaction(|tx| {
            Database::mark_drawer_superseded_tx(tx, &predecessor, &successor, "wing", "room")
        })
        .unwrap();

        for (wing, room) in [
            (None, None),
            (Some("wing"), None),
            (None, Some("room")),
            (Some("wing"), Some("room")),
        ] {
            let current = db.bm25_search("zebra quokka", 10, wing, room).unwrap();
            assert_eq!(
                current
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>(),
                vec![successor.as_str()],
                "scope ({wing:?}, {room:?}) must hide the superseded row"
            );

            let history = db
                .bm25_search_with_history("zebra quokka", 10, wing, room, true)
                .unwrap();
            assert_eq!(
                history.len(),
                2,
                "scope ({wing:?}, {room:?}) must return history on request"
            );
        }
    }

    #[test]
    fn find_near_duplicate_returns_deterministic_current_same_scope_match() {
        let db = Database::open_in_memory().unwrap();
        let query = vec![1.0, 0.0];
        let id_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let id_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let excluded = "cccccccccccccccccccccccccccccccc";
        let superseded = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        db.insert_drawer(id_b, "same", &query, "wing", "room", "", "test")
            .unwrap();
        db.insert_drawer(id_a, "same", &query, "wing", "room", "", "test")
            .unwrap();
        db.insert_drawer(excluded, "same", &query, "wing", "room", "", "test")
            .unwrap();
        db.insert_drawer(superseded, "old", &query, "wing", "room", "", "test")
            .unwrap();
        db.insert_drawer(
            "malformed0000000000000000000000000",
            "malformed",
            &query,
            "wing",
            "room",
            "",
            "test",
        )
        .unwrap();
        db.insert_drawer(
            "dddddddddddddddddddddddddddddddd",
            "other scope",
            &query,
            "other-wing",
            "room",
            "",
            "test",
        )
        .unwrap();
        db.conn
            .execute(
                "UPDATE drawers SET superseded_by = 'newer' WHERE id = ?1",
                [superseded],
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE drawers SET embedding = ?1 WHERE id = ?2",
                rusqlite::params![vec![0_u8; 3], "malformed0000000000000000000000000"],
            )
            .unwrap();

        let duplicate = db
            .find_near_duplicate(&query, "wing", "room", excluded)
            .unwrap()
            .expect("a current, in-scope duplicate");
        assert_eq!(duplicate.0, id_a);
        assert!((duplicate.1 - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn find_near_duplicate_ignores_synthetic_preference_siblings() {
        let db = Database::open_in_memory().unwrap();
        let query = vec![1.0, 0.0];
        let synthetic_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let real_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let real_vector = [0.95, (1.0_f32 - 0.95_f32.powi(2)).sqrt()];

        db.insert_drawer(
            synthetic_id,
            "synthetic preference artifact",
            &query,
            "wing",
            "room",
            &format!("{PREF_SENTINEL}parent"),
            "mcp",
        )
        .unwrap();
        db.insert_drawer(
            real_id,
            "real near duplicate",
            &real_vector,
            "wing",
            "room",
            "",
            "mcp",
        )
        .unwrap();

        let duplicate = db
            .find_near_duplicate(&query, "wing", "room", "excluded")
            .unwrap()
            .expect("a qualifying real drawer");
        assert_eq!(duplicate.0, real_id);
        assert!((duplicate.1 - 0.95).abs() < 1e-5);
    }

    #[test]
    fn find_near_duplicate_rejects_zero_norm_and_below_threshold_candidates() {
        let db = Database::open_in_memory().unwrap();
        let query = vec![1.0, 0.0];
        db.insert_drawer(
            "zero000000000000000000000000000000",
            "zero",
            &[0.0, 0.0],
            "wing",
            "room",
            "",
            "test",
        )
        .unwrap();
        db.insert_drawer(
            "low0000000000000000000000000000000",
            "low",
            &[0.9, 1.0],
            "wing",
            "room",
            "",
            "test",
        )
        .unwrap();

        assert!(db
            .find_near_duplicate(&query, "wing", "room", "excluded")
            .unwrap()
            .is_none());
    }

    #[test]
    fn find_near_duplicate_includes_the_exact_threshold() {
        let db = Database::open_in_memory().unwrap();
        let threshold_vector = [
            NEAR_DUPLICATE_THRESHOLD,
            (1.0 - NEAR_DUPLICATE_THRESHOLD.powi(2)).sqrt(),
        ];
        db.insert_drawer(
            "threshold00000000000000000000000000",
            "at threshold",
            &threshold_vector,
            "wing",
            "room",
            "",
            "test",
        )
        .unwrap();

        let duplicate = db
            .find_near_duplicate(&[1.0, 0.0], "wing", "room", "excluded")
            .unwrap()
            .expect("a score exactly at the threshold must be included");
        assert_eq!(duplicate.0, "threshold00000000000000000000000000");
        assert!((duplicate.1 - NEAR_DUPLICATE_THRESHOLD).abs() < 1e-5);
    }
}

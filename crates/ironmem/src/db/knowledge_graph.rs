use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::schema::Database;
use crate::error::MemoryError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Triple {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub confidence: f64,
    pub source_closet: Option<String>,
    pub extracted_at: String,
}

/// Lowercase `s`, turn every character that is neither alphanumeric nor
/// whitespace into a space, and collapse whitespace runs to single spaces.
/// `find_entities_in_text` applies it to both the text and each entity name.
fn normalize_mention(s: &str) -> String {
    let mapped: String = s
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();
    collapse_whitespace(&mapped)
}

/// Trim `s` and collapse each internal whitespace run to a single space.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `needle` occurs in `haystack` with no alphanumeric character
/// directly before or after that occurrence. Every occurrence is tried,
/// overlapping ones included, so an embedded one (the "c++" in "abc++")
/// does not hide a standalone one later in the text.
fn contains_bounded(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(offset) = haystack[from..].find(needle) {
        let start = from + offset;
        let end = start + needle.len();
        let before_ok = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        // Step one character past this occurrence's start, not past its end,
        // so an overlapping occurrence is still found.
        from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn entity_id(name: &str, entity_type: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.to_lowercase().as_bytes());
    hasher.update(entity_type.as_bytes());
    format!("{:x}", hasher.finalize())[..32].to_string()
}

fn triple_interval_id(
    subject: &str,
    predicate: &str,
    object: &str,
    valid_from: Option<&str>,
    extracted_at: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(subject.as_bytes());
    hasher.update(predicate.as_bytes());
    hasher.update(object.as_bytes());
    hasher.update(valid_from.unwrap_or("").as_bytes());
    hasher.update(extracted_at.as_bytes());
    format!("{:x}", hasher.finalize())[..32].to_string()
}

/// Knowledge graph operations on the shared Database.
pub struct KnowledgeGraph<'a> {
    db: &'a Database,
}

impl<'a> KnowledgeGraph<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Add or update an entity.
    pub fn upsert_entity(&self, name: &str, entity_type: &str) -> Result<String, MemoryError> {
        Self::upsert_entity_conn(&self.db.conn, name, entity_type)
    }

    /// Add a triple (relationship between entities).
    #[allow(clippy::too_many_arguments)]
    pub fn add_triple(
        &self,
        subject_name: &str,
        subject_type: &str,
        predicate: &str,
        object_name: &str,
        object_type: &str,
        valid_from: Option<&str>,
        confidence: f64,
        source_closet: Option<&str>,
    ) -> Result<String, MemoryError> {
        Self::add_triple_conn(
            &self.db.conn,
            subject_name,
            subject_type,
            predicate,
            object_name,
            object_type,
            valid_from,
            confidence,
            source_closet,
        )
    }

    /// Invalidate a triple by setting valid_to.
    pub fn invalidate_triple(&self, triple_id: &str, valid_to: &str) -> Result<bool, MemoryError> {
        Self::invalidate_triple_conn(&self.db.conn, triple_id, valid_to)
    }

    /// Query triples for an entity (both as subject and object).
    /// Only returns currently valid triples (valid_to IS NULL).
    ///
    /// Results are capped at `limit` (a well-connected hub would otherwise dump
    /// every triple into MCP responses and the KG-boost loop). The secondary
    /// `id` sort keeps the truncation deterministic when `extracted_at` ties,
    /// so the same `limit` always returns the same prefix.
    pub fn query_entity_current(
        &self,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<Triple>, MemoryError> {
        let mut stmt = self.db.conn.prepare(
            "SELECT id, subject, predicate, object, valid_from, valid_to, confidence, source_closet, extracted_at
             FROM triples
             WHERE (subject = ?1 OR object = ?1) AND valid_to IS NULL
             ORDER BY extracted_at DESC, id ASC
             LIMIT ?2",
        )?;

        // Clamp to i64 so a pathologically large usize can't wrap negative —
        // SQLite reads a negative LIMIT as "unlimited", which would defeat the
        // cap this method exists to enforce.
        let bounded = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![entity_id, bounded], Self::row_to_triple)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// [`Self::query_entity_current`], restricted to a single predicate.
    ///
    /// The `limit` in the unrestricted version is applied *before* the caller
    /// can filter, so an entity whose edges are of several kinds spends one
    /// budget across all of them: enough `has_merge` edges push every
    /// `has_review` edge out of the window, and the caller reads "never
    /// reviewed" rather than erroring. Pushing the predicate into the SQL
    /// gives each kind its own budget, which is the only way the truncation
    /// of one kind cannot silently erase another.
    pub fn query_entity_current_with_predicate(
        &self,
        entity_id: &str,
        predicate: &str,
        limit: usize,
    ) -> Result<Vec<Triple>, MemoryError> {
        let mut stmt = self.db.conn.prepare(
            "SELECT id, subject, predicate, object, valid_from, valid_to, confidence, source_closet, extracted_at
             FROM triples
             WHERE (subject = ?1 OR object = ?1) AND predicate = ?2 AND valid_to IS NULL
             ORDER BY extracted_at DESC, id ASC
             LIMIT ?3",
        )?;

        // Clamped for the same reason as in `query_entity_current`.
        let bounded = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![entity_id, predicate, bounded], Self::row_to_triple)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Get entity timeline — all triples (including invalidated) sorted by valid_from.
    pub fn timeline_for_entity_id(&self, entity_id: &str) -> Result<Vec<Triple>, MemoryError> {
        let mut stmt = self.db.conn.prepare(
            "SELECT t.id, t.subject, t.predicate, t.object, t.valid_from, t.valid_to,
                    t.confidence, t.source_closet, t.extracted_at
             FROM triples t
             WHERE t.subject = ?1 OR t.object = ?1
             ORDER BY COALESCE(t.valid_from, t.extracted_at) ASC",
        )?;

        let rows = stmt.query_map(params![entity_id], Self::row_to_triple)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Find entities whose name appears in the given text.
    /// Used by KG-boosted search to detect entity mentions in queries.
    ///
    /// Text and name both go through [`normalize_mention`], so punctuation
    /// acts as a word boundary: "cat." matches entity "cat", and the entity
    /// "iron-build" matches "use iron-build." as the tokens `iron build`.
    /// A name that itself contains punctuation must also appear verbatim
    /// (case- and whitespace-insensitively) as a whole word, with no letter
    /// or digit directly on either side: without that check "c++" would
    /// match any bare "c" or the tail of "abc++", and "iron-build" would
    /// match "iron build". Both conditions hold for the same occurrence, so
    /// "plan c ... abc++" is not a mention of "c++".
    ///
    /// The comparison runs in Rust rather than SQL because SQLite has no way
    /// to apply the same normalization to `name`, and its `LOWER` folds ASCII
    /// only. A substring filter in SQL could not use an index either, so this
    /// scans no more rows; names are borrowed and only matched rows allocate
    /// an `Entity`.
    pub fn find_entities_in_text(&self, text: &str) -> Result<Vec<Entity>, MemoryError> {
        const MAX_MATCHES: usize = 100;

        let text_tokens = format!(" {} ", normalize_mention(text));
        let text_verbatim = collapse_whitespace(&text.to_lowercase());

        let mut stmt = self
            .db
            .conn
            .prepare("SELECT id, name, entity_type, created_at FROM entities")?;
        let mut rows = stmt.query([])?;

        let mut matches = Vec::new();
        let mut needle = String::new();
        while let Some(row) = rows.next()? {
            let name = row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?;
            let name_tokens = normalize_mention(name);
            // A name of only punctuation/whitespace normalizes to nothing and
            // would otherwise match any text through the bare `"  "` needle.
            if name_tokens.is_empty() {
                continue;
            }
            needle.clear();
            needle.push(' ');
            needle.push_str(&name_tokens);
            needle.push(' ');
            if !text_tokens.contains(needle.as_str()) {
                continue;
            }
            // A punctuated name needs one occurrence that is both verbatim and
            // a whole word. Checking the tokens and the verbatim text apart
            // would let "plan c ... abc++" match "c++": the bare "c" supplies
            // the token and "abc++" the characters. A bounded verbatim
            // occurrence also yields the tokens, so the check above is only a
            // cheap prefilter here.
            let name_verbatim = collapse_whitespace(&name.to_lowercase());
            if name_verbatim != name_tokens && !contains_bounded(&text_verbatim, &name_verbatim) {
                continue;
            }
            matches.push(Entity {
                id: row.get(0)?,
                name: name.to_string(),
                entity_type: row.get(2)?,
                created_at: row.get(3)?,
            });
            if matches.len() >= MAX_MATCHES {
                break;
            }
        }
        Ok(matches)
    }

    pub fn find_entities_by_name(
        &self,
        name: &str,
        entity_type: Option<&str>,
    ) -> Result<Vec<Entity>, MemoryError> {
        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Entity> {
            Ok(Entity {
                id: row.get(0)?,
                name: row.get(1)?,
                entity_type: row.get(2)?,
                created_at: row.get(3)?,
            })
        };

        let mut entities = Vec::new();

        match entity_type {
            Some(kind) => {
                let mut stmt = self.db.conn.prepare(
                    "SELECT id, name, entity_type, created_at
                     FROM entities
                     WHERE LOWER(name) = LOWER(?1) AND entity_type = ?2
                     ORDER BY created_at ASC",
                )?;
                let rows = stmt.query_map(params![name, kind], map_row)?;
                for row in rows {
                    entities.push(row?);
                }
            }
            None => {
                let mut stmt = self.db.conn.prepare(
                    "SELECT id, name, entity_type, created_at
                     FROM entities
                     WHERE LOWER(name) = LOWER(?1)
                     ORDER BY entity_type ASC, created_at ASC",
                )?;
                let rows = stmt.query_map(params![name], map_row)?;
                for row in rows {
                    entities.push(row?);
                }
            }
        }

        Ok(entities)
    }

    pub fn resolve_entity(
        &self,
        name: &str,
        entity_type: Option<&str>,
    ) -> Result<Entity, MemoryError> {
        let matches = self.find_entities_by_name(name, entity_type)?;
        match matches.len() {
            0 => {
                let type_suffix = entity_type
                    .map(|kind| format!(" ({kind})"))
                    .unwrap_or_default();
                Err(MemoryError::NotFound(format!(
                    "No entity found for {name}{type_suffix}"
                )))
            }
            1 => Ok(matches.into_iter().next().ok_or_else(|| {
                MemoryError::NotFound(format!("Entity '{name}' disappeared during lookup"))
            })?),
            _ => {
                let choices = matches
                    .iter()
                    .map(|entity| format!("{}:{}", entity.name, entity.entity_type))
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(MemoryError::Validation(format!(
                    "Entity '{name}' is ambiguous; specify entity_type. Matches: {choices}"
                )))
            }
        }
    }

    /// Get an entity by ID.
    pub fn get_entity(&self, entity_id: &str) -> Result<Option<Entity>, MemoryError> {
        let mut stmt = self
            .db
            .conn
            .prepare("SELECT id, name, entity_type, created_at FROM entities WHERE id = ?1")?;

        let mut rows = stmt.query_map(params![entity_id], |row| {
            Ok(Entity {
                id: row.get(0)?,
                name: row.get(1)?,
                entity_type: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;

        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Stats: entity count, triple count, current triple count.
    pub fn stats(&self) -> Result<serde_json::Value, MemoryError> {
        let entity_count: i64 =
            self.db
                .conn
                .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))?;
        let triple_count: i64 =
            self.db
                .conn
                .query_row("SELECT COUNT(*) FROM triples", [], |r| r.get(0))?;
        let current_count: i64 = self.db.conn.query_row(
            "SELECT COUNT(*) FROM triples WHERE valid_to IS NULL",
            [],
            |r| r.get(0),
        )?;

        Ok(serde_json::json!({
            "entity_count": entity_count,
            "triple_count": triple_count,
            "current_triple_count": current_count,
        }))
    }

    fn row_to_triple(row: &rusqlite::Row<'_>) -> rusqlite::Result<Triple> {
        Ok(Triple {
            id: row.get(0)?,
            subject: row.get(1)?,
            predicate: row.get(2)?,
            object: row.get(3)?,
            valid_from: row.get(4)?,
            valid_to: row.get(5)?,
            confidence: row.get(6)?,
            source_closet: row.get(7)?,
            extracted_at: row.get(8)?,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_triple_tx(
        tx: &Transaction<'_>,
        subject_name: &str,
        subject_type: &str,
        predicate: &str,
        object_name: &str,
        object_type: &str,
        valid_from: Option<&str>,
        confidence: f64,
        source_closet: Option<&str>,
    ) -> Result<String, MemoryError> {
        Self::add_triple_conn(
            tx,
            subject_name,
            subject_type,
            predicate,
            object_name,
            object_type,
            valid_from,
            confidence,
            source_closet,
        )
    }

    pub(crate) fn invalidate_triple_tx(
        tx: &Transaction<'_>,
        triple_id: &str,
        valid_to: &str,
    ) -> Result<bool, MemoryError> {
        Self::invalidate_triple_conn(tx, triple_id, valid_to)
    }

    fn upsert_entity_conn(
        conn: &Connection,
        name: &str,
        entity_type: &str,
    ) -> Result<String, MemoryError> {
        let id = entity_id(name, entity_type);
        conn.execute(
            "INSERT INTO entities (id, name, entity_type)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO NOTHING",
            params![id, name, entity_type],
        )?;
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)]
    fn add_triple_conn(
        conn: &Connection,
        subject_name: &str,
        subject_type: &str,
        predicate: &str,
        object_name: &str,
        object_type: &str,
        valid_from: Option<&str>,
        confidence: f64,
        source_closet: Option<&str>,
    ) -> Result<String, MemoryError> {
        let subject_id = Self::upsert_entity_conn(conn, subject_name, subject_type)?;
        let object_id = Self::upsert_entity_conn(conn, object_name, object_type)?;

        if let Some(existing_id) = conn
            .query_row(
                "SELECT id FROM triples
                 WHERE subject = ?1 AND predicate = ?2 AND object = ?3 AND valid_to IS NULL
                 LIMIT 1",
                params![&subject_id, predicate, &object_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            conn.execute(
                "UPDATE triples
                 SET valid_from = COALESCE(?1, valid_from),
                     confidence = ?2,
                     source_closet = ?3
                 WHERE id = ?4",
                params![valid_from, confidence, source_closet, &existing_id],
            )?;
            return Ok(existing_id);
        }

        let extracted_at = chrono::Utc::now().to_rfc3339();
        let id = triple_interval_id(
            &subject_id,
            predicate,
            &object_id,
            valid_from,
            &extracted_at,
        );

        conn.execute(
            "INSERT INTO triples
             (id, subject, predicate, object, valid_from, confidence, source_closet, extracted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                &id,
                &subject_id,
                predicate,
                &object_id,
                valid_from,
                confidence,
                source_closet,
                extracted_at
            ],
        )?;

        Ok(id)
    }

    fn invalidate_triple_conn(
        conn: &Connection,
        triple_id: &str,
        valid_to: &str,
    ) -> Result<bool, MemoryError> {
        let count = conn.execute(
            "UPDATE triples SET valid_to = ?1 WHERE id = ?2 AND valid_to IS NULL",
            params![valid_to, triple_id],
        )?;
        Ok(count > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::Database;

    #[test]
    fn test_upsert_entity_idempotent() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        let id1 = kg.upsert_entity("Alice", "person").unwrap();
        let id2 = kg.upsert_entity("Alice", "person").unwrap();

        assert_eq!(id1, id2); // Same name+type → same ID
        assert_eq!(id1.len(), 32);

        let entity = kg.get_entity(&id1).unwrap().unwrap();
        assert_eq!(entity.name, "Alice");
        assert_eq!(entity.entity_type, "person");
    }

    #[test]
    fn test_entity_id_varies_by_type() {
        let id_person = entity_id("Alice", "person");
        let id_org = entity_id("Alice", "organization");
        assert_ne!(id_person, id_org);
    }

    #[test]
    fn test_add_and_query_triple() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        let triple_id = kg
            .add_triple(
                "Alice", "person", "works_at", "Acme", "company", None, 0.9, None,
            )
            .unwrap();
        assert_eq!(triple_id.len(), 32);

        // Query from subject side
        let alice_id = entity_id("alice", "person");
        let triples = kg.query_entity_current(&alice_id, 50).unwrap();
        assert_eq!(triples.len(), 1);
        assert_eq!(triples[0].predicate, "works_at");
        assert!((triples[0].confidence - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn test_query_entity_current_truncates_and_orders_stably() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        // Seed >50 current triples on a single hub entity.
        for i in 0..60 {
            kg.add_triple(
                "Hub",
                "person",
                "knows",
                &format!("Friend{i:02}"),
                "person",
                None,
                1.0,
                None,
            )
            .unwrap();
        }

        let hub_id = entity_id("hub", "person");

        // Explicit limit truncates the result set.
        let limited = kg.query_entity_current(&hub_id, 50).unwrap();
        assert_eq!(limited.len(), 50);

        let smaller = kg.query_entity_current(&hub_id, 10).unwrap();
        assert_eq!(smaller.len(), 10);

        // Stable ordering: same limit → identical id sequence across calls,
        // even when extracted_at ties (tiebreaker on id keeps it deterministic).
        let again = kg.query_entity_current(&hub_id, 50).unwrap();
        let ids_a: Vec<&str> = limited.iter().map(|t| t.id.as_str()).collect();
        let ids_b: Vec<&str> = again.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids_a, ids_b);

        // The truncated set is a prefix of the larger set (consistent order).
        let smaller_ids: Vec<&str> = smaller.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(smaller_ids, ids_a[..10]);
    }

    #[test]
    fn a_predicate_scoped_query_cannot_be_starved_by_a_busier_predicate() {
        // The failure this method exists to make unreachable: one entity's
        // edges are of several kinds, the unrestricted query spends a single
        // budget across all of them newest-first, and enough edges of the
        // busy kind push every edge of the quiet kind out of the window. The
        // caller then reads an empty result and concludes the quiet fact
        // never happened — a wrong answer, not an error.
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        // The quiet kind first, so it is the *oldest* and therefore the
        // first to be truncated away.
        kg.add_triple(
            "Hub",
            "issue",
            "has_review",
            "R1",
            "review",
            None,
            1.0,
            None,
        )
        .unwrap();
        for i in 0..20 {
            kg.add_triple(
                "Hub",
                "issue",
                "has_merge",
                &format!("M{i:02}"),
                "merge",
                None,
                1.0,
                None,
            )
            .unwrap();
        }
        let hub_id = entity_id("hub", "issue");

        // The unrestricted query, with a budget smaller than the busy kind:
        // the review is gone.
        let mixed = kg.query_entity_current(&hub_id, 10).unwrap();
        assert!(
            !mixed.iter().any(|t| t.predicate == "has_review"),
            "the starvation this test is about must actually happen"
        );

        // The scoped query, at the same budget: the review is still there.
        let reviews = kg
            .query_entity_current_with_predicate(&hub_id, "has_review", 10)
            .unwrap();
        assert_eq!(reviews.len(), 1);
        assert!(reviews.iter().all(|t| t.predicate == "has_review"));

        // And the busy kind still gets its own full budget.
        let merges = kg
            .query_entity_current_with_predicate(&hub_id, "has_merge", 10)
            .unwrap();
        assert_eq!(merges.len(), 10);
        assert!(merges.iter().all(|t| t.predicate == "has_merge"));
    }

    #[test]
    fn test_query_entity_current_tiebreaks_on_id_when_extracted_at_ties() {
        let db = Database::open_in_memory().unwrap();

        // Insert triples directly with an IDENTICAL extracted_at so the
        // secondary `id ASC` sort is the only thing that can order them.
        // Ids are inserted out of sorted order; without the tiebreaker the
        // result would follow rowid (insertion) order and this test fails.
        let hub = "hub00000000000000000000000000000";
        db.conn
            .execute(
                "INSERT INTO entities (id, name, entity_type) VALUES (?1, 'Hub', 'thing')",
                params![hub],
            )
            .unwrap();

        // Triple ids inserted out of sorted order; each needs a distinct object
        // (UNIQUE on subject,predicate,object) and a shared extracted_at so the
        // `id ASC` tiebreaker is the only thing that can order the result.
        let ids = [
            "cccccccccccccccccccccccccccccccc",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "dddddddddddddddddddddddddddddddd",
        ];
        for (i, id) in ids.iter().enumerate() {
            let obj = format!("obj{i:029}");
            db.conn
                .execute(
                    "INSERT INTO entities (id, name, entity_type) VALUES (?1, ?2, 'thing')",
                    params![obj, format!("Obj{i}")],
                )
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO triples
                     (id, subject, predicate, object, confidence, extracted_at)
                     VALUES (?1, ?2, 'rel', ?3, 1.0, '2026-01-01T00:00:00Z')",
                    params![id, hub, obj],
                )
                .unwrap();
        }

        let kg = KnowledgeGraph::new(&db);
        let got = kg.query_entity_current(hub, 50).unwrap();
        let got_ids: Vec<&str> = got.iter().map(|t| t.id.as_str()).collect();

        let mut expected: Vec<&str> = ids.to_vec();
        expected.sort_unstable();
        assert_eq!(got_ids, expected);
    }

    #[test]
    fn test_invalidate_triple() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        let triple_id = kg
            .add_triple(
                "Alice", "person", "works_at", "Acme", "company", None, 1.0, None,
            )
            .unwrap();

        // Triple is visible before invalidation
        let alice_id = entity_id("alice", "person");
        assert_eq!(kg.query_entity_current(&alice_id, 50).unwrap().len(), 1);

        // Invalidate
        assert!(kg.invalidate_triple(&triple_id, "2025-01-01").unwrap());

        // No longer visible in current query
        assert_eq!(kg.query_entity_current(&alice_id, 50).unwrap().len(), 0);

        // Double invalidation returns false
        assert!(!kg.invalidate_triple(&triple_id, "2025-01-02").unwrap());
    }

    #[test]
    fn test_readding_invalidated_triple_creates_new_interval() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        let first_id = kg
            .add_triple(
                "Alice",
                "person",
                "works_at",
                "Acme",
                "company",
                Some("2020-01-01"),
                1.0,
                None,
            )
            .unwrap();
        assert!(kg.invalidate_triple(&first_id, "2021-01-01").unwrap());

        let second_id = kg
            .add_triple(
                "Alice",
                "person",
                "works_at",
                "Acme",
                "company",
                Some("2022-01-01"),
                1.0,
                None,
            )
            .unwrap();

        assert_ne!(first_id, second_id);

        let alice_id = entity_id("alice", "person");
        let current = kg.query_entity_current(&alice_id, 50).unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].id, second_id);

        let timeline = kg.timeline_for_entity_id(&alice_id).unwrap();
        assert_eq!(timeline.len(), 2);
        assert!(timeline.iter().any(
            |triple| triple.id == first_id && triple.valid_to.as_deref() == Some("2021-01-01")
        ));
        assert!(timeline
            .iter()
            .any(|triple| triple.id == second_id && triple.valid_to.is_none()));
    }

    #[test]
    fn test_timeline_includes_invalidated() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        kg.add_triple(
            "Alice",
            "person",
            "works_at",
            "Acme",
            "company",
            Some("2020-01-01"),
            1.0,
            None,
        )
        .unwrap();
        let t2 = kg
            .add_triple(
                "Alice",
                "person",
                "works_at",
                "BigCo",
                "company",
                Some("2023-01-01"),
                1.0,
                None,
            )
            .unwrap();

        // Invalidate the second one
        kg.invalidate_triple(&t2, "2024-06-01").unwrap();

        let alice_id = entity_id("alice", "person");
        let timeline = kg.timeline_for_entity_id(&alice_id).unwrap();
        assert_eq!(timeline.len(), 2); // Both visible in timeline
    }

    #[test]
    fn test_resolve_entity_requires_type_when_name_is_ambiguous() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        kg.upsert_entity("Apple", "company").unwrap();
        kg.upsert_entity("Apple", "person").unwrap();

        let err = kg.resolve_entity("Apple", None).unwrap_err().to_string();
        assert!(err.contains("ambiguous"));

        let company = kg.resolve_entity("Apple", Some("company")).unwrap();
        assert_eq!(company.entity_type, "company");
    }

    #[test]
    fn test_timeline_for_entity_id_does_not_merge_same_name_different_types() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        kg.add_triple(
            "Apple",
            "company",
            "ships",
            "iPhone",
            "product",
            Some("2024-01-01"),
            1.0,
            None,
        )
        .unwrap();
        kg.add_triple(
            "Apple",
            "person",
            "knows",
            "Bob",
            "person",
            Some("2024-02-01"),
            1.0,
            None,
        )
        .unwrap();

        let company = kg.resolve_entity("Apple", Some("company")).unwrap();
        let person = kg.resolve_entity("Apple", Some("person")).unwrap();

        let company_timeline = kg.timeline_for_entity_id(&company.id).unwrap();
        let person_timeline = kg.timeline_for_entity_id(&person.id).unwrap();

        assert_eq!(company_timeline.len(), 1);
        assert_eq!(person_timeline.len(), 1);
        assert_eq!(company_timeline[0].predicate, "ships");
        assert_eq!(person_timeline[0].predicate, "knows");
    }

    #[test]
    fn test_find_entities_word_boundary() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        kg.upsert_entity("cat", "animal").unwrap();
        kg.upsert_entity("scatter", "action").unwrap();

        // "cat" should match "I have a cat" but NOT "scatter"
        let matches = kg.find_entities_in_text("I have a cat").unwrap();
        let names: Vec<&str> = matches.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"cat"));
        assert!(!names.contains(&"scatter"));
    }

    #[test]
    fn test_find_entities_with_punctuation() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        kg.upsert_entity("cat", "animal").unwrap();

        // Punctuation adjacent to entity name should still match
        let cases = vec![
            "I saw a cat.",
            "the cat, the dog",
            "what about (cat)?",
            "cat!",
            "is it a cat;",
            "cat: a feline",
        ];

        for query in cases {
            let matches = kg.find_entities_in_text(query).unwrap();
            assert!(
                matches.iter().any(|e| e.name == "cat"),
                "Expected 'cat' to match in: {query}"
            );
        }
    }

    #[test]
    fn test_find_entities_with_punctuation_in_name() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        kg.upsert_entity("iron-build", "tool").unwrap();
        kg.upsert_entity("kg_boost", "function").unwrap();
        kg.upsert_entity("Node.js", "runtime").unwrap();

        // Each entity is named in a query, including next to the punctuation
        // the query normalization strips (a trailing period, a comma).
        let cases = [
            ("how does iron-build dispatch tasks?", "iron-build"),
            ("is kg_boost capped, or not", "kg_boost"),
            ("we run on node.js.", "Node.js"),
        ];

        for (query, expected) in cases {
            let matches = kg.find_entities_in_text(query).unwrap();
            assert!(
                matches.iter().any(|e| e.name == expected),
                "Expected '{expected}' to match in: {query}, got {matches:?}"
            );
        }
    }

    #[test]
    fn test_find_entities_punctuated_name_needs_exact_mention() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        kg.upsert_entity("c++", "language").unwrap();
        kg.upsert_entity("iron-build", "tool").unwrap();

        // Normalizing "c++" leaves the single token "c", so a bare "c" in the
        // query must not be read as a mention; nor may a longer word ending
        // in the punctuated name, or the words without their joining hyphen.
        // The token and the verbatim text must also be the same occurrence:
        // in the combined query the bare "c" supplies the token and "abc++"
        // the characters, but no single occurrence is a mention of "c++".
        for query in [
            "plan c is the fallback",
            "abc++ compiles",
            "iron build",
            "plan c is the fallback; abc++ compiles",
            "iron build or xiron-build",
        ] {
            let matches = kg.find_entities_in_text(query).unwrap();
            assert!(
                matches.is_empty(),
                "Expected no match in: {query}, got {matches:?}"
            );
        }

        // A standalone mention still matches, including after an embedded
        // one, at the very start or end of the text, and next to punctuation.
        for query in [
            "I write C++ daily",
            "abc++ is not c++",
            "c++",
            "(c++), mostly",
        ] {
            let matches = kg.find_entities_in_text(query).unwrap();
            assert!(
                matches.iter().any(|e| e.name == "c++"),
                "Expected 'c++' to match in: {query}, got {matches:?}"
            );
        }
    }

    #[test]
    fn test_contains_bounded() {
        assert!(contains_bounded("i write c++ daily", "c++"));
        assert!(contains_bounded("c++", "c++"));
        assert!(contains_bounded("use iron-build.", "iron-build"));
        assert!(!contains_bounded("abc++ compiles", "c++"));
        assert!(!contains_bounded("xiron-buildy", "iron-build"));
        assert!(!contains_bounded("anything", ""));
        // A later, separate occurrence is found after an embedded one.
        assert!(contains_bounded("xa.a a.a", "a.a"));
        // "a.a" occurs at 1 (after "x") and, overlapping it, at 3 (after
        // "."); only the second is bounded. A non-overlapping scan resumes
        // at 4 and would miss it.
        assert!(contains_bounded("xa.a.a", "a.a"));
        // Multibyte characters around and inside the needle.
        assert!(contains_bounded("é c++ é", "c++"));
        assert!(!contains_bounded("éc++", "c++"));
        assert!(contains_bounded("ünï-côde.", "ünï-côde"));
    }

    #[test]
    fn test_find_entities_skips_name_of_only_punctuation() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        // "---" normalizes to the empty string; its needle would be a bare
        // pair of spaces, which an empty query's padded text contains.
        kg.upsert_entity("---", "separator").unwrap();

        for query in ["", "---", "hello world"] {
            let matches = kg.find_entities_in_text(query).unwrap();
            assert!(
                matches.is_empty(),
                "Expected no match in: {query:?}, got {matches:?}"
            );
        }
    }

    #[test]
    fn test_find_entities_no_match() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        kg.upsert_entity("Alice", "person").unwrap();

        let matches = kg.find_entities_in_text("Bob went to the store").unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn test_get_entity_missing() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        assert!(kg.get_entity("nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_stats() {
        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        let stats = kg.stats().unwrap();
        assert_eq!(stats["entity_count"], 0);
        assert_eq!(stats["triple_count"], 0);

        kg.add_triple("A", "t", "rel", "B", "t", None, 1.0, None)
            .unwrap();

        let stats = kg.stats().unwrap();
        assert_eq!(stats["entity_count"], 2);
        assert_eq!(stats["triple_count"], 1);
        assert_eq!(stats["current_triple_count"], 1);
    }

    #[test]
    fn test_kg_insert_rolls_back_if_wal_fails() {
        let db = Database::open_in_memory().unwrap();

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
                let triple_id = KnowledgeGraph::add_triple_tx(
                    tx,
                    "Alice",
                    "person",
                    "works_at",
                    "Acme",
                    "company",
                    Some("2024-01-01"),
                    1.0,
                    None,
                )?;
                Database::wal_log_tx(
                    tx,
                    "fail",
                    &serde_json::json!({"triple_id": triple_id}),
                    None,
                )?;
                Ok(())
            })
            .unwrap_err();

        assert!(err.to_string().contains("forced wal failure"));
        let triple_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM triples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(triple_count, 0);
    }
}

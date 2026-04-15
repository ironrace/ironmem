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
    pub fn query_entity_current(&self, entity_id: &str) -> Result<Vec<Triple>, MemoryError> {
        let mut stmt = self.db.conn.prepare(
            "SELECT id, subject, predicate, object, valid_from, valid_to, confidence, source_closet, extracted_at
             FROM triples
             WHERE (subject = ?1 OR object = ?1) AND valid_to IS NULL
             ORDER BY extracted_at DESC",
        )?;

        let rows = stmt.query_map(params![entity_id], Self::row_to_triple)?;
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
    /// Normalizes punctuation to spaces so "cat." matches entity "cat".
    pub fn find_entities_in_text(&self, text: &str) -> Result<Vec<Entity>, MemoryError> {
        // Normalize: lowercase, replace non-alphanumeric with spaces, collapse, pad
        let normalized: String = text
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
        let text_padded = format!(
            " {} ",
            normalized.split_whitespace().collect::<Vec<_>>().join(" ")
        );

        let mut stmt = self.db.conn.prepare(
            "SELECT id, name, entity_type, created_at
             FROM entities
             WHERE INSTR(?1, ' ' || LOWER(name) || ' ') > 0
             LIMIT 100",
        )?;

        let rows = stmt.query_map(params![text_padded], |row| {
            Ok(Entity {
                id: row.get(0)?,
                name: row.get(1)?,
                entity_type: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;

        let mut matches = Vec::new();
        for row in rows {
            matches.push(row?);
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
        let triples = kg.query_entity_current(&alice_id).unwrap();
        assert_eq!(triples.len(), 1);
        assert_eq!(triples[0].predicate, "works_at");
        assert!((triples[0].confidence - 0.9).abs() < f64::EPSILON);
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
        assert_eq!(kg.query_entity_current(&alice_id).unwrap().len(), 1);

        // Invalidate
        assert!(kg.invalidate_triple(&triple_id, "2025-01-01").unwrap());

        // No longer visible in current query
        assert_eq!(kg.query_entity_current(&alice_id).unwrap().len(), 0);

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
        let current = kg.query_entity_current(&alice_id).unwrap();
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

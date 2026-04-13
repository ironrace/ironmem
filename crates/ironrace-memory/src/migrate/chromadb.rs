//! Migrate from a ChromaDB store to ironrace-memory's SQLite format.
//!
//! Strategy:
//! 1. Open ChromaDB's SQLite backing store
//! 2. Read all documents + metadata
//! 3. Re-embed with our ONNX pipeline (for tokenizer consistency)
//! 4. Insert into our schema
//! 5. Copy knowledge_graph.sqlite3 tables if present

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

use crate::db::knowledge_graph::KnowledgeGraph;
use crate::error::MemoryError;
use crate::mcp::app::App;

struct MempalaceDrawer {
    embedding_id: String,
    content: String,
    wing: String,
    room: String,
    source_file: String,
    added_by: String,
}

/// Directories that must never be opened as a migration source.
const BLOCKED_MIGRATE_PREFIXES: &[&str] = &[
    "/etc", "/usr", "/var", "/sys", "/proc", "/dev", "/boot", "/run", "/bin", "/sbin",
];

/// Migrate from ChromaDB store to ironrace-memory.
pub fn migrate_from_chromadb(chromadb_path: &str, app: &App) -> Result<(), MemoryError> {
    let path = Path::new(chromadb_path);

    if !path.exists() {
        return Err(MemoryError::Migration(format!(
            "ChromaDB store not found at: {}",
            chromadb_path
        )));
    }

    // Canonicalize to resolve symlinks before checking system boundaries.
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let canonical_str = canonical.to_string_lossy();
    if BLOCKED_MIGRATE_PREFIXES.iter().any(|prefix| {
        canonical_str == *prefix || canonical_str.starts_with(&format!("{}/", prefix))
    }) {
        return Err(MemoryError::Validation(format!(
            "Migration source '{canonical_str}' is a system directory and cannot be used"
        )));
    }

    tracing::info!("Migrating from ChromaDB store at: {}", chromadb_path);
    let chroma_db = path.join("chroma.sqlite3");
    if !chroma_db.is_file() {
        return Err(MemoryError::Migration(format!(
            "Expected ChromaDB sqlite at {}",
            chroma_db.display()
        )));
    }

    let source = Connection::open(&chroma_db)?;
    let drawers = load_drawers_from_chromadb(&source)?;
    if !drawers.is_empty() {
        let texts: Vec<&str> = drawers
            .iter()
            .map(|drawer| drawer.content.as_str())
            .collect();
        let embeddings = {
            let mut embedder = app
                .embedder
                .write()
                .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;
            embedder.embed_batch(&texts).map_err(MemoryError::Embed)?
        };

        app.db.with_transaction(|tx| {
            for (index, drawer) in drawers.iter().enumerate() {
                let start = index * ironrace_embed::embedder::EMBED_DIM;
                let end = start + ironrace_embed::embedder::EMBED_DIM;
                let embedding = embeddings.get(start..end).ok_or_else(|| {
                    MemoryError::Migration(
                        "Embedding batch length mismatch during migration".into(),
                    )
                })?;
                crate::db::schema::Database::insert_drawer_tx(
                    tx,
                    &drawer.embedding_id,
                    &drawer.content,
                    embedding,
                    &drawer.wing,
                    &drawer.room,
                    &drawer.source_file,
                    &drawer.added_by,
                )?;
            }
            Ok(())
        })?;
    }

    migrate_knowledge_graph(path, app)?;
    app.mark_dirty();
    Ok(())
}

fn load_drawers_from_chromadb(source: &Connection) -> Result<Vec<MempalaceDrawer>, MemoryError> {
    let mut stmt = source.prepare(
        "SELECT
            e.embedding_id,
            COALESCE(MAX(CASE WHEN m.key = 'chroma:document' THEN m.string_value END), '') AS content,
            COALESCE(MAX(CASE WHEN m.key = 'wing' THEN m.string_value END), 'memory') AS wing,
            COALESCE(MAX(CASE WHEN m.key = 'room' THEN m.string_value END), 'general') AS room,
            COALESCE(MAX(CASE WHEN m.key = 'source_file' THEN m.string_value END), '') AS source_file,
            COALESCE(MAX(CASE WHEN m.key = 'added_by' THEN m.string_value END), 'mempalace') AS added_by
         FROM embeddings e
         JOIN embedding_metadata m ON m.id = e.id
         GROUP BY e.id
         ORDER BY e.id",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(MempalaceDrawer {
            embedding_id: row.get(0)?,
            content: row.get(1)?,
            wing: row.get(2)?,
            room: row.get(3)?,
            source_file: row.get(4)?,
            added_by: row.get(5)?,
        })
    })?;

    let mut result = Vec::new();
    for row in rows {
        let drawer = row?;
        if drawer.content.trim().is_empty() {
            continue;
        }
        result.push(drawer);
    }
    Ok(result)
}

type EntityRow = (String, String, String, String);
type TripleRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    f64,
    Option<String>,
);

fn migrate_knowledge_graph(root: &Path, app: &App) -> Result<(), MemoryError> {
    let kg_path = detect_knowledge_graph_path(root);
    let Some(kg_path) = kg_path else {
        return Ok(());
    };
    let source = Connection::open(kg_path)?;

    // Collect all rows eagerly from the source before opening our transaction.
    let entities = collect_kg_entities(&source)?;
    let triples = collect_kg_triples(&source)?;

    if entities.is_empty() && triples.is_empty() {
        return Ok(());
    }

    let extracted_at = chrono::Utc::now().to_rfc3339();

    // Write everything in a single transaction so a crash mid-migration does
    // not leave partially imported entities without their corresponding triples.
    app.db.with_transaction(|tx| {
        for (id, name, entity_type, properties) in &entities {
            tx.execute(
                "INSERT INTO entities (id, name, entity_type, properties)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                    name = excluded.name,
                    entity_type = excluded.entity_type,
                    properties = excluded.properties",
                params![id, name, entity_type, properties],
            )?;
        }
        for (id, subject, predicate, object, valid_from, valid_to, confidence, source_closet) in
            &triples
        {
            tx.execute(
                "INSERT INTO triples
                 (id, subject, predicate, object, valid_from, valid_to, confidence, source_closet, extracted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET
                    valid_from = excluded.valid_from,
                    valid_to = excluded.valid_to,
                    confidence = excluded.confidence,
                    source_closet = excluded.source_closet",
                params![
                    id,
                    subject,
                    predicate,
                    object,
                    valid_from,
                    valid_to,
                    confidence,
                    source_closet,
                    &extracted_at,
                ],
            )?;
        }
        Ok(())
    })?;

    let _ = KnowledgeGraph::new(&app.db).stats()?;
    Ok(())
}

fn collect_kg_entities(source: &Connection) -> Result<Vec<EntityRow>, MemoryError> {
    let mut stmt =
        source.prepare("SELECT id, name, type, COALESCE(properties, '{}') FROM entities")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(MemoryError::Db)
}

fn collect_kg_triples(source: &Connection) -> Result<Vec<TripleRow>, MemoryError> {
    let mut stmt = source.prepare(
        "SELECT id, subject, predicate, object, valid_from, valid_to,
                COALESCE(confidence, 1.0), source_closet
         FROM triples",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, f64>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(MemoryError::Db)
}

fn detect_knowledge_graph_path(root: &Path) -> Option<PathBuf> {
    let local = root.join("knowledge_graph.sqlite3");
    if local.is_file() {
        return Some(local);
    }
    dirs::home_dir()
        .map(|home| home.join(".mempalace").join("knowledge_graph.sqlite3"))
        .filter(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_drawers_from_chroma_style_tables() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE embeddings (
                id INTEGER PRIMARY KEY,
                segment_id TEXT NOT NULL,
                embedding_id TEXT NOT NULL,
                seq_id BLOB NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE embedding_metadata (
                id INTEGER,
                key TEXT NOT NULL,
                string_value TEXT,
                int_value INTEGER,
                float_value REAL,
                bool_value INTEGER
            );
            ",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO embeddings (id, segment_id, embedding_id, seq_id, created_at)
             VALUES (1, 'seg', 'drawer_1', X'01', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        for (key, value) in [
            ("chroma:document", "hello world"),
            ("wing", "workspace"),
            ("room", "docs"),
            ("source_file", "/tmp/README.md"),
            ("added_by", "mempalace"),
        ] {
            conn.execute(
                "INSERT INTO embedding_metadata (id, key, string_value) VALUES (1, ?1, ?2)",
                params![key, value],
            )
            .unwrap();
        }

        let drawers = load_drawers_from_chromadb(&conn).unwrap();
        assert_eq!(drawers.len(), 1);
        assert_eq!(drawers[0].embedding_id, "drawer_1");
        assert_eq!(drawers[0].content, "hello world");
        assert_eq!(drawers[0].wing, "workspace");
    }
}

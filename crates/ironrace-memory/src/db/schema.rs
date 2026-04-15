use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, Transaction};

use crate::error::MemoryError;
use ironrace_embed::embedder::EMBED_DIM;

const SCHEMA_SQL: &str = include_str!("../../../../migrations/001_init.sql");

/// Database wrapper around a SQLite connection.
///
/// `conn` is intentionally restricted to `pub(super)` (visible only within
/// `crate::db`). External callers must go through the `Database` API so that
/// all access is auditable and the single-threaded invariant is enforced at the
/// boundary rather than scattered across the codebase.
pub struct Database {
    pub(super) conn: Connection,
}

impl Database {
    /// Open (or create) the database at the given path.
    pub fn open(path: &Path) -> Result<Self, MemoryError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        retry_on_busy(|| conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;"))?;

        // Restrict database file permissions to owner-only
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }

        Ok(Self { conn })
    }

    /// Open an in-memory database (for testing and integration tests).
    pub fn open_in_memory() -> Result<Self, MemoryError> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Run schema migrations.
    pub fn migrate(&self) -> Result<(), MemoryError> {
        retry_on_busy(|| self.conn.execute_batch(SCHEMA_SQL))?;
        Ok(())
    }

    /// Execute a closure inside a SQLite transaction and commit on success.
    pub fn with_transaction<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, MemoryError>,
    ) -> Result<T, MemoryError> {
        let tx = self.conn.unchecked_transaction()?;
        let result = f(&tx)?;
        tx.commit()?;
        Ok(result)
    }

    /// Load all vectors from the drawers table for HNSW index building.
    /// Returns (id, embedding) pairs.
    pub fn load_all_vectors(&self) -> Result<Vec<(String, Vec<f32>)>, MemoryError> {
        let mut stmt = self.conn.prepare("SELECT id, embedding FROM drawers")?;

        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            if !blob.len().is_multiple_of(std::mem::size_of::<f32>()) {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    blob.len(),
                    rusqlite::types::Type::Blob,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "Drawer {id} has invalid embedding blob length {}",
                            blob.len()
                        ),
                    )),
                ));
            }
            let embedding: Vec<f32> = blob
                .chunks_exact(4)
                .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect();
            if embedding.len() != EMBED_DIM {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    embedding.len(),
                    rusqlite::types::Type::Blob,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "Drawer {id} embedding dimension {} does not match expected {}",
                            embedding.len(),
                            EMBED_DIM
                        ),
                    )),
                ));
            }
            Ok((id, embedding))
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }
}

fn retry_on_busy<T>(
    mut operation: impl FnMut() -> Result<T, rusqlite::Error>,
) -> Result<T, rusqlite::Error> {
    let start = std::time::Instant::now();
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if is_busy_error(&error) && start.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_busy_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked,
                ..
            },
            _
        )
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::Database;

    /// Returns a `(TempDir, PathBuf)` pair for a database nested under a temp directory.
    /// The caller **must** retain the `TempDir` for the lifetime of the test; dropping it
    /// deletes the directory and invalidates the path.
    fn nested_db_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("sub").join("test.db");
        (dir, db_path)
    }

    #[test]
    fn test_open_creates_parent_dirs_and_migrate_creates_schema() {
        let (_dir, db_path) = nested_db_path();
        let db = Database::open(&db_path).unwrap();
        db.migrate().unwrap();

        // Verify the drawers table was created.
        let count: i64 = db
            .conn
            .query_row("SELECT count(*) FROM drawers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // Calling migrate() a second time must be idempotent (all DDL uses IF NOT EXISTS).
        db.migrate().unwrap();
    }

    #[test]
    fn test_with_transaction_commits_on_success() {
        let db = Database::open_in_memory().unwrap();

        db.with_transaction(|tx| {
            tx.execute(
                "INSERT INTO drawers (id, content, embedding, wing, room, source_file, added_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "txncommit00000000000000000000001",
                    "committed",
                    vec![0u8; ironrace_embed::embedder::EMBED_DIM * std::mem::size_of::<f32>()],
                    "w",
                    "r",
                    "",
                    "test"
                ],
            )?;
            Ok(())
        })
        .unwrap();

        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM drawers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_with_transaction_rolls_back_on_error() {
        use crate::error::MemoryError;

        let db = Database::open_in_memory().unwrap();

        let result: Result<(), _> = db.with_transaction(|tx| {
            let rows_inserted = tx.execute(
                "INSERT INTO drawers (id, content, embedding, wing, room, source_file, added_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "rollback_test_id_000000000000001",
                    "test content",
                    vec![0u8; 4 * ironrace_embed::embedder::EMBED_DIM],
                    "wing",
                    "room",
                    "",
                    "test"
                ],
            )?;
            assert_eq!(
                rows_inserted, 1,
                "INSERT must succeed before testing rollback"
            );
            Err(MemoryError::Validation("force rollback".into()))
        });

        assert!(result.is_err());

        let count: i64 = db
            .conn
            .query_row("SELECT count(*) FROM drawers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "transaction must have been rolled back");
    }

    #[test]
    fn test_load_all_vectors_rejects_corrupt_blob_length() {
        let db = Database::open_in_memory().unwrap();
        db.with_transaction(|tx| {
            tx.execute(
                "INSERT INTO drawers (id, content, embedding, wing, room, source_file, added_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "badblob000000000000000000000001",
                    "bad",
                    vec![1u8, 2, 3],
                    "w",
                    "r",
                    "",
                    "test"
                ],
            )?;
            Ok(())
        })
        .unwrap();

        let err = db.load_all_vectors().unwrap_err().to_string();
        assert!(err.contains("invalid embedding blob length"));
    }

    #[test]
    fn test_load_all_vectors_rejects_wrong_dimension() {
        let db = Database::open_in_memory().unwrap();
        let blob: Vec<u8> = [1.0f32, 2.0, 3.0]
            .into_iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        db.with_transaction(|tx| {
            tx.execute(
                "INSERT INTO drawers (id, content, embedding, wing, room, source_file, added_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "baddim0000000000000000000000002",
                    "bad",
                    blob,
                    "w",
                    "r",
                    "",
                    "test"
                ],
            )?;
            Ok(())
        })
        .unwrap();

        let err = db.load_all_vectors().unwrap_err().to_string();
        assert!(err.contains("embedding dimension"));
    }
}

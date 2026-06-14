use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, Transaction};

use crate::error::MemoryError;
use ironrace_embed::embedder::EMBED_DIM;

const SCHEMA_SQL: &str = include_str!("../../migrations/001_init.sql");
const FTS_SQL: &str = include_str!("../../migrations/002_fts.sql");
const COLLAB_SQL: &str = include_str!("../../migrations/003_collab.sql");
const COLLAB_V1_SQL: &str = include_str!("../../migrations/004_collab_planning_v1.sql");
const COLLAB_V2_SQL: &str = include_str!("../../migrations/005_collab_v2.sql");
const COLLAB_IMPLEMENTER_SQL: &str = include_str!("../../migrations/006_collab_implementer.sql");
const DROP_CURRENT_TASK_INDEX_SQL: &str =
    include_str!("../../migrations/007_drop_current_task_index.sql");
const METRICS_SQL: &str = include_str!("../../migrations/008_metrics.sql");

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

    /// Open an EXISTING database with a caller-bounded busy timeout and **no
    /// migration**. For latency-critical hot paths (the UserPromptSubmit hook)
    /// that must never pay the default 5 s busy timeout or run schema
    /// migrations. Uses `SQLITE_OPEN_READ_WRITE` WITHOUT `_CREATE`, so a missing
    /// file errors instead of being silently created.
    ///
    /// The `0o600` owner-only permission hardening that [`Self::open`] applies is
    /// intentionally delegated to `open`: this opener never creates the file (no
    /// `_CREATE` flag), so the file already exists and was hardened by whichever
    /// `open` call created it. Re-applying the chmod here would only add a syscall
    /// to a latency-critical hot path.
    pub fn open_with_busy_timeout(path: &Path, busy: Duration) -> Result<Self, MemoryError> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(busy)?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        Ok(Self { conn })
    }

    /// Execute raw SQL against the connection. Test-only fixture for putting the
    /// schema into a failure state (e.g. dropping a table) so error-propagation
    /// paths can be exercised. Compiled out of release builds.
    #[cfg(test)]
    pub(crate) fn exec_raw(&self, sql: &str) -> Result<(), MemoryError> {
        self.conn.execute_batch(sql)?;
        Ok(())
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

    /// Run schema migrations in version order. Idempotent: uses schema_version
    /// to skip already-applied migrations.
    ///
    /// Concurrency: serializes across processes/threads via `BEGIN IMMEDIATE`.
    /// The version-gated migrations contain non-idempotent `ALTER TABLE … ADD
    /// COLUMN` statements that fail with `duplicate column` if two openers
    /// race the same migration step. Acquiring the SQLite write lock upfront
    /// means a second migrator either (a) blocks until the first commits and
    /// then re-reads `MAX(version)` to see the bumped value and skip the
    /// already-applied steps, or (b) times out on `busy_timeout` and surfaces
    /// a clean error rather than corrupting the schema. The base
    /// `SCHEMA_SQL` (migration 001) is idempotent (`CREATE TABLE IF NOT
    /// EXISTS`) and runs outside the lock so a fresh-DB first-open path
    /// stays simple.
    pub fn migrate(&self) -> Result<(), MemoryError> {
        // v1: base schema (drawers, entities, triples, wal_log, schema_version)
        retry_on_busy(|| self.conn.execute_batch(SCHEMA_SQL))?;

        // Acquire the SQLite write lock for the remaining version-gated
        // migrations. `retry_on_busy` handles the contention path (a peer
        // migrator holding the lock); once we own the lock, no other writer
        // can interleave a non-idempotent `ALTER TABLE` with our reads.
        retry_on_busy(|| self.conn.execute_batch("BEGIN IMMEDIATE"))?;

        let result = self.run_version_gated_migrations();

        match &result {
            Ok(_) => {
                self.conn.execute_batch("COMMIT")?;
            }
            Err(_) => {
                // Best-effort rollback; even if it fails the caller already
                // has the migration error and the connection will be dropped.
                let _ = self.conn.execute_batch("ROLLBACK");
            }
        }
        result
    }

    /// Inside-lock half of `migrate()`. Re-reads `MAX(version)` so a peer
    /// migrator that just committed is observed before we run an `ALTER
    /// TABLE`. Do not call outside `migrate()` — assumes the caller holds
    /// `BEGIN IMMEDIATE`.
    fn run_version_gated_migrations(&self) -> Result<(), MemoryError> {
        let current_version = read_schema_version(&self.conn)?;

        // v2: FTS5 full-text search index for hybrid BM25+vector retrieval
        if current_version < 2 {
            self.conn.execute_batch(FTS_SQL)?;
        }

        // v3: collab protocol tables for bounded planning between Claude and Codex
        if current_version < 3 {
            self.conn.execute_batch(COLLAB_SQL)?;
        }

        // v4: planning protocol v1 final — task, review_round, ended_at columns
        // and PlanEscalated → PlanLocked data migration.
        if current_version < 4 {
            self.conn.execute_batch(COLLAB_V1_SQL)?;
        }

        // v5: collab v2 coding loop — task_list, per-task & global round
        // counters, base_sha / last_head_sha drift tracking, pr_url,
        // coding_failure.
        if current_version < 5 {
            self.conn.execute_batch(COLLAB_V2_SQL)?;
        }

        // v6: per-session `implementer` column (claude|codex) so
        // `/collab start --implementer=codex` can route the
        // `CodeImplementPending` phase to Codex.
        if current_version < 6 {
            self.conn.execute_batch(COLLAB_IMPLEMENTER_SQL)?;
        }

        // v7: drop the now-zombified `current_task_index` column added by
        // migration 005. v3 batch mode replaced the per-task loop and the
        // column has been written as NULL and never read since.
        if current_version < 7 {
            self.conn.execute_batch(DROP_CURRENT_TASK_INDEX_SQL)?;
        }

        // v8: metrics counter tables (token_usage, occupancy_samples,
        // session_summary, task_outcomes) per METRICS_SPEC §5/§8. All DDL is
        // IF NOT EXISTS so it stays safe under the BEGIN IMMEDIATE race path.
        if current_version < 8 {
            self.conn.execute_batch(METRICS_SQL)?;
        }

        Ok(())
    }

    pub fn create_collab_tables(&self) -> Result<(), MemoryError> {
        retry_on_busy(|| self.conn.execute_batch(COLLAB_SQL))?;
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

    /// Borrow the underlying connection for a read-only closure. Unlike
    /// [`Self::with_transaction`], this opens no transaction — use it for plain
    /// `SELECT`s (e.g. the session-start hook reading collab/diary state) so a
    /// read is not wrapped in a write-capable `BEGIN`/`COMMIT`.
    pub fn with_connection<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, MemoryError>,
    ) -> Result<T, MemoryError> {
        f(&self.conn)
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
        let mut skipped = 0usize;
        for row in rows {
            match row {
                Ok(item) => result.push(item),
                Err(_) => skipped += 1,
            }
        }
        if skipped > 0 {
            tracing::warn!(
                "{skipped} drawer(s) skipped: embedding dimension mismatch — \
                 re-embed or run `ironmem migrate` to restore full search coverage"
            );
        }
        Ok(result)
    }
}

/// Read the highest applied schema version from `schema_version`.
///
/// Returns `1` only for the legitimate fresh-DB case where the table exists
/// but is empty (`MAX(version)` is `NULL`). Real query failures — a missing
/// `schema_version` table, a corrupt row, or a lock/`SQLITE_BUSY` — are
/// **propagated** rather than silently collapsed to `1`. The previous
/// `query_row(...).unwrap_or(1)` masked those failures, which would cause the
/// migrator to treat a broken DB as brand-new and re-run every migration.
fn read_schema_version(conn: &Connection) -> Result<i64, MemoryError> {
    let max_version: Option<i64> =
        conn.query_row("SELECT MAX(version) FROM schema_version", [], |row| {
            row.get(0)
        })?;
    Ok(max_version.unwrap_or(1))
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

    use rusqlite::OptionalExtension;

    use super::*;

    /// Returns a `(TempDir, PathBuf)` pair for a database nested under a temp directory.
    /// The caller **must** retain the `TempDir` for the lifetime of the test; dropping it
    /// deletes the directory and invalidates the path.
    fn nested_db_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("sub").join("test.db");
        (dir, db_path)
    }

    #[test]
    fn open_with_busy_timeout_reads_without_migrating() {
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite3");
        {
            let db = Database::open(&path).unwrap();
            db.migrate().unwrap();
        }
        let db = Database::open_with_busy_timeout(&path, Duration::from_millis(50)).unwrap();
        let n = db.count_drawers(None).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn open_with_busy_timeout_errors_on_missing_file() {
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.sqlite3");
        let result = Database::open_with_busy_timeout(&path, Duration::from_millis(50));
        assert!(
            result.is_err(),
            "missing DB file must error, not be created"
        );
        assert!(!path.exists(), "must not create the file");
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
                    vec![0u8; ironrace_embed::embedder::EMBED_DIM * std::mem::size_of::<f32>()],
                    "w",
                    "r",
                    "",
                    "test"
                ],
            )?;
            assert_eq!(rows_inserted, 1);
            Err(MemoryError::NotFound("forced rollback".into()))
        });

        assert!(result.is_err());
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM drawers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    // ---- Migration 008 (metrics tables) coverage ----

    fn schema_version_of(db: &Database) -> i64 {
        db.conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap()
    }

    fn table_exists(db: &Database, name: &str) -> bool {
        db.conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_some()
    }

    fn index_exists(db: &Database, name: &str) -> bool {
        db.conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1",
                [name],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_some()
    }

    const METRICS_TABLES: [&str; 4] = [
        "token_usage",
        "occupancy_samples",
        "session_summary",
        "task_outcomes",
    ];

    /// Build a connection migrated to exactly v7 (no metrics tables yet) by
    /// replaying migrations 001-007 directly from the module consts.
    fn open_at_v7() -> Database {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn.execute_batch(FTS_SQL).unwrap();
        conn.execute_batch(COLLAB_SQL).unwrap();
        conn.execute_batch(COLLAB_V1_SQL).unwrap();
        conn.execute_batch(COLLAB_V2_SQL).unwrap();
        conn.execute_batch(COLLAB_IMPLEMENTER_SQL).unwrap();
        conn.execute_batch(DROP_CURRENT_TASK_INDEX_SQL).unwrap();
        Database { conn }
    }

    #[test]
    fn test_fresh_migrate_reaches_v8_with_metrics_tables() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(schema_version_of(&db), 8);
        for t in METRICS_TABLES {
            assert!(table_exists(&db, t), "missing table {t}");
        }
        assert!(index_exists(&db, "idx_token_usage_task_ts"));
        assert!(index_exists(&db, "idx_token_usage_collab_phase"));
        assert!(index_exists(&db, "idx_occupancy_session_ts"));
        assert!(index_exists(&db, "idx_task_outcomes_collab"));
    }

    #[test]
    fn test_v7_to_v8_upgrade_adds_metrics_tables() {
        let db = open_at_v7();
        assert_eq!(schema_version_of(&db), 7);
        for t in METRICS_TABLES {
            assert!(!table_exists(&db, t), "table {t} should not exist at v7");
        }
        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), 8);
        for t in METRICS_TABLES {
            assert!(table_exists(&db, t), "missing table {t} after upgrade");
        }
    }

    #[test]
    fn test_migrate_twice_idempotent_at_v8() {
        let db = Database::open_in_memory().unwrap();
        db.migrate().unwrap();
        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), 8);
    }

    // ---- read_schema_version: distinguish fresh-DB from real DB errors ----

    #[test]
    fn read_schema_version_returns_max_applied() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
             INSERT INTO schema_version (version) VALUES (3), (7), (5);",
        )
        .unwrap();
        assert_eq!(super::read_schema_version(&conn).unwrap(), 7);
    }

    #[test]
    fn read_schema_version_empty_table_defaults_to_1() {
        // An existing-but-empty schema_version table (MAX -> NULL) is the
        // legitimate fresh-DB case and must default to 1, not error.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE schema_version (version INTEGER PRIMARY KEY);")
            .unwrap();
        assert_eq!(super::read_schema_version(&conn).unwrap(), 1);
    }

    #[test]
    fn read_schema_version_missing_table_is_error_not_silent_v1() {
        // The old `.unwrap_or(1)` masked this as v1 (silently re-running
        // migrations). A genuinely broken/locked DB must surface an error.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        assert!(super::read_schema_version(&conn).is_err());
    }
}

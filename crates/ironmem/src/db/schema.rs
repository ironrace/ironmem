use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, Transaction};

use crate::db::ReadOnlyDb;
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
const COLLAB_PLAN_DRAWERS_SQL: &str = include_str!("../../migrations/009_collab_plan_drawers.sql");
const COLLAB_GENERATION_LEASE_SQL: &str =
    include_str!("../../migrations/010_collab_generation_lease.sql");
const CODE_MAPS_SQL: &str = include_str!("../../migrations/011_code_maps.sql");
const SYMBOL_IMPORT_GRAPH_SQL: &str = include_str!("../../migrations/012_symbol_import_graph.sql");
const METRICS_HARNESS_CHECK_SQL: &str =
    include_str!("../../migrations/013_metrics_harness_check.sql");
const CONTEXT_SIZE_REFS_SQL: &str = include_str!("../../migrations/014_context_size_refs.sql");
const COLLAB_RECOVERY_STATE_SQL: &str =
    include_str!("../../migrations/015_collab_recovery_state.sql");
const COLLAB_MESSAGE_DRAWERS_SQL: &str =
    include_str!("../../migrations/016_collab_message_drawers.sql");
const DRAWER_SUPERSESSION_SQL: &str = include_str!("../../migrations/017_drawer_supersession.sql");
const MCP_RESPONSE_COMPACTION_METRICS_SQL: &str =
    include_str!("../../migrations/018_mcp_response_compaction_metrics.sql");
const COLLAB_PILOT_SQL: &str = include_str!("../../migrations/019_collab_pilot.sql");

/// Highest schema version a fully-migrated database reports. Bump alongside the
/// `run_version_gated_migrations` ladder below so `ironmem doctor` can tell a
/// behind-migration database from an up-to-date one.
pub const LATEST_SCHEMA_VERSION: i64 = 19;

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

    /// Open an EXISTING database **read-only** and **without migration**.
    ///
    /// Designed for the dashboard server, which must never create, modify, or
    /// migrate the database file. Uses `SQLITE_OPEN_READ_ONLY | NO_MUTEX` so a
    /// missing file fails fast instead of being silently created. No WAL pragma,
    /// no `create_dir_all`, no `migrate`. `PRAGMA foreign_keys=ON` is the only
    /// pragma executed — it is safe in read-only mode.
    ///
    /// Returns [`ReadOnlyDb`], a thin newtype exposing only read/query methods,
    /// so a dashboard handler that tries to write fails to compile rather than at
    /// runtime.
    ///
    /// A missing file is handled by SQLite itself: opening with
    /// `SQLITE_OPEN_READ_ONLY` (and no `SQLITE_OPEN_CREATE`) errors with
    /// `SQLITE_CANTOPEN` and creates nothing. That open error is mapped to the
    /// same descriptive `db not found at <path>` message — no TOCTOU
    /// `path.exists()` pre-check is performed.
    pub fn open_read_only(path: &Path) -> Result<ReadOnlyDb, MemoryError> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ErrorCode::CannotOpen,
                    ..
                },
                _,
            ) => MemoryError::NotFound(format!("db not found at {}", path.display())),
            other => MemoryError::Db(other),
        })?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        Ok(ReadOnlyDb {
            inner: Self { conn },
        })
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

        // v9: plan-by-reference drawer-id columns on collab_sessions
        // (issue #90). Nullable adds; NULL = legacy inline-plan path.
        if current_version < 9 {
            self.conn.execute_batch(COLLAB_PLAN_DRAWERS_SQL)?;
        }

        // v10: per-actor generation lease table for session_handoff (issue #91).
        if current_version < 10 {
            self.conn.execute_batch(COLLAB_GENERATION_LEASE_SQL)?;
        }

        // v11: lazy per-area code maps (issue #94) — code_maps sidecar table +
        // token_usage exploration-attribution columns (map_status, turn_id, area).
        if current_version < 11 {
            self.conn.execute_batch(CODE_MAPS_SQL)?;
        }

        // v12: local symbol/import graph index — code_index_files, code_symbols,
        // code_imports, and code_symbol_edges tables for offline code-aware retrieval.
        if current_version < 12 {
            self.conn.execute_batch(SYMBOL_IMPORT_GRAPH_SQL)?;
        }

        // v13: relax the metrics harness CHECK from claude/codex-only to the
        // registry slug form so any registered harness can persist metrics
        // (issue #155). Value-preserving rebuild of token_usage, occupancy_samples,
        // session_summary; rows copied byte-for-byte. Collab implementer (006) and
        // generation-lease agent (010) CHECKs stay claude/codex by design.
        if current_version < 13 {
            self.conn.execute_batch(METRICS_HARNESS_CHECK_SQL)?;
        }

        // v14: compact collab task-list refs and per-tool MCP response
        // attribution. Nullable columns preserve all pre-014 rows.
        if current_version < 14 {
            self.conn.execute_batch(CONTEXT_SIZE_REFS_SQL)?;
        }

        // v15: recoverable tooling-failure state on collab_sessions (issue
        // #197). Six nullable columns; NULL means no failure/recovery in
        // flight, which is every pre-015 row.
        if current_version < 15 {
            self.conn.execute_batch(COLLAB_RECOVERY_STATE_SQL)?;
        }

        // v16: nullable drawer references for collab messages (issue #206).
        // NULL preserves the legacy inline-content messages sent before this
        // migration; new queue writes always provide a drawer id.
        if current_version < 16 {
            self.conn.execute_batch(COLLAB_MESSAGE_DRAWERS_SQL)?;
        }

        // v17: durable drawer supersession lineage. NULL preserves every
        // existing drawer as current, while a partial wing/room index supports
        // current-only supersession and duplicate checks.
        if current_version < 17 {
            self.conn.execute_batch(DRAWER_SUPERSESSION_SQL)?;
        }

        // v18: nullable serialized JSON byte counts for response compaction
        // telemetry. NULL preserves the distinction between unavailable and
        // zero-savings values for all pre-018 rows.
        if current_version < 18 {
            self.conn
                .execute_batch(MCP_RESPONSE_COMPACTION_METRICS_SQL)?;
        }

        // v19: per-session `pilot` agent role for protocol genericity
        // (issue #246). Every pre-019 row reads `pilot='claude'`, so no
        // data migration is needed. The column defaults to 'claude' to
        // preserve the original behavior for sessions and callers that
        // omit the field.
        if current_version < 19 {
            self.conn.execute_batch(COLLAB_PILOT_SQL)?;
        }

        Ok(())
    }

    /// Read the highest applied schema version from this connection without
    /// running any migration. Useful for diagnostics (`ironmem doctor`) on a
    /// database that may be behind the current binary.
    pub fn schema_version(&self) -> Result<i64, MemoryError> {
        read_schema_version(&self.conn)
    }

    pub fn create_collab_tables(&self) -> Result<(), MemoryError> {
        self.migrate()
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
    fn latest_schema_version_matches_highest_migration() {
        // The exported constant must track the highest migration a fresh,
        // fully-migrated database reports — doctor compares against it.
        let db = Database::open_in_memory().unwrap();
        assert_eq!(LATEST_SCHEMA_VERSION, db.schema_version().unwrap());
        assert_eq!(LATEST_SCHEMA_VERSION, 19);
    }

    #[test]
    fn create_collab_tables_applies_the_current_collab_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("collab.sqlite3")).unwrap();

        db.create_collab_tables().unwrap();

        assert_eq!(db.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
        assert!(column_exists(&db, "messages", "drawer_id"));
    }

    #[test]
    fn schema_version_reads_current_version_without_migrating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite3");
        {
            let db = Database::open(&path).unwrap();
            db.migrate().unwrap();
        }
        // Re-open without migrating; the persisted version is still readable.
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
    }

    // ---- open_read_only tests ----

    #[test]
    fn open_read_only_errors_on_missing_file_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.sqlite3");
        assert!(!path.exists(), "precondition: file must not exist");
        let result = Database::open_read_only(&path);
        assert!(result.is_err(), "open_read_only of missing path must error");
        assert!(
            !path.exists(),
            "open_read_only must NOT create the file on error"
        );
    }

    #[test]
    fn open_read_only_does_not_upgrade_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.sqlite3");
        // Create + fully migrate a db.
        {
            let db = Database::open(&path).unwrap();
            db.migrate().unwrap();
        }
        let version_before = {
            let db = Database::open(&path).unwrap();
            db.schema_version().unwrap()
        };
        // Open read-only — must not change schema version.
        let db_ro = Database::open_read_only(&path).unwrap();
        let version_after = db_ro.schema_version().unwrap();
        assert_eq!(
            version_before, version_after,
            "open_read_only must not alter schema_version"
        );
        assert_eq!(version_after, LATEST_SCHEMA_VERSION);
    }

    #[test]
    fn open_read_only_schema_version_mismatch_is_readable() {
        // Verify that schema_version() works on a read-only connection so the
        // dashboard can report version mismatches without write access.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sv.sqlite3");
        {
            let db = Database::open(&path).unwrap();
            db.migrate().unwrap();
        }
        let db = Database::open_read_only(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
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

    fn column_exists(db: &Database, table: &str, col: &str) -> bool {
        let mut stmt = db
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let name: String = row.get(1).unwrap();
            if name == col {
                return true;
            }
        }
        false
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

    /// Build a connection migrated to exactly v8 (no plan-drawer columns yet) by
    /// replaying migrations 001-008 directly from the module consts.
    fn open_at_v8() -> Database {
        let db = open_at_v7();
        db.conn.execute_batch(METRICS_SQL).unwrap();
        db
    }

    #[test]
    fn test_fresh_migrate_reaches_head_with_all_tables() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
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
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        for t in METRICS_TABLES {
            assert!(table_exists(&db, t), "missing table {t} after upgrade");
        }
    }

    #[test]
    fn test_migrate_twice_idempotent() {
        let db = Database::open_in_memory().unwrap();
        db.migrate().unwrap();
        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
    }

    // ---- Migration 009 (plan-by-reference drawer-id columns) coverage ----

    const PLAN_DRAWER_COLUMNS: [&str; 2] = ["canonical_plan_drawer_id", "final_plan_drawer_id"];

    #[test]
    fn test_fresh_migrate_reaches_v9_with_plan_drawer_columns() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        for c in PLAN_DRAWER_COLUMNS {
            assert!(
                column_exists(&db, "collab_sessions", c),
                "missing column {c} on collab_sessions"
            );
        }
    }

    #[test]
    fn test_v8_to_v9_upgrade_adds_plan_drawer_columns() {
        let db = open_at_v8();
        assert_eq!(schema_version_of(&db), 8);
        for c in PLAN_DRAWER_COLUMNS {
            assert!(
                !column_exists(&db, "collab_sessions", c),
                "column {c} should not exist at v8"
            );
        }
        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        for c in PLAN_DRAWER_COLUMNS {
            assert!(
                column_exists(&db, "collab_sessions", c),
                "missing column {c} after upgrade"
            );
        }
    }

    /// Build a connection migrated to exactly v9 (no generation-lease table yet) by
    /// replaying migrations 001-009 directly from the module consts.
    fn open_at_v9() -> Database {
        let db = open_at_v8();
        db.conn.execute_batch(COLLAB_PLAN_DRAWERS_SQL).unwrap();
        db
    }

    /// Build a connection migrated to exactly v10 (no code_maps table yet) by
    /// replaying migrations 001-010 directly from the module consts.
    fn open_at_v10() -> Database {
        let db = open_at_v9();
        db.conn.execute_batch(COLLAB_GENERATION_LEASE_SQL).unwrap();
        db
    }

    #[test]
    fn test_v9_to_v10_upgrade_adds_lease_table() {
        let db = open_at_v9();
        assert_eq!(schema_version_of(&db), 9);
        assert!(
            !table_exists(&db, "collab_actor_generations"),
            "lease table should not exist at v9"
        );
        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        assert!(
            table_exists(&db, "collab_actor_generations"),
            "missing collab_actor_generations after upgrade"
        );
        assert!(
            index_exists(&db, "idx_collab_actor_generations_session"),
            "missing idx_collab_actor_generations_session after upgrade"
        );
    }

    #[test]
    fn test_v8_to_v9_upgrade_preserves_existing_collab_sessions_with_null_plan_drawer_ids() {
        let db = open_at_v8();
        crate::collab::queue::create_session(
            &db.conn,
            "legacy-session",
            "/repo",
            "main",
            Some("legacy task"),
            crate::collab::Agent::Claude,
        )
        .unwrap();

        db.migrate().unwrap();

        let session = crate::collab::queue::load_session(&db.conn, "legacy-session").unwrap();
        assert!(session.canonical_plan_drawer_id.is_none());
        assert!(session.final_plan_drawer_id.is_none());
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

    // ---- Migration 011 (code_maps table + token_usage exploration columns) ----

    /// Build a connection migrated to exactly v11 (no symbol graph tables yet) by
    /// replaying migrations 001-011 directly from the module consts.
    fn open_at_v11() -> Database {
        let db = open_at_v10();
        db.conn.execute_batch(CODE_MAPS_SQL).unwrap();
        db
    }

    #[test]
    fn test_fresh_migrate_reaches_v11_tables() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        assert!(table_exists(&db, "code_maps"), "code_maps table must exist");
    }

    const TOKEN_USAGE_V11_COLUMNS: [&str; 3] = ["map_status", "turn_id", "area"];

    #[test]
    fn test_v10_to_v11_upgrade_adds_code_maps() {
        let db = open_at_v10();
        assert_eq!(schema_version_of(&db), 10);
        assert!(
            !table_exists(&db, "code_maps"),
            "code_maps should not exist at v10"
        );
        for c in TOKEN_USAGE_V11_COLUMNS {
            assert!(
                !column_exists(&db, "token_usage", c),
                "token_usage.{c} should not exist at v10"
            );
        }
        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        assert!(
            table_exists(&db, "code_maps"),
            "code_maps must exist after upgrade to v11+"
        );
        for c in TOKEN_USAGE_V11_COLUMNS {
            assert!(
                column_exists(&db, "token_usage", c),
                "token_usage.{c} must exist after upgrade to v11+"
            );
        }
    }

    #[test]
    fn test_v10_to_v11_preserves_existing_token_usage_rows_as_null() {
        let db = open_at_v10();
        // Insert a token_usage row at v10 (before the exploration columns exist).
        db.conn
            .execute(
                "INSERT INTO token_usage
                    (ts, source, harness, input_tokens, output_tokens,
                     cache_creation_input_tokens, cache_read_input_tokens,
                     estimated, chars)
                 VALUES ('2026-06-15T00:00:00Z', 'mcp_response', 'claude',
                         0, 0, 0, 0, 1, 0)",
                [],
            )
            .unwrap();

        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);

        // The pre-existing row must read back with the three new columns NULL.
        let (map_status, turn_id, area): (Option<String>, Option<String>, Option<String>) = db
            .conn
            .query_row(
                "SELECT map_status, turn_id, area FROM token_usage
                 WHERE ts = '2026-06-15T00:00:00Z'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(map_status.is_none(), "map_status must back-fill as NULL");
        assert!(turn_id.is_none(), "turn_id must back-fill as NULL");
        assert!(area.is_none(), "area must back-fill as NULL");
    }

    #[test]
    fn test_migrate_twice_idempotent_v11() {
        let db = Database::open_in_memory().unwrap();
        db.migrate().unwrap();
        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
    }

    // ---- Migration 012 (symbol/import graph tables) ----

    const SYMBOL_GRAPH_TABLES: [&str; 4] = [
        "code_index_files",
        "code_symbols",
        "code_imports",
        "code_symbol_edges",
    ];

    #[test]
    fn test_fresh_migrate_reaches_v12() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        for t in SYMBOL_GRAPH_TABLES {
            assert!(table_exists(&db, t), "missing table {t} at v12");
        }
        assert!(index_exists(&db, "idx_code_symbols_repo_name"));
        assert!(index_exists(&db, "idx_code_symbols_repo_qname"));
        assert!(index_exists(&db, "idx_code_symbols_repo_path"));
        assert!(index_exists(&db, "idx_code_imports_repo_module"));
        assert!(index_exists(&db, "idx_code_imports_repo_path"));
        assert!(index_exists(&db, "idx_code_symbol_edges_repo_from"));
        assert!(index_exists(&db, "idx_code_symbol_edges_repo_to"));
        assert!(index_exists(&db, "idx_code_symbol_edges_repo_kind"));
    }

    #[test]
    fn test_v11_to_v12_upgrade_adds_symbol_graph_tables() {
        let db = open_at_v11();
        assert_eq!(schema_version_of(&db), 11);
        for t in SYMBOL_GRAPH_TABLES {
            assert!(!table_exists(&db, t), "table {t} should not exist at v11");
        }
        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        for t in SYMBOL_GRAPH_TABLES {
            assert!(
                table_exists(&db, t),
                "missing table {t} after upgrade to v12"
            );
        }
    }

    #[test]
    fn test_migrate_twice_idempotent_v12() {
        let db = Database::open_in_memory().unwrap();
        db.migrate().unwrap();
        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
    }

    // ---- Migration 013 (relax metrics harness CHECK to registry slug) ----

    /// Build a connection migrated to exactly v12 (no harness CHECK relaxation yet)
    /// by replaying migrations 001-012 directly from the module consts.
    fn open_at_v12() -> Database {
        let db = open_at_v11();
        db.conn.execute_batch(SYMBOL_IMPORT_GRAPH_SQL).unwrap();
        db
    }

    #[test]
    fn test_v12_to_v13_preserves_existing_metrics_rows() {
        let db = open_at_v12();
        assert_eq!(schema_version_of(&db), 12);

        // Insert claude and codex rows into all three metrics tables before migration.
        db.conn
            .execute(
                "INSERT INTO token_usage
                    (ts, source, harness, input_tokens, output_tokens,
                     cache_creation_input_tokens, cache_read_input_tokens,
                     estimated, chars)
                 VALUES ('2026-06-29T10:00:00Z', 'transcript', 'claude',
                         100, 50, 10, 5, 0, 200)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO token_usage
                    (ts, source, harness, input_tokens, output_tokens,
                     cache_creation_input_tokens, cache_read_input_tokens,
                     estimated, chars)
                 VALUES ('2026-06-29T10:01:00Z', 'mcp_response', 'codex',
                         80, 30, 0, 0, 0, 150)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO occupancy_samples
                    (ts, harness, input_tokens, cache_read_input_tokens)
                 VALUES ('2026-06-29T10:02:00Z', 'claude', 500, 100)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO occupancy_samples
                    (ts, harness, input_tokens, cache_read_input_tokens)
                 VALUES ('2026-06-29T10:03:00Z', 'codex', 400, 50)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO session_summary (session_id, harness)
                 VALUES ('pre-v13-claude', 'claude')",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO session_summary (session_id, harness)
                 VALUES ('pre-v13-codex', 'codex')",
                [],
            )
            .unwrap();

        // Run the remaining migrations.
        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);

        // Verify token_usage rows are preserved byte-for-byte.
        let tu_rows: Vec<(String, String, i64, i64)> = {
            let mut stmt = db
                .conn
                .prepare(
                    "SELECT harness, source, input_tokens, output_tokens
                     FROM token_usage ORDER BY ts",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(tu_rows.len(), 2);
        assert_eq!(tu_rows[0], ("claude".into(), "transcript".into(), 100, 50));
        assert_eq!(tu_rows[1], ("codex".into(), "mcp_response".into(), 80, 30));

        // Verify occupancy_samples rows are preserved.
        let occ_rows: Vec<(String, i64)> = {
            let mut stmt = db
                .conn
                .prepare("SELECT harness, input_tokens FROM occupancy_samples ORDER BY ts")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(occ_rows.len(), 2);
        assert_eq!(occ_rows[0], ("claude".into(), 500));
        assert_eq!(occ_rows[1], ("codex".into(), 400));

        // Verify session_summary rows are preserved.
        let ss_rows: Vec<(String, String)> = {
            let mut stmt = db
                .conn
                .prepare("SELECT session_id, harness FROM session_summary ORDER BY session_id")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(ss_rows.len(), 2);
        assert_eq!(ss_rows[0], ("pre-v13-claude".into(), "claude".into()));
        assert_eq!(ss_rows[1], ("pre-v13-codex".into(), "codex".into()));
    }

    #[test]
    fn test_v13_accepts_synthetic_third_harness_ids() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);

        for harness in &["gemini", "grok-2", "grok-ai", "grok_ai", "copilot4"] {
            db.conn
                .execute(
                    "INSERT INTO token_usage
                        (ts, source, harness, input_tokens, output_tokens,
                         cache_creation_input_tokens, cache_read_input_tokens,
                         estimated, chars)
                     VALUES (?1, 'transcript', ?2, 0, 0, 0, 0, 0, 0)",
                    rusqlite::params![format!("2026-06-29T12:00:00Z-{harness}"), harness],
                )
                .unwrap_or_else(|e| panic!("harness '{harness}' should be accepted: {e}"));

            db.conn
                .execute(
                    "INSERT INTO occupancy_samples (ts, harness, input_tokens, cache_read_input_tokens)
                     VALUES (?1, ?2, 0, 0)",
                    rusqlite::params![format!("2026-06-29T12:01:00Z-{harness}"), harness],
                )
                .unwrap_or_else(|e| panic!("harness '{harness}' should be accepted: {e}"));

            db.conn
                .execute(
                    "INSERT INTO session_summary (session_id, harness) VALUES (?1, ?2)",
                    rusqlite::params![format!("synth-{harness}"), harness],
                )
                .unwrap_or_else(|e| panic!("harness '{harness}' should be accepted: {e}"));
        }
    }

    #[test]
    fn test_v13_rejects_invalid_harness_ids() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);

        // These must all be rejected by the relaxed CHECK.
        let invalid: &[&str] = &[
            "Claude", // uppercase
            "a b",    // space
            "a;b",    // semicolon
            "-x",     // leading hyphen (not [a-z0-9])
            "",       // empty string fails GLOB '[a-z0-9]*' (needs at least one char)
        ];

        for bad in invalid {
            let tu_result = db.conn.execute(
                "INSERT INTO token_usage
                    (ts, source, harness, input_tokens, output_tokens,
                     cache_creation_input_tokens, cache_read_input_tokens,
                     estimated, chars)
                 VALUES (?1, 'transcript', ?2, 0, 0, 0, 0, 0, 0)",
                rusqlite::params![format!("2026-06-29T13:00:00Z-bad"), bad],
            );
            assert!(
                tu_result.is_err(),
                "token_usage should reject harness={bad:?}"
            );

            let occ_result = db.conn.execute(
                "INSERT INTO occupancy_samples (ts, harness, input_tokens, cache_read_input_tokens)
                 VALUES (?1, ?2, 0, 0)",
                rusqlite::params![format!("2026-06-29T13:01:00Z-bad"), bad],
            );
            assert!(
                occ_result.is_err(),
                "occupancy_samples should reject harness={bad:?}"
            );

            let ss_result = db.conn.execute(
                "INSERT INTO session_summary (session_id, harness) VALUES (?1, ?2)",
                rusqlite::params![format!("bad-{bad}"), bad],
            );
            assert!(
                ss_result.is_err(),
                "session_summary should reject harness={bad:?}"
            );
        }
    }

    #[test]
    fn test_fresh_migrate_reaches_v14() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        // All three rebuilt tables and their indexes must still exist.
        for t in ["token_usage", "occupancy_samples", "session_summary"] {
            assert!(table_exists(&db, t), "missing table {t} at v14");
        }
        assert!(index_exists(&db, "idx_token_usage_task_ts"));
        assert!(index_exists(&db, "idx_token_usage_collab_phase"));
        assert!(index_exists(&db, "idx_token_usage_mcp_tool"));
        assert!(index_exists(&db, "idx_occupancy_session_ts"));
        assert!(column_exists(&db, "collab_sessions", "task_list_drawer_id"));
        assert!(column_exists(&db, "token_usage", "tool_name"));
        assert!(column_exists(&db, "token_usage", "original_response_bytes"));
        assert!(column_exists(
            &db,
            "token_usage",
            "compacted_response_bytes"
        ));
    }

    #[test]
    fn test_migrate_twice_idempotent_v14() {
        let db = Database::open_in_memory().unwrap();
        db.migrate().unwrap();
        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
    }

    // ---- Migration 014 (compact task-list refs + tool-name metrics) ----

    /// Build a connection migrated to exactly v13 (no task-list refs/tool-name
    /// attribution yet) by replaying migrations 001-013 directly.
    fn open_at_v13() -> Database {
        let db = open_at_v12();
        db.conn.execute_batch(METRICS_HARNESS_CHECK_SQL).unwrap();
        db
    }

    #[test]
    fn test_v13_to_v14_adds_context_size_columns_and_preserves_rows() {
        let db = open_at_v13();
        assert_eq!(schema_version_of(&db), 13);
        assert!(
            !column_exists(&db, "collab_sessions", "task_list_drawer_id"),
            "task_list_drawer_id should not exist at v13"
        );
        assert!(
            !column_exists(&db, "token_usage", "tool_name"),
            "token_usage.tool_name should not exist at v13"
        );

        crate::collab::queue::create_session(
            &db.conn,
            "legacy-v13-session",
            "/repo",
            "main",
            Some("legacy task"),
            crate::collab::Agent::Claude,
        )
        .unwrap();
        db.conn
            .execute(
                "INSERT INTO token_usage
                    (ts, source, harness, input_tokens, output_tokens,
                     cache_creation_input_tokens, cache_read_input_tokens,
                     estimated, chars)
                 VALUES ('2026-07-01T00:00:00Z', 'mcp_response', 'claude',
                         0, 40, 0, 0, 1, 160)",
                [],
            )
            .unwrap();

        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        assert!(column_exists(&db, "collab_sessions", "task_list_drawer_id"));
        assert!(column_exists(&db, "token_usage", "tool_name"));
        assert!(index_exists(&db, "idx_token_usage_mcp_tool"));

        let session = crate::collab::queue::load_session(&db.conn, "legacy-v13-session").unwrap();
        assert!(
            session.task_list_drawer_id.is_none(),
            "legacy sessions backfill task_list_drawer_id as NULL"
        );
        let tool_name: Option<String> = db
            .conn
            .query_row(
                "SELECT tool_name FROM token_usage WHERE ts = '2026-07-01T00:00:00Z'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            tool_name.is_none(),
            "legacy metrics rows backfill NULL tool_name"
        );
    }

    // ---- Migration 015 (collab recovery-state columns, issue #197) ----

    /// Build a connection migrated to exactly v14 (no recovery-state columns
    /// yet) by replaying migrations 001-014 directly from the module consts.
    fn open_at_v14() -> Database {
        let db = open_at_v13();
        db.conn.execute_batch(CONTEXT_SIZE_REFS_SQL).unwrap();
        db
    }

    const RECOVERY_STATE_COLUMNS: [&str; 7] = [
        "pending_failure",
        "failed_from_phase",
        "recovery_phase",
        "recovery_owner",
        "recovery_origin_owner",
        "recovery_attempts",
        "total_recovery_attempts",
    ];

    #[test]
    fn test_v14_to_v15_upgrade_preserves_existing_collab_sessions_and_adds_recovery_columns() {
        let db = open_at_v14();
        assert_eq!(schema_version_of(&db), 14);
        for col in RECOVERY_STATE_COLUMNS {
            assert!(
                !column_exists(&db, "collab_sessions", col),
                "{col} should not exist at v14"
            );
        }

        crate::collab::queue::create_session(
            &db.conn,
            "legacy-v14-session",
            "/repo",
            "main",
            Some("legacy task"),
            crate::collab::Agent::Codex,
        )
        .unwrap();

        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);

        for col in RECOVERY_STATE_COLUMNS {
            assert!(
                column_exists(&db, "collab_sessions", col),
                "missing {col} after upgrade"
            );
        }

        // Pre-existing column values survive the upgrade untouched.
        let record =
            crate::collab::queue::load_session_record(&db.conn, "legacy-v14-session").unwrap();
        assert_eq!(record.repo_path, "/repo");
        assert_eq!(record.branch, "main");
        assert_eq!(record.task.as_deref(), Some("legacy task"));
        assert_eq!(record.session.implementer, crate::collab::Agent::Codex);

        // The six new columns backfill NULL for legacy rows. Each is read back
        // as its own `query_row` (rather than one wide tuple) to keep the
        // return type simple for clippy's `type_complexity` lint.
        for col in RECOVERY_STATE_COLUMNS {
            let value: Option<String> = db
                .conn
                .query_row(
                    &format!("SELECT {col} FROM collab_sessions WHERE id = ?1"),
                    ["legacy-v14-session"],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(value.is_none(), "{col} should be NULL for a legacy row");
        }
    }

    /// Build a connection migrated to exactly v15 (before message drawer
    /// references) by replaying migrations 001-015 directly from the module
    /// consts.
    fn open_at_v15() -> Database {
        let db = open_at_v14();
        db.conn.execute_batch(COLLAB_RECOVERY_STATE_SQL).unwrap();
        db
    }

    #[test]
    fn test_v15_to_v16_adds_message_drawer_id_and_preserves_legacy_messages() {
        let db = open_at_v15();
        assert_eq!(schema_version_of(&db), 15);
        assert!(
            !column_exists(&db, "messages", "drawer_id"),
            "drawer_id should not exist at v15"
        );

        crate::collab::queue::create_session(
            &db.conn,
            "legacy-v15-message-session",
            "/repo",
            "main",
            None,
            crate::collab::Agent::Claude,
        )
        .unwrap();
        db.conn
            .execute(
                "INSERT INTO messages (id, session_id, sender, receiver, topic, content)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                [
                    "legacy-v15-message",
                    "legacy-v15-message-session",
                    "claude",
                    "codex",
                    "draft",
                    "legacy message body",
                ],
            )
            .unwrap();

        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        assert!(column_exists(&db, "messages", "drawer_id"));

        let messages =
            crate::collab::queue::recv_messages(&db.conn, "legacy-v15-message-session", "codex", 1)
                .unwrap();
        assert_eq!(messages.len(), 1);
        assert!(
            messages[0].drawer_id.is_none(),
            "legacy message drawer_id must remain NULL"
        );
    }

    #[test]
    fn test_migrate_twice_idempotent_v16() {
        let db = Database::open_in_memory().unwrap();
        db.migrate().unwrap();
        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
    }

    #[test]
    fn test_v16_to_v17_adds_drawer_supersession_and_preserves_legacy_drawers() {
        let db = open_at_v15();
        db.conn.execute_batch(COLLAB_MESSAGE_DRAWERS_SQL).unwrap();
        assert_eq!(schema_version_of(&db), 16);
        assert!(
            !column_exists(&db, "drawers", "superseded_by"),
            "superseded_by should not exist at v16"
        );

        db.conn
            .execute(
                "INSERT INTO drawers (id, content, embedding, wing, room, source_file, added_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "legacy-v16-drawer",
                    "legacy drawer",
                    vec![0_u8; EMBED_DIM * std::mem::size_of::<f32>()],
                    "legacy-wing",
                    "legacy-room",
                    "",
                    "test"
                ],
            )
            .unwrap();

        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        assert!(column_exists(&db, "drawers", "superseded_by"));
        assert!(index_exists(&db, "idx_drawers_current_wing_room"));
        let superseded_by: Option<String> = db
            .conn
            .query_row(
                "SELECT superseded_by FROM drawers WHERE id = ?1",
                ["legacy-v16-drawer"],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            superseded_by.is_none(),
            "legacy drawers must remain current"
        );
    }

    // ---- Migration 019 (collab pilot column, issue #246) ----

    /// Build a connection migrated to exactly v18 (no pilot column yet)
    /// by replaying migrations 001-018 directly from the module consts.
    fn open_at_v18() -> Database {
        let db = open_at_v15();
        db.conn.execute_batch(COLLAB_MESSAGE_DRAWERS_SQL).unwrap();
        db.conn.execute_batch(DRAWER_SUPERSESSION_SQL).unwrap();
        db.conn
            .execute_batch(MCP_RESPONSE_COMPACTION_METRICS_SQL)
            .unwrap();
        db
    }

    #[test]
    fn test_fresh_migrate_reaches_v19_with_pilot_column() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        assert!(
            column_exists(&db, "collab_sessions", "pilot"),
            "pilot column must exist on collab_sessions"
        );
    }

    #[test]
    fn test_v18_to_v19_adds_pilot_column_and_preserves_legacy_sessions() {
        let db = open_at_v18();
        assert_eq!(schema_version_of(&db), 18);
        assert!(
            !column_exists(&db, "collab_sessions", "pilot"),
            "pilot should not exist at v18"
        );

        crate::collab::queue::create_session(
            &db.conn,
            "legacy-v18-session",
            "/repo",
            "main",
            Some("legacy task"),
            crate::collab::Agent::Claude,
        )
        .unwrap();

        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        assert!(column_exists(&db, "collab_sessions", "pilot"));

        // Legacy sessions backfill pilot='claude'
        let pilot: String = db
            .conn
            .query_row(
                "SELECT pilot FROM collab_sessions WHERE id = ?1",
                ["legacy-v18-session"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            pilot, "claude",
            "legacy sessions must backfill pilot='claude'"
        );
    }

    #[test]
    fn test_pilot_check_constraint_rejects_invalid_values() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);

        // Valid values must be accepted
        let claude_result = db.conn.execute(
            "INSERT INTO collab_sessions (id, repo_path, branch, pilot)
             VALUES (?1, '/repo', 'main', 'claude')",
            ["session-claude"],
        );
        assert!(claude_result.is_ok(), "pilot='claude' should be accepted");

        let codex_result = db.conn.execute(
            "INSERT INTO collab_sessions (id, repo_path, branch, pilot)
             VALUES (?1, '/repo', 'main', 'codex')",
            ["session-codex"],
        );
        assert!(codex_result.is_ok(), "pilot='codex' should be accepted");

        // Invalid values must be rejected by the CHECK constraint
        let invalid_result = db.conn.execute(
            "INSERT INTO collab_sessions (id, repo_path, branch, pilot)
             VALUES (?1, '/repo', 'main', 'invalid')",
            ["session-invalid"],
        );
        assert!(
            invalid_result.is_err(),
            "pilot='invalid' should be rejected by CHECK constraint"
        );
    }

    #[test]
    fn test_migrate_twice_idempotent_v19() {
        let db = Database::open_in_memory().unwrap();
        db.migrate().unwrap();
        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
    }
}

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
const COLLAB_CHECKPOINTS_SQL: &str = include_str!("../../migrations/020_collab_checkpoints.sql");
const CHECKPOINT_ATTESTATION_CHECK_SQL: &str =
    include_str!("../../migrations/021_checkpoint_attestation_check.sql");
const COLLAB_HANDOFF_PROVENANCE_SQL: &str =
    include_str!("../../migrations/022_collab_handoff_provenance.sql");

/// Highest schema version a fully-migrated database reports. Bump alongside the
/// `run_version_gated_migrations` ladder below so `ironmem doctor` can tell a
/// behind-migration database from an up-to-date one.
pub const LATEST_SCHEMA_VERSION: i64 = 22;

/// Total attempts (first try included) `with_transaction` makes when every
/// attempt fails with `SQLITE_BUSY_SNAPSHOT`. See the retry-policy section of
/// [`Database::with_transaction`] for why this is bounded.
const BUSY_SNAPSHOT_MAX_ATTEMPTS: usize = 5;

/// Fixed pause between busy-snapshot retry attempts. A snapshot retry needs a
/// *fresh* snapshot, not a long wait, so a short fixed delay (to let the
/// competing writer finish its commit) is sufficient — no backoff.
const BUSY_SNAPSHOT_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Database wrapper around a SQLite connection.
///
/// `conn` is intentionally restricted to `pub(super)` (visible only within
/// `crate::db`). External callers must go through the `Database` API so that
/// all access is auditable and the single-threaded invariant is enforced at the
/// boundary rather than scattered across the codebase.
pub struct Database {
    pub(super) conn: Connection,
}

/// What [`Database::arm_busy_snapshot_once`] observed on the armed connection.
///
/// Both counters keep running for the connection's lifetime, so a test reads
/// them *after* the production call it is exercising has returned.
#[cfg(test)]
pub(crate) struct BusySnapshotProbe {
    transactions: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    contentions: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl BusySnapshotProbe {
    /// Number of `BEGIN`s issued on the armed connection — one per
    /// [`Database::with_transaction`] attempt, so `2` means the first attempt
    /// failed and was replayed.
    pub(crate) fn transactions_begun(&self) -> usize {
        self.transactions.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Number of contending commits that actually landed. Arming injects at
    /// most one, so `0` means the fixture never fired — the contended call ran
    /// unopposed and the test proved nothing.
    pub(crate) fn contentions_injected(&self) -> usize {
        self.contentions.load(std::sync::atomic::Ordering::SeqCst)
    }
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

    /// Test-only contention fixture: arm this connection so that the **next**
    /// transaction which reads before it writes fails, exactly once, with a
    /// genuine `SQLITE_BUSY_SNAPSHOT` (extended code 517).
    ///
    /// This is the Task 5/6 determinism recipe (two connections on one
    /// file-backed WAL database, staleness engineered by statement ordering on
    /// a single thread — never by sleeps or timing races) generalised so it can
    /// be pointed at a **production** closure, whose body a test cannot edit to
    /// interleave the contending write by hand.
    ///
    /// The interleave point is SQLite's authorizer, which fires while a
    /// statement is being *prepared*. Within one `with_transaction` attempt the
    /// statements run in order, so the first authorizer callback reporting a
    /// write action (`INSERT`/`UPDATE`/`DELETE`) is necessarily raised *after*
    /// the closure's preceding read has already been stepped — i.e. after the
    /// transaction's read snapshot is pinned — and *before* that write is
    /// stepped. Committing the contender's write at exactly that moment
    /// advances the WAL past the pinned snapshot, so the closure's write then
    /// fails the read→write upgrade with `SQLITE_BUSY_SNAPSHOT` immediately
    /// (the busy handler is never consulted for this code — waiting cannot
    /// un-stale a snapshot). The contender fires only once, so the replay that
    /// [`Self::with_transaction`] performs sees a fresh snapshot and commits.
    ///
    /// The returned [`BusySnapshotProbe`] reports what actually happened, so a
    /// test can prove the replay occurred instead of assuming it:
    /// `transactions_begun()` counts `BEGIN`s on this connection (one per
    /// `with_transaction` attempt) and `contentions_injected()` counts
    /// contender commits.
    ///
    /// `path` must be the path this database was opened from. The armed
    /// authorizer owns the contending connection and lives until this
    /// `Database` is dropped.
    ///
    /// The contender writes to a scratch table, so it advances the WAL without
    /// changing anything a test asserts on — the right default when the
    /// property under test is "the replayed closure commits exactly once".
    /// Use [`Self::arm_busy_snapshot_once_with`] when the point is instead that
    /// the replay **re-reads** something: point the contender at a table the
    /// closure's predicate depends on, and a predicate hoisted out of the
    /// transaction stops noticing the change.
    #[cfg(test)]
    pub(crate) fn arm_busy_snapshot_once(
        &self,
        path: &Path,
    ) -> Result<BusySnapshotProbe, MemoryError> {
        self.arm_busy_snapshot_once_with(
            path,
            "INSERT INTO busy_snapshot_contention DEFAULT VALUES",
        )
    }

    /// [`Self::arm_busy_snapshot_once`] with the contending statement chosen by
    /// the caller.
    ///
    /// `contending_sql` runs on a second connection at the interleave point and
    /// must take no parameters. It commits on its own connection, so it sees
    /// only committed state — it cannot observe or depend on the closure's
    /// in-flight transaction.
    #[cfg(test)]
    pub(crate) fn arm_busy_snapshot_once_with(
        &self,
        path: &Path,
        contending_sql: &str,
    ) -> Result<BusySnapshotProbe, MemoryError> {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // Scratch table for the contender: its commit only has to advance the
        // WAL, so it must not touch any table the test asserts on.
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS busy_snapshot_contention (id INTEGER PRIMARY KEY);",
        )?;
        let contender = Connection::open(path)?;
        let contending_sql = contending_sql.to_string();

        let transactions = Arc::new(AtomicUsize::new(0));
        let contentions = Arc::new(AtomicUsize::new(0));
        let transaction_counter = Arc::clone(&transactions);
        let contention_counter = Arc::clone(&contentions);
        let mut armed = true;

        self.conn.authorizer(Some(move |context: AuthContext<'_>| {
            match context.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Begin,
                } => {
                    transaction_counter.fetch_add(1, Ordering::SeqCst);
                }
                AuthAction::Insert { .. }
                | AuthAction::Update { .. }
                | AuthAction::Delete { .. }
                    if armed =>
                {
                    armed = false;
                    // Deliberately not `expect`: this runs inside a SQLite
                    // callback, where a panic would surface as an opaque
                    // authorizer failure. Count only commits that landed, and
                    // let the test's `contentions_injected()` assertion report
                    // a fixture that failed to fire.
                    if contender.execute(&contending_sql, []).is_ok() {
                        contention_counter.fetch_add(1, Ordering::SeqCst);
                    }
                }
                _ => {}
            }
            Authorization::Allow
        }));

        Ok(BusySnapshotProbe {
            transactions,
            contentions,
        })
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

        // v20: first-class collab implementation checkpoints (issue #273).
        // Replaces the `collab-checkpoint:<session_id>` drawer convention with
        // an enforceable table so `implementation_done` can demand proof.
        if current_version < 20 {
            self.conn.execute_batch(COLLAB_CHECKPOINTS_SQL)?;
        }

        // v21: what the server established about an operator attestation's
        // acknowledged range (issue #273 Task 10). One nullable column; NULL
        // means "no verdict recorded", which is every pre-021 row and every
        // implementer-attested row.
        if current_version < 21 {
            self.conn.execute_batch(CHECKPOINT_ATTESTATION_CHECK_SQL)?;
        }

        // v22: provenance for the pending handoff token (issue #298). One
        // NOT NULL column defaulting to 0 — "not minted by the forced path" —
        // which is the answer that selects the strict staleness predicate, so
        // every pre-022 row fails closed. See the migration for why the bit
        // cannot be derived from anything already stored.
        if current_version < 22 {
            self.conn.execute_batch(COLLAB_HANDOFF_PROVENANCE_SQL)?;
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

    /// Execute a closure inside a SQLite transaction and commit on success,
    /// with a bounded retry for `SQLITE_BUSY_SNAPSHOT`.
    ///
    /// # Retry policy
    ///
    /// **Retried:** only `SQLITE_BUSY_SNAPSHOT` (extended result code 517),
    /// detected by [`is_busy_snapshot_error`]. In WAL mode this means a
    /// concurrent connection committed after this transaction pinned its read
    /// snapshot, so the read→write upgrade was refused. Waiting inside the
    /// same transaction is futile (SQLite never invokes the busy handler for
    /// this code — a stale snapshot cannot become fresh), but re-running the
    /// *whole* transaction from `BEGIN` acquires a new snapshot, which is
    /// exactly what each retry attempt does.
    ///
    /// **Never retried:** every other `MemoryError`, including all
    /// semantic/validation failures, `NotFound`, and every other SQLite code
    /// (plain `SQLITE_BUSY`, constraint violations, I/O errors, …). Those
    /// propagate immediately from the first attempt, unretried: a semantic
    /// failure is deterministic — re-running the closure would produce the
    /// same error, waste the delay budget, and mask real bugs behind
    /// "it failed five times" noise.
    ///
    /// **Bound:** a fixed cap of [`BUSY_SNAPSHOT_MAX_ATTEMPTS`] (5) total
    /// attempts with a short fixed sleep between them. The cap is deliberate:
    /// under sustained contention an unbounded loop would hang the caller
    /// invisibly, whereas exhausting the cap surfaces the busy-snapshot error
    /// itself as a clear, diagnosable failure.
    ///
    /// # Why retrying is safe (atomicity)
    ///
    /// Each attempt is a complete `BEGIN`/`COMMIT` against a fresh
    /// transaction. A failed attempt's transaction is dropped uncommitted,
    /// and rusqlite's `Transaction` rolls back on `Drop` — so a failed
    /// attempt leaves **no partial state**: the database after N failed
    /// attempts is byte-for-byte the state before attempt 1. That is what
    /// makes retry-until-success sound — a retry can never double-apply or
    /// half-apply the closure's writes.
    ///
    /// # Closure contract: no non-DB side effects
    ///
    /// Because the closure may run more than once, it must not perform any
    /// side effect outside the database that is unsafe to repeat — no network
    /// calls, no file writes, no mutation of external caches or captured
    /// state that survives a rolled-back attempt. Only its writes *through
    /// the transaction* are undone by rollback. Closures that cannot honor
    /// this contract (non-idempotent external effects, or captures that can
    /// only be consumed once) belong on [`Self::with_transaction_once`], the
    /// documented no-retry opt-out.
    ///
    /// **Nothing enforces that contract.** The `Fn` bound is not a
    /// repeatability check. All it rules out is capture by unique borrow
    /// (`&mut`) and moved-out captures — the *syntactic* shapes that make a
    /// closure literally uncallable a second time. It permits every side
    /// effect that actually matters here, because none of them need a unique
    /// borrow at the capture site: `Cell`/`RefCell`/`Mutex`/`RwLock`/atomic
    /// interior mutability, `Command::new(..).output()`, `std::fs::write`, an
    /// outbound HTTP request. A future closure that bumps an `AtomicU64`,
    /// pushes onto a `RefCell<Vec<_>>`, emits a webhook, or appends to a log
    /// file compiles cleanly under `Fn` and then silently double-applies that
    /// effect the first time a busy-snapshot replay fires — invisible in every
    /// uncontended test run, reproducible only under real concurrency. This
    /// contract is prose, upheld by review; treat this section as its
    /// checklist, not as a description of something the compiler checks.
    ///
    /// ## Known in-tree exceptions
    ///
    /// The tree does **not** honor the contract literally. Three production
    /// closures reach outside the transaction today. All are replay-safe, but
    /// for reasons specific to each — not because the contract held:
    ///
    /// 1. `crates/ironmem/src/mcp/tools/handoff.rs` —
    ///    `ensure_actor_generation_current`, called from inside the caller's
    ///    closure (e.g. `handle_collab_send`), mutates the process-global
    ///    `RwLock<HashMap>` advisory generation cache: it clears an entry
    ///    (`App::clear_cached_generation`) on the cache-ahead-of-DB path, and
    ///    writes a `0` entry (`App::set_cached_generation`) on the
    ///    never-handed-off path. Both survive a rolled-back attempt, so this
    ///    is exactly the "mutation of external caches" the paragraph above
    ///    forbids — the class `GenerationClaim` was introduced to eliminate
    ///    (a *token claim* describes uncommitted DB state and is therefore
    ///    deliberately not cached here; the caller publishes it after commit).
    ///    Replaying them is nevertheless safe because both are idempotent and
    ///    fail-closed. Clearing an absent entry is a no-op, and a dropped
    ///    entry can only make the next check stricter — it falls back to the
    ///    authoritative DB rules (bind at 0 on a never-handed-off session,
    ///    otherwise demand a token), never wider. The `0` entry is written
    ///    only on the `db_active == 0` path, so a surviving `0` can later
    ///    select just two arms: `cached == db_active`, which admits exactly
    ///    when `db_active` is still `0` — the same answer a *missing* entry
    ///    produces via the `db_active == 0` branch — or `cached < db_active`
    ///    once the DB generation advances, which refuses. The `cached >
    ///    db_active` arm is unreachable from a `0` entry because
    ///    `collab_generation_lease` carries a `CHECK (generation >= 0)`. So a
    ///    replay can neither widen access nor desynchronize the cache.
    /// 2. `crates/ironmem/src/mcp/tools/collab_session.rs` —
    ///    `handle_collab_send`'s closure calls
    ///    `validate_global_review_head_advance`, which spawns `git merge-base
    ///    --is-ancestor` via `Command::new("git")` while the write transaction
    ///    is open. The subprocess is read-only, so re-running it is safe in
    ///    the sense that matters for correctness — but it is still a process
    ///    spawn inside a closure this doc calls side-effect-free, it holds the
    ///    transaction open across the blocking `Command::output()`, and the
    ///    retry loop can now re-spawn it up to [`BUSY_SNAPSHOT_MAX_ATTEMPTS`]
    ///    times per request. Worse for contention: in the tokenless case that
    ///    spawn sits exactly between the reads that pin the transaction's
    ///    snapshot and the first write, which is precisely the window a
    ///    competing commit has to invalidate the snapshot. That makes this the
    ///    closure most likely to lose the snapshot race and consume the whole
    ///    retry budget.
    /// 3. `crates/ironmem/src/mcp/tools/collab_session.rs` —
    ///    `ensure_no_conflicting_process_session_tx`, called from inside
    ///    `handle_collab_start`'s and `handle_collab_start_code_review`'s
    ///    closures, mutates the same process-global `RwLock<HashMap>` active-
    ///    session-scope cache as exception 1 above
    ///    (`App::clear_active_collab_session_for_scope_if_matches`), used for
    ///    metrics attribution rather than authorization. Replaying it is safe
    ///    for the same shape of reason as exception 1: the clear is
    ///    conditional on the cached binding still matching the session being
    ///    superseded, so it is a no-op once already cleared, and a clear that
    ///    survives a rolled-back attempt only means the scope's next lookup
    ///    finds no cached binding — the metrics-attribution code path already
    ///    treats an absent binding as "not eligible for implicit
    ///    attribution," never as a wider grant. Worst case is a transient
    ///    attribution miss until a later successful call repopulates the
    ///    binding, not a correctness or authorization gap.
    ///
    /// A fourth exception added without updating this list would be
    /// indistinguishable, at review time, from a closure that honors the
    /// contract — so add it here.
    pub fn with_transaction<T>(
        &self,
        f: impl Fn(&Transaction<'_>) -> Result<T, MemoryError>,
    ) -> Result<T, MemoryError> {
        let mut attempt = 1;
        loop {
            let result: Result<T, MemoryError> = (|| {
                let tx = self.conn.unchecked_transaction()?;
                let value = f(&tx)?;
                tx.commit()?;
                Ok(value)
            })();
            match result {
                Err(error)
                    if is_busy_snapshot_error(&error) && attempt < BUSY_SNAPSHOT_MAX_ATTEMPTS =>
                {
                    // The failed attempt's `Transaction` was dropped above,
                    // rolling it back; the next iteration begins fresh.
                    attempt += 1;
                    std::thread::sleep(BUSY_SNAPSHOT_RETRY_DELAY);
                }
                other => return other,
            }
        }
    }

    /// Execute a closure inside a SQLite transaction and commit on success,
    /// **without any retry**.
    ///
    /// `with_transaction` has a bounded retry loop for
    /// `SQLITE_BUSY_SNAPSHOT` (a concurrent writer invalidated this
    /// transaction's read snapshot), which requires its closure to be safely
    /// callable more than once and so widens its bound from `FnOnce` to
    /// `Fn`. This variant is the escape hatch for closures that cannot meet
    /// that bound: it keeps the original `FnOnce` signature and runs the
    /// closure exactly once, with no retry loop at all.
    ///
    /// Use `with_transaction_once` only for closures that are non-repeatable
    /// *in principle*, not merely inconvenient to rewrite:
    /// - the closure moves a captured value that cannot be cloned or
    ///   reborrowed (so it cannot be called a second time), or
    /// - the closure performs a non-idempotent side effect outside the
    ///   database (e.g. sending a network request, appending to an external
    ///   log) that must not happen twice.
    ///
    /// Every call site must carry a one-line comment explaining which of the
    /// above applies. Choosing this variant forfeits the bounded
    /// `SQLITE_BUSY_SNAPSHOT` retry guarantee that `with_transaction`
    /// provides, so prefer `with_transaction` whenever the closure can be
    /// made `Fn`.
    ///
    /// # Call sites
    ///
    /// **None.** There are currently zero production call sites — but that
    /// absence is hand-maintained, not compiler-established.
    ///
    /// What widening `with_transaction` from `FnOnce` to `Fn` actually
    /// established is narrower than "no closure needs the opt-out":
    /// recompiling the workspace under the new bound surfaced exactly one
    /// closure that failed it (`symbol_graph::index::index_repo`, which
    /// accumulated per-file counts into captured `usize`s — a unique-borrow
    /// capture, i.e. a *syntactic* failure, not a semantic one), and it was
    /// repairable rather than non-repeatable: it now returns its counts so
    /// the caller accumulates them after commit. Every other closure
    /// compiled, which proves only that none of them captures by unique
    /// borrow. It is not a repeatability proof — see the closure-contract
    /// section on [`Self::with_transaction`] for what `Fn` does and does not
    /// rule out, and for the two in-tree closures that do reach outside the
    /// transaction while satisfying the bound. Judging that no closure in the
    /// tree is non-repeatable *in principle* was a human reading of those
    /// bodies, and it stays true only for as long as future changes keep
    /// making it true.
    ///
    /// This list is the single reviewable place the escape-hatch surface can
    /// be audited from: any future call site must be added here with its
    /// path, why the closure is non-repeatable in principle, and why
    /// forfeiting `SQLITE_BUSY_SNAPSHOT` retry is safe there.
    pub fn with_transaction_once<T>(
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

/// True only for `SQLITE_BUSY_SNAPSHOT` (extended result code 517, primary
/// `SQLITE_BUSY`): a WAL-mode read transaction whose snapshot went stale — a
/// concurrent connection committed after this transaction's first read — and
/// which then attempted to upgrade to a writer.
///
/// This is deliberately **narrower** than [`is_busy_error`], which matches any
/// `SQLITE_BUSY`/`SQLITE_LOCKED` primary code and is used only around schema
/// init/migration. `rusqlite::ErrorCode` cannot distinguish busy-snapshot from
/// plain busy (both map to `DatabaseBusy`), so this predicate matches on the
/// raw `extended_code` instead. Busy-snapshot is the one busy flavor where
/// waiting is futile but *re-running the whole transaction* is guaranteed to
/// see a fresh snapshot — which is exactly what the bounded retry loop in
/// `with_transaction` needs to detect.
///
/// The match is on [`MemoryError::Db`] specifically, so the retry is reachable
/// only while the underlying `rusqlite::Error` is still carried in that
/// variant. A write path that rewraps its SQLite failure on the way out —
/// `.map_err(|e| MemoryError::Validation(format!("...{e}")))`, or a rewrap into
/// `Internal`/`Migration` — makes its busy-snapshots invisible to this
/// predicate, silently forfeits the retry for that path, and surfaces a
/// transient snapshot collision to the MCP client as a hard failure. Today's
/// write helpers propagate with bare `?` or `.map_err(MemoryError::from)` and
/// so keep the variant, but nothing enforces that: when adding a `map_err` to
/// anything that runs inside `with_transaction`, preserve `MemoryError::Db`
/// for genuine SQLite failures and rewrap only non-SQLite ones.
fn is_busy_snapshot_error(error: &MemoryError) -> bool {
    matches!(
        error,
        MemoryError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                extended_code: rusqlite::ffi::SQLITE_BUSY_SNAPSHOT,
                ..
            },
            _,
        ))
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

    /// Insert a `collab_sessions` row using only the pre-019 column set (no
    /// `pilot`). `queue::create_session` cannot be used by the legacy-row
    /// migration tests below because it now always writes `pilot`
    /// (migration 019), which does not exist on the pinned pre-v19 schemas
    /// (`open_at_v8`/`open_at_v13`/`open_at_v14`/`open_at_v15`/`open_at_v18`)
    /// those tests deliberately construct to verify migration preserves
    /// legacy rows.
    fn insert_legacy_collab_session_pre_pilot(
        conn: &rusqlite::Connection,
        id: &str,
        repo_path: &str,
        branch: &str,
        task: Option<&str>,
        implementer: &str,
    ) {
        conn.execute(
            "INSERT INTO collab_sessions (id, repo_path, branch, task, implementer)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, repo_path, branch, task, implementer],
        )
        .unwrap();
    }

    #[test]
    fn latest_schema_version_matches_highest_migration() {
        // The exported constant must track the highest migration a fresh,
        // fully-migrated database reports — doctor compares against it.
        let db = Database::open_in_memory().unwrap();
        assert_eq!(LATEST_SCHEMA_VERSION, db.schema_version().unwrap());
        assert_eq!(LATEST_SCHEMA_VERSION, 22);
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
        insert_legacy_collab_session_pre_pilot(
            &db.conn,
            "legacy-session",
            "/repo",
            "main",
            Some("legacy task"),
            "claude",
        );

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

        insert_legacy_collab_session_pre_pilot(
            &db.conn,
            "legacy-v13-session",
            "/repo",
            "main",
            Some("legacy task"),
            "claude",
        );
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

        insert_legacy_collab_session_pre_pilot(
            &db.conn,
            "legacy-v14-session",
            "/repo",
            "main",
            Some("legacy task"),
            "codex",
        );

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

        insert_legacy_collab_session_pre_pilot(
            &db.conn,
            "legacy-v15-message-session",
            "/repo",
            "main",
            None,
            "claude",
        );
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

    /// Build a connection migrated to exactly v19 (no collab_checkpoints
    /// table yet) by replaying migrations 001-019 directly from the module
    /// consts.
    fn open_at_v19() -> Database {
        let db = open_at_v18();
        db.conn.execute_batch(COLLAB_PILOT_SQL).unwrap();
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

        insert_legacy_collab_session_pre_pilot(
            &db.conn,
            "legacy-v18-session",
            "/repo",
            "main",
            Some("legacy task"),
            "claude",
        );

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

    /// Deterministic contention harness: produce a genuine
    /// `SQLITE_BUSY_SNAPSHOT` (extended code 517) from a real WAL-mode
    /// database, with no sleeps and no threads.
    ///
    /// The staleness is engineered purely by statement ordering on two
    /// connections driven from the one test thread:
    /// 1. conn A opens a `BEGIN DEFERRED` transaction and reads — this pins
    ///    A's read snapshot without taking any write lock;
    /// 2. conn B commits a write, advancing the WAL past A's snapshot;
    /// 3. conn A then attempts its first write. Its snapshot is now stale, so
    ///    SQLite refuses the read→write upgrade with `SQLITE_BUSY_SNAPSHOT`
    ///    immediately (the busy handler is never invoked for this code —
    ///    waiting can't un-stale a snapshot), making the failure
    ///    deterministic rather than timing-dependent.
    fn produce_busy_snapshot_error() -> rusqlite::Error {
        let dir = tempfile::tempdir().unwrap();
        // WAL mode requires a real file-backed database, not `:memory:`.
        let db_path = dir.path().join("busy_snapshot.db");

        let conn_a = rusqlite::Connection::open(&db_path).unwrap();
        conn_a
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE snapshot_probe (id INTEGER PRIMARY KEY, v INTEGER NOT NULL);
                 INSERT INTO snapshot_probe (v) VALUES (0);",
            )
            .unwrap();
        let mode: String = conn_a
            .query_row("PRAGMA journal_mode;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal", "harness requires WAL mode to be engaged");

        let conn_b = rusqlite::Connection::open(&db_path).unwrap();

        // Step 1: pin conn A's read snapshot (DEFERRED takes no lock until
        // the first write; the SELECT starts the read transaction).
        conn_a.execute_batch("BEGIN DEFERRED").unwrap();
        let count: i64 = conn_a
            .query_row("SELECT COUNT(*) FROM snapshot_probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Step 2: conn B commits a write, so the WAL moves past A's snapshot.
        conn_b
            .execute("INSERT INTO snapshot_probe (v) VALUES (1)", [])
            .unwrap();

        // Step 3: conn A's first write inside its still-open transaction must
        // now fail the snapshot upgrade.
        conn_a
            .execute("INSERT INTO snapshot_probe (v) VALUES (2)", [])
            .expect_err("stale-snapshot write upgrade must fail with SQLITE_BUSY_SNAPSHOT")
    }

    #[test]
    fn harness_reproduces_busy_snapshot_extended_code() {
        // The harness must yield exactly extended code 517 — not plain
        // SQLITE_BUSY — or every consumer test below proves nothing.
        let error = produce_busy_snapshot_error();
        match &error {
            rusqlite::Error::SqliteFailure(ffi_error, _) => {
                assert_eq!(
                    ffi_error.extended_code,
                    rusqlite::ffi::SQLITE_BUSY_SNAPSHOT,
                    "expected extended code 517 (SQLITE_BUSY_SNAPSHOT), got {error:?}"
                );
                assert_eq!(ffi_error.code, rusqlite::ErrorCode::DatabaseBusy);
            }
            other => panic!("expected SqliteFailure, got {other:?}"),
        }
    }

    #[test]
    fn is_busy_snapshot_error_true_for_genuine_busy_snapshot() {
        let error = MemoryError::from(produce_busy_snapshot_error());
        assert!(is_busy_snapshot_error(&error));
    }

    #[test]
    fn is_busy_snapshot_error_false_for_plain_busy_and_locked() {
        // Plain SQLITE_BUSY (5) and SQLITE_LOCKED (6) satisfy the broad
        // `is_busy_error` predicate but must NOT satisfy the snapshot-specific
        // one: extended code 517 is the only accepted value.
        let plain_busy = MemoryError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        ));
        assert!(!is_busy_snapshot_error(&plain_busy));

        let plain_locked = MemoryError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            None,
        ));
        assert!(!is_busy_snapshot_error(&plain_locked));
    }

    #[test]
    fn is_busy_snapshot_error_false_for_semantic_error() {
        let semantic = MemoryError::Validation("not a database error".into());
        assert!(!is_busy_snapshot_error(&semantic));
    }

    // ---- `with_transaction` bounded busy-snapshot retry (Task 6) ----

    /// Task 6 extension of the Task 5 harness above: instead of producing one
    /// bare `SQLITE_BUSY_SNAPSHOT` error, this sets up a file-backed WAL
    /// [`Database`] plus a second "contender" connection so a test can dictate
    /// *per attempt* whether `with_transaction`'s closure hits a genuine
    /// busy-snapshot failure. Same determinism recipe as
    /// [`produce_busy_snapshot_error`] — staleness is engineered purely by
    /// statement ordering on one thread, never by sleeps or timing races.
    ///
    /// The caller must retain the returned `TempDir` for the test's lifetime.
    fn retry_harness() -> (tempfile::TempDir, Database, rusqlite::Connection) {
        let dir = tempfile::tempdir().unwrap();
        // WAL mode requires a real file-backed database, not `:memory:`.
        let db_path = dir.path().join("retry_probe.db");
        let db = Database::open(&db_path).unwrap();
        db.exec_raw("CREATE TABLE retry_probe (id INTEGER PRIMARY KEY, v INTEGER NOT NULL);")
            .unwrap();
        let contender = rusqlite::Connection::open(&db_path).unwrap();
        (dir, db, contender)
    }

    /// Drive `with_transaction` so its first `fail_first_n` attempts fail with
    /// a genuine `SQLITE_BUSY_SNAPSHOT` and every later attempt succeeds.
    ///
    /// Each attempt of the closure:
    /// 1. increments `attempts` (so the test can count invocations),
    /// 2. pins the attempt's read snapshot with a `SELECT` (the transaction is
    ///    `BEGIN DEFERRED`, so no lock is taken until the first write),
    /// 3. if this attempt is still within `fail_first_n`, has the contender
    ///    connection commit a write — advancing the WAL past the snapshot
    ///    pinned in step 2, so
    /// 4. the closure's own `INSERT` (value 100) deterministically fails the
    ///    read→write upgrade with extended code 517. Past `fail_first_n`, the
    ///    contender stays quiet and the same `INSERT` succeeds.
    fn run_retry_scenario(
        db: &Database,
        contender: &rusqlite::Connection,
        fail_first_n: usize,
        attempts: &std::cell::Cell<usize>,
    ) -> Result<(), MemoryError> {
        db.with_transaction(|tx| {
            let attempt = attempts.get() + 1;
            attempts.set(attempt);
            let _count: i64 =
                tx.query_row("SELECT COUNT(*) FROM retry_probe", [], |row| row.get(0))?;
            if attempt <= fail_first_n {
                contender.execute("INSERT INTO retry_probe (v) VALUES (?1)", [attempt as i64])?;
            }
            tx.execute("INSERT INTO retry_probe (v) VALUES (100)", [])?;
            Ok(())
        })
    }

    #[test]
    fn with_transaction_retry_exhaustion_propagates_busy_snapshot() {
        // (i) Exhaustion: the contender wins every attempt. After the fixed
        // cap of 5 attempts the busy-snapshot error must propagate, and the
        // closure must have run exactly 5 times (bounded, not infinite).
        let (_dir, db, contender) = retry_harness();
        let attempts = std::cell::Cell::new(0usize);

        let result = run_retry_scenario(&db, &contender, usize::MAX, &attempts);

        let error = result.expect_err("all attempts stale — busy snapshot must propagate");
        assert!(
            is_busy_snapshot_error(&error),
            "exhaustion must surface the busy-snapshot error itself, got {error:?}"
        );
        assert_eq!(attempts.get(), 5, "retry must stop at the 5-attempt cap");

        // No attempt may have left partial state behind: every failed attempt
        // rolled back, so no closure INSERT (v=100) ever committed.
        let committed: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM retry_probe WHERE v = 100",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(committed, 0, "failed attempts must leave no partial state");
    }

    #[test]
    fn with_transaction_retry_succeeds_after_transient_contention() {
        // (ii) Success after N: the contender wins the first 2 attempts and
        // then stays quiet; attempt 3 must succeed and the committed state
        // must reflect exactly ONE application of the closure.
        let (_dir, db, contender) = retry_harness();
        let attempts = std::cell::Cell::new(0usize);

        run_retry_scenario(&db, &contender, 2, &attempts)
            .expect("attempt 3 sees a fresh snapshot and must succeed");
        assert_eq!(attempts.get(), 3, "2 stale attempts + 1 successful attempt");

        // Exactly one committed closure write — the two rolled-back attempts
        // must not have double-applied it.
        let committed: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM retry_probe WHERE v = 100",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            committed, 1,
            "committed state must reflect exactly one application of the closure"
        );

        // Sanity: both contender commits (v=1, v=2) are present.
        let contender_rows: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM retry_probe WHERE v < 100",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(contender_rows, 2);
    }

    #[test]
    fn with_transaction_once_propagates_busy_snapshot_without_replay() {
        // The non-retryable escape hatch must preserve its defining promise:
        // even a genuine SQLITE_BUSY_SNAPSHOT invokes an FnOnce closure once,
        // returns that error unchanged, and rolls back its attempted write.
        let (_dir, db, contender) = retry_harness();
        let attempts = std::cell::Cell::new(0usize);

        let result: Result<(), _> = db.with_transaction_once(|tx| {
            attempts.set(attempts.get() + 1);
            let _count: i64 =
                tx.query_row("SELECT COUNT(*) FROM retry_probe", [], |row| row.get(0))?;
            contender.execute("INSERT INTO retry_probe (v) VALUES (1)", [])?;
            tx.execute("INSERT INTO retry_probe (v) VALUES (100)", [])?;
            Ok(())
        });

        let error = result.expect_err("the stale snapshot must propagate without retry");
        assert!(
            is_busy_snapshot_error(&error),
            "the original busy-snapshot error must propagate, got {error:?}"
        );
        assert_eq!(attempts.get(), 1, "FnOnce closure must run exactly once");
        let committed: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM retry_probe WHERE v = 100",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(committed, 0, "the failed one-shot attempt must roll back");
    }

    #[test]
    fn with_transaction_retry_never_retries_semantic_error() {
        // (iii) Negative: a semantic error is not a busy snapshot — the
        // closure must run exactly once and the error must propagate
        // unwrapped, on the first attempt.
        let db = Database::open_in_memory().unwrap();
        let calls = std::cell::Cell::new(0usize);

        let result: Result<(), _> = db.with_transaction(|_tx| {
            calls.set(calls.get() + 1);
            Err(MemoryError::Validation("semantic failure".into()))
        });

        match result {
            Err(MemoryError::Validation(msg)) => assert_eq!(msg, "semantic failure"),
            other => panic!("expected the Validation error unwrapped, got {other:?}"),
        }
        assert_eq!(
            calls.get(),
            1,
            "a non-busy-snapshot error must never trigger a retry"
        );
    }

    // ---- Migration 020 (collab_checkpoints table) coverage ----

    #[test]
    fn migration_020_creates_collab_checkpoints_table() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        assert_eq!(LATEST_SCHEMA_VERSION, 22);

        // The table exists and carries every column the checkpoint contract
        // needs, at the declared affinity the header argues for. Asserting the
        // type alongside the name is what keeps the affinity decisions
        // load-bearing: redeclaring `updated_at` as
        // `TEXT NOT NULL DEFAULT (datetime('now'))` — the exact change the
        // header spends a paragraph arguing against — passes a name-only check.
        let columns: Vec<(String, String)> = db
            .conn
            .prepare("SELECT name, type FROM pragma_table_info('collab_checkpoints')")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();

        for (expected, expected_type) in [
            ("session_id", "TEXT"),
            ("task_id", "INTEGER"),
            ("task_title", "TEXT"),
            ("status", "TEXT"),
            ("head_sha", "TEXT"),
            ("commit_sha", "TEXT"),
            ("completed_task_ids", "TEXT"),
            ("next_task_id", "INTEGER"),
            ("gates_result", "TEXT"),
            ("gates_sha", "TEXT"),
            ("gates_commands", "TEXT"),
            ("summary", "TEXT"),
            ("attested_by", "TEXT"),
            ("acknowledged_divergence", "TEXT"),
            // Added by migration 021, not 020. Asserted here rather than in a
            // separate test because the contract this list states is "every
            // column the checkpoint contract needs" — a reader checking whether
            // the verdict is persisted at all should find the answer in one
            // place. `test_v20_to_v21_adds_attestation_check` is what pins the
            // upgrade path itself.
            ("attestation_check", "TEXT"),
            // The one integer timestamp in the schema, deliberately not the
            // TEXT/datetime('now') convention the other `_at` columns use.
            ("updated_at", "INTEGER"),
        ] {
            let actual = columns.iter().find(|(name, _)| name == expected);
            let Some((_, actual_type)) = actual else {
                panic!("collab_checkpoints is missing column {expected:?}; has {columns:?}");
            };
            assert_eq!(
                actual_type, expected_type,
                "collab_checkpoints.{expected} must be declared {expected_type}, got {actual_type}"
            );
        }
    }

    #[test]
    fn migration_020_round_trips_every_optional_checkpoint_column() {
        let db = Database::open_in_memory().unwrap();
        db.conn
            .execute(
                "INSERT INTO collab_sessions (id, repo_path, branch) VALUES ('s-round', '/repo', 'main')",
                [],
            )
            .unwrap();

        // Every other migration-020 test leaves the optional columns NULL by
        // omission and only ever observes `gates_result` at its default, so a
        // regression that narrowed one of them — a stray NOT NULL on
        // `gates_sha`, a copy-paste `CHECK (commit_sha IS NULL)`, a wrong
        // affinity — would redden nothing. This writes a fully-populated
        // `completed` checkpoint and reads every value back unchanged.
        db.conn
            .execute(
                "INSERT INTO collab_checkpoints
                    (session_id, task_id, task_title, status, head_sha, commit_sha,
                     completed_task_ids, next_task_id, gates_result, gates_sha,
                     gates_commands, summary, attested_by, updated_at)
                 VALUES ('s-round', 3, 'Wire the gate', 'completed', 'a1b2c3', 'd4e5f6',
                         '1,2,3', 4, 'failed: clippy', 'a1b2c3',
                         'cargo fmt --all -- --check && cargo test --workspace',
                         'Task 3 done', 'implementer', 1750000000)",
                [],
            )
            .unwrap();

        // Read back column by column. Rust implements `PartialEq`/`Debug` only
        // up to 12-element tuples and the row has 13 meaningful columns, so a
        // single tuple comparison will not build — and naming each column here
        // makes a failure say which one drifted rather than dumping the row.
        let text_columns = [
            ("task_title", "Wire the gate"),
            ("status", "completed"),
            ("head_sha", "a1b2c3"),
            ("commit_sha", "d4e5f6"),
            ("completed_task_ids", "1,2,3"),
            // Free text after the `failed: ` prefix: the round-trip is what
            // pins `gates_result` as deliberately not CHECK-constrained to a
            // closed vocabulary.
            ("gates_result", "failed: clippy"),
            ("gates_sha", "a1b2c3"),
            (
                "gates_commands",
                "cargo fmt --all -- --check && cargo test --workspace",
            ),
            ("summary", "Task 3 done"),
            ("attested_by", "implementer"),
        ];
        for (column, expected) in text_columns {
            let actual: String = db
                .conn
                .query_row(
                    &format!(
                        "SELECT {column} FROM collab_checkpoints WHERE session_id = 's-round'"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                actual, expected,
                "collab_checkpoints.{column} must round-trip unchanged"
            );
        }

        let integer_columns = [
            ("task_id", 3_i64),
            ("next_task_id", 4),
            ("updated_at", 1_750_000_000),
        ];
        for (column, expected) in integer_columns {
            let actual: i64 = db
                .conn
                .query_row(
                    &format!(
                        "SELECT {column} FROM collab_checkpoints WHERE session_id = 's-round'"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                actual, expected,
                "collab_checkpoints.{column} must round-trip unchanged"
            );
        }

        // Affinity, not just value: SQLite's weak typing would happily store a
        // stringified timestamp in an INTEGER column, and such a value compares
        // greater than every real unix second.
        let typeof_updated_at: String = db
            .conn
            .query_row(
                "SELECT typeof(updated_at) FROM collab_checkpoints WHERE session_id = 's-round'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            typeof_updated_at, "integer",
            "updated_at must store an integer, not a stringified timestamp"
        );
    }

    #[test]
    fn migration_020_enforces_one_current_checkpoint_per_session() {
        let db = Database::open_in_memory().unwrap();
        db.conn
            .execute_batch(
                "INSERT INTO collab_sessions (id, phase, current_owner, repo_path, branch)
                 VALUES ('s1', 'CodeImplementPending', 'claude', '/repo', 'main');",
            )
            .unwrap();

        let insert = "INSERT INTO collab_checkpoints
            (session_id, status, head_sha, completed_task_ids, attested_by, updated_at)
            VALUES ('s1', 'started', 'aaa', '', 'implementer', 1)";
        db.conn.execute(insert, []).unwrap();

        // session_id is the primary key: a second plain INSERT must conflict
        // rather than accumulate a second 'current' row.
        let err = db.conn.execute(insert, []).unwrap_err();
        assert!(
            err.to_string().contains("UNIQUE"),
            "expected a UNIQUE violation on the second insert, got: {err}"
        );
    }

    #[test]
    fn migration_020_requires_session_id_status_head_sha_and_updated_at() {
        let db = Database::open_in_memory().unwrap();
        // A fresh session per case, so a primary-key collision can never
        // masquerade as the NOT NULL constraint firing.
        for session_id in [
            "s-req-status",
            "s-req-head",
            "s-req-updated",
            "s-req-defaults",
        ] {
            db.conn
                .execute(
                    "INSERT INTO collab_sessions (id, repo_path, branch) VALUES (?1, '/repo', 'main')",
                    [session_id],
                )
                .unwrap();
        }

        // session_id: plain SQLite does not imply NOT NULL for a `TEXT PRIMARY
        // KEY`, so the explicit NOT NULL is the only thing keeping a NULL-keyed
        // row — unreachable by lookup and by FK cascade alike — out of the table.
        let null_session_id = db.conn.execute(
            "INSERT INTO collab_checkpoints (session_id, status, head_sha, updated_at)
             VALUES (NULL, 'started', 'aaa', 1)",
            [],
        );
        assert!(
            null_session_id.is_err(),
            "session_id must be NOT NULL, but a NULL-keyed checkpoint was accepted"
        );

        // status: NOT NULL is the *only* thing rejecting a NULL status here.
        // The `CHECK (status IN (...))` beside it gives no NULL protection at
        // all, because `NULL IN (...)` evaluates to NULL and SQLite treats a
        // CHECK that evaluates to NULL as satisfied.
        let missing_status = db.conn.execute(
            "INSERT INTO collab_checkpoints (session_id, head_sha, updated_at)
             VALUES ('s-req-status', 'aaa', 1)",
            [],
        );
        assert!(
            missing_status.is_err(),
            "status must be NOT NULL, but a checkpoint without one was accepted"
        );

        // head_sha: the column the whole issue turns on. A checkpoint without
        // one cannot be compared against live git HEAD at all.
        let missing_head_sha = db.conn.execute(
            "INSERT INTO collab_checkpoints (session_id, status, updated_at)
             VALUES ('s-req-head', 'started', 1)",
            [],
        );
        assert!(
            missing_head_sha.is_err(),
            "head_sha must be NOT NULL, but a checkpoint without one was accepted"
        );

        // updated_at: NOT NULL with deliberately no DEFAULT, so a writer that
        // forgot to stamp it fails loudly instead of getting a silently-filled
        // timestamp. That property is asserted here, not just in the comment.
        let missing_updated_at = db.conn.execute(
            "INSERT INTO collab_checkpoints (session_id, status, head_sha)
             VALUES ('s-req-updated', 'started', 'aaa')",
            [],
        );
        assert!(
            missing_updated_at.is_err(),
            "updated_at must be NOT NULL with no DEFAULT, but an unstamped checkpoint was accepted"
        );

        // With only the required columns supplied the insert succeeds, and the
        // NOT NULL DEFAULTs fill in the documented values rather than NULL.
        // Reading them back as `String` also fails loudly if a DEFAULT is lost.
        db.conn
            .execute(
                "INSERT INTO collab_checkpoints (session_id, status, head_sha, updated_at)
                 VALUES ('s-req-defaults', 'started', 'aaa', 1)",
                [],
            )
            .unwrap();
        let (completed_task_ids, gates_result, attested_by): (String, String, String) = db
            .conn
            .query_row(
                "SELECT completed_task_ids, gates_result, attested_by
                 FROM collab_checkpoints WHERE session_id = 's-req-defaults'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            completed_task_ids, "",
            "completed_task_ids must default to the empty string, not NULL"
        );
        assert_eq!(
            gates_result, "not_run",
            "gates_result must default to 'not_run', not NULL"
        );
        assert_eq!(
            attested_by, "implementer",
            "attested_by must default to 'implementer', not NULL"
        );
    }

    #[test]
    fn migration_020_status_check_constraint_accepts_known_rejects_unknown() {
        let db = Database::open_in_memory().unwrap();

        for (i, status) in ["started", "completed", "blocked", "batch_complete"]
            .iter()
            .enumerate()
        {
            let session_id = format!("s-status-{i}");
            db.conn
                .execute(
                    "INSERT INTO collab_sessions (id, repo_path, branch) VALUES (?1, '/repo', 'main')",
                    [&session_id],
                )
                .unwrap();
            let result = db.conn.execute(
                "INSERT INTO collab_checkpoints
                    (session_id, status, head_sha, completed_task_ids, updated_at)
                 VALUES (?1, ?2, 'aaa', '', 1)",
                rusqlite::params![session_id, status],
            );
            assert!(
                result.is_ok(),
                "status={status:?} should be accepted, got {result:?}"
            );
        }

        db.conn
            .execute(
                "INSERT INTO collab_sessions (id, repo_path, branch) VALUES ('s-status-bad', '/repo', 'main')",
                [],
            )
            .unwrap();
        let bad = db.conn.execute(
            "INSERT INTO collab_checkpoints
                (session_id, status, head_sha, completed_task_ids, updated_at)
             VALUES ('s-status-bad', 'bogus', 'aaa', '', 1)",
            [],
        );
        assert!(
            bad.is_err(),
            "status='bogus' should be rejected by the CHECK constraint"
        );
    }

    #[test]
    fn migration_020_attested_by_check_constraint_accepts_known_rejects_unknown() {
        let db = Database::open_in_memory().unwrap();

        for (i, attested_by) in ["implementer", "operator"].iter().enumerate() {
            let session_id = format!("s-attested-{i}");
            db.conn
                .execute(
                    "INSERT INTO collab_sessions (id, repo_path, branch) VALUES (?1, '/repo', 'main')",
                    [&session_id],
                )
                .unwrap();
            let result = db.conn.execute(
                "INSERT INTO collab_checkpoints
                    (session_id, status, head_sha, completed_task_ids, attested_by, updated_at)
                 VALUES (?1, 'started', 'aaa', '', ?2, 1)",
                rusqlite::params![session_id, attested_by],
            );
            assert!(
                result.is_ok(),
                "attested_by={attested_by:?} should be accepted, got {result:?}"
            );
        }

        db.conn
            .execute(
                "INSERT INTO collab_sessions (id, repo_path, branch) VALUES ('s-attested-bad', '/repo', 'main')",
                [],
            )
            .unwrap();
        let bad = db.conn.execute(
            "INSERT INTO collab_checkpoints
                (session_id, status, head_sha, completed_task_ids, attested_by, updated_at)
             VALUES ('s-attested-bad', 'started', 'aaa', '', 'bogus', 1)",
            [],
        );
        assert!(
            bad.is_err(),
            "attested_by='bogus' should be rejected by the CHECK constraint"
        );
    }

    #[test]
    fn migration_020_correlation_check_rejects_implementer_with_divergence() {
        let db = Database::open_in_memory().unwrap();
        // A fresh session per case: sharing one session would make the
        // positive case depend on the negative case having failed first and
        // left the primary key free, so a regressed CHECK would surface as a
        // UNIQUE violation pointing at the wrong constraint.
        for session_id in ["s-corr", "s-corr-ok"] {
            db.conn
                .execute(
                    "INSERT INTO collab_sessions (id, repo_path, branch) VALUES (?1, '/repo', 'main')",
                    [session_id],
                )
                .unwrap();
        }

        // attested_by='implementer' can never carry acknowledged_divergence.
        let bad = db.conn.execute(
            "INSERT INTO collab_checkpoints
                (session_id, status, head_sha, completed_task_ids, attested_by,
                 acknowledged_divergence, updated_at)
             VALUES ('s-corr', 'started', 'aaa', '', 'implementer', 'b9c2ce0..75a4ea3', 1)",
            [],
        );
        assert!(
            bad.is_err(),
            "attested_by='implementer' with acknowledged_divergence set must be rejected"
        );

        // attested_by='operator' with a divergence range set is exactly the
        // human-attested-backfill case the column exists for.
        let ok = db.conn.execute(
            "INSERT INTO collab_checkpoints
                (session_id, status, head_sha, completed_task_ids, attested_by,
                 acknowledged_divergence, updated_at)
             VALUES ('s-corr-ok', 'started', 'aaa', '', 'operator', 'b9c2ce0..75a4ea3', 1)",
            [],
        );
        assert!(
            ok.is_ok(),
            "attested_by='operator' with acknowledged_divergence set should be accepted"
        );
    }

    #[test]
    fn migration_020_rejects_checkpoint_for_nonexistent_session() {
        let db = Database::open_in_memory().unwrap();

        // The sibling cascade test covers the delete-time half of the foreign
        // key; this covers the insert-time half. Dropping `REFERENCES`
        // entirely reddens both, but dropping only `ON DELETE CASCADE` reddens
        // only the cascade test, so the pair says *which* half regressed
        // rather than just that something did.
        let orphan = db.conn.execute(
            "INSERT INTO collab_checkpoints
                (session_id, status, head_sha, completed_task_ids, updated_at)
             VALUES ('no-such-session', 'started', 'aaa', '', 1)",
            [],
        );
        assert!(
            orphan.is_err(),
            "a checkpoint naming a session that does not exist must be rejected \
             by the foreign key, got {orphan:?}"
        );
    }

    #[test]
    fn migration_020_deleting_session_cascades_to_checkpoint() {
        let db = Database::open_in_memory().unwrap();
        // Two independent sessions, each with its own checkpoint. With only
        // one session in the table a cascade scoped to the deleted row and an
        // over-broad `DELETE FROM collab_checkpoints` with no predicate are
        // indistinguishable — both leave zero rows. The bystander is what
        // makes this test able to tell them apart.
        for session_id in ["s-cascade", "s-bystander"] {
            db.conn
                .execute(
                    "INSERT INTO collab_sessions (id, repo_path, branch) VALUES (?1, '/repo', 'main')",
                    [session_id],
                )
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO collab_checkpoints
                        (session_id, status, head_sha, completed_task_ids, updated_at)
                     VALUES (?1, 'started', 'aaa', '', 1)",
                    [session_id],
                )
                .unwrap();
        }

        db.conn
            .execute("DELETE FROM collab_sessions WHERE id = 's-cascade'", [])
            .unwrap();

        let remaining: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM collab_checkpoints WHERE session_id = 's-cascade'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            remaining, 0,
            "ON DELETE CASCADE must remove the checkpoint when its session is deleted"
        );

        let bystander: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM collab_checkpoints WHERE session_id = 's-bystander'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            bystander, 1,
            "the cascade must be scoped to the deleted session; another session's \
             checkpoint must survive"
        );
    }

    fn open_at_v20() -> Database {
        let db = open_at_v19();
        db.conn.execute_batch(COLLAB_CHECKPOINTS_SQL).unwrap();
        db
    }

    fn open_at_v21() -> Database {
        let db = open_at_v20();
        db.conn
            .execute_batch(CHECKPOINT_ATTESTATION_CHECK_SQL)
            .unwrap();
        db
    }

    /// The upgrade path for migration 022, and — more importantly — the
    /// direction its default points.
    ///
    /// `pending_handoff_forced` gates which staleness predicate
    /// `session_handoff { force_reissue: true }` applies: `1` selects the
    /// narrowed one that ignores the agent's own `pending_handoff_issued_at`,
    /// `0` selects the full five-signal read that refuses on a live session. A
    /// lease row that predates this migration has no provenance to report, and
    /// the whole design rests on that unknown reading as **not forced** — the
    /// strict answer. If the default were ever flipped to 1, every pre-022 row
    /// on disk would silently become eligible for the narrowed gate, which is
    /// the lease-takeover hole this column exists to close.
    #[test]
    fn test_v21_to_v22_adds_handoff_provenance_defaulting_to_not_forced() {
        let db = open_at_v21();
        assert_eq!(schema_version_of(&db), 21);
        assert!(
            !column_exists(&db, "collab_actor_generations", "pending_handoff_forced"),
            "pending_handoff_forced should not exist at v21"
        );

        // A pre-022 lease row with a token pending, written the way v21 wrote
        // them — with no provenance column to write.
        db.conn
            .execute(
                "INSERT INTO collab_sessions (id, repo_path, branch)
                 VALUES ('legacy-v21-session', '/repo', 'main')",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO collab_actor_generations
                     (session_id, agent, generation, pending_handoff_token,
                      pending_handoff_generation, pending_handoff_issued_at)
                 VALUES ('legacy-v21-session', 'claude', 3, 'legacy-token', 4,
                         datetime('now'))",
                [],
            )
            .unwrap();

        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        assert!(column_exists(
            &db,
            "collab_actor_generations",
            "pending_handoff_forced"
        ));

        let (token, forced): (String, i64) = db
            .conn
            .query_row(
                "SELECT pending_handoff_token, pending_handoff_forced
                   FROM collab_actor_generations
                  WHERE session_id = 'legacy-v21-session' AND agent = 'claude'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(token, "legacy-token", "the pre-existing row must survive");
        assert_eq!(
            forced, 0,
            "a row that predates provenance must read NOT forced — the strict answer. \
             Flipping this default would make every legacy lease row eligible for the \
             narrowed staleness gate."
        );

        // The CHECK is what stops a third value appearing and being read as
        // truthy by anything that compares against 0.
        let bad = db.conn.execute(
            "UPDATE collab_actor_generations SET pending_handoff_forced = 2
              WHERE session_id = 'legacy-v21-session'",
            [],
        );
        assert!(
            bad.is_err(),
            "pending_handoff_forced must be CHECK-constrained to 0 or 1"
        );
    }

    /// The upgrade path for migration 021, and the reason it is a separate
    /// migration rather than an edit to 020: a database can already report
    /// schema_version 20, and 020's `CREATE TABLE IF NOT EXISTS` would silently
    /// skip such a database — leaving a table this code then queries a missing
    /// column on. Every checkpoint read and write would fail at runtime.
    ///
    /// The pre-existing row must survive with a NULL verdict, which is what
    /// makes "no verdict recorded" a real state rather than a theoretical one,
    /// and why `CollabCheckpoint::attestation_verdict` renders NULL on an
    /// operator row as unchecked.
    #[test]
    fn test_v20_to_v21_adds_attestation_check_and_preserves_legacy_checkpoints() {
        let db = open_at_v20();
        assert_eq!(schema_version_of(&db), 20);
        assert!(
            !column_exists(&db, "collab_checkpoints", "attestation_check"),
            "attestation_check should not exist at v20"
        );

        db.conn
            .execute(
                "INSERT INTO collab_sessions (id, repo_path, branch)
                 VALUES ('legacy-v20-session', '/repo', 'main')",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO collab_checkpoints
                     (session_id, status, head_sha, attested_by,
                      acknowledged_divergence, updated_at)
                 VALUES ('legacy-v20-session', 'started', 'aaa111', 'operator',
                         'aaa000..aaa111', 1760000000)",
                [],
            )
            .unwrap();

        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        assert!(column_exists(
            &db,
            "collab_checkpoints",
            "attestation_check"
        ));

        let (survived, verdict): (i64, Option<String>) = db
            .conn
            .query_row(
                "SELECT COUNT(*), MAX(attestation_check) FROM collab_checkpoints
                 WHERE session_id = 'legacy-v20-session'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            survived, 1,
            "the pre-021 checkpoint must survive the upgrade"
        );
        assert_eq!(
            verdict, None,
            "a pre-021 operator attestation carries no verdict, and must read as \
             unchecked rather than be back-filled with one the server never made"
        );

        // The vocabulary CHECK is live on the added column.
        let bogus = db.conn.execute(
            "UPDATE collab_checkpoints SET attestation_check = 'totally_fine'
             WHERE session_id = 'legacy-v20-session'",
            [],
        );
        assert!(
            bogus.is_err(),
            "migration 021's CHECK must reject a verdict outside the vocabulary"
        );
    }

    #[test]
    fn test_v19_to_v20_adds_collab_checkpoints_table_and_preserves_legacy_sessions() {
        let db = open_at_v19();
        assert_eq!(schema_version_of(&db), 19);
        assert!(
            !table_exists(&db, "collab_checkpoints"),
            "collab_checkpoints should not exist at v19"
        );

        db.conn
            .execute(
                "INSERT INTO collab_sessions (id, repo_path, branch)
                 VALUES ('legacy-v19-session', '/repo', 'main')",
                [],
            )
            .unwrap();

        db.migrate().unwrap();
        assert_eq!(schema_version_of(&db), LATEST_SCHEMA_VERSION);
        assert_eq!(LATEST_SCHEMA_VERSION, 22);
        assert!(table_exists(&db, "collab_checkpoints"));

        // The pre-upgrade session must survive the migration intact.
        let survived: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM collab_sessions WHERE id = 'legacy-v19-session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            survived, 1,
            "legacy session must survive the v19->v20 upgrade"
        );
    }
}

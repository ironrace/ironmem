//! Compile-time read-only view over a [`Database`].
//!
//! [`Database`] exposes every write/migrate method, so a dashboard handler that
//! accidentally calls a write path only fails at runtime (the underlying SQLite
//! connection is `SQLITE_OPEN_READ_ONLY`, so the write would error — but only
//! once a request hit it). [`ReadOnlyDb`] is a thin newtype that wraps a
//! read-only [`Database`] and re-exposes ONLY the read/query methods the
//! dashboard data layer needs. Because the inner [`Database`] is private, a
//! write call from dashboard code is a *compile* error, not a runtime one.
//!
//! This wrapper holds no logic of its own: every method delegates verbatim to
//! the corresponding [`Database`] read method.

use rusqlite::Connection;

use crate::db::knowledge_graph::KnowledgeGraph;
use crate::db::schema::Database;
use crate::db::Drawer;
use crate::error::MemoryError;
use crate::report::{run_report, Report, ReportOptions};

/// A read-only handle to a [`Database`] opened with `SQLITE_OPEN_READ_ONLY`.
///
/// Exposes only read/query methods. Write and migrate methods on [`Database`]
/// are unreachable through this type, so misuse is caught at compile time.
pub struct ReadOnlyDb {
    pub(super) inner: Database,
}

impl ReadOnlyDb {
    /// Highest applied schema version (no migration is run).
    pub fn schema_version(&self) -> Result<i64, MemoryError> {
        self.inner.schema_version()
    }

    /// Borrow the underlying connection for a read-only closure.
    ///
    /// Mirrors [`Database::with_connection`]; opens no transaction.
    pub fn with_connection<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, MemoryError>,
    ) -> Result<T, MemoryError> {
        self.inner.with_connection(f)
    }

    /// Count drawers, optionally filtered by wing.
    pub fn count_drawers(&self, wing: Option<&str>) -> Result<usize, MemoryError> {
        self.inner.count_drawers(wing)
    }

    /// Wing → total drawer count.
    pub fn wing_counts(&self) -> Result<Vec<(String, usize)>, MemoryError> {
        self.inner.wing_counts()
    }

    /// Room → total drawer count, optionally filtered by wing.
    pub fn room_counts(&self, wing: Option<&str>) -> Result<Vec<(String, usize)>, MemoryError> {
        self.inner.room_counts(wing)
    }

    /// Wing → room → drawer count.
    pub fn taxonomy(
        &self,
    ) -> Result<std::collections::HashMap<String, Vec<(String, usize)>>, MemoryError> {
        self.inner.taxonomy()
    }

    /// Most recent drawers with optional wing/room filters.
    pub fn get_drawers(
        &self,
        wing: Option<&str>,
        room: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Drawer>, MemoryError> {
        self.inner.get_drawers(wing, room, limit)
    }

    /// Exact-id drawer lookup.
    pub fn get_drawer(&self, id: &str) -> Result<Option<Drawer>, MemoryError> {
        self.inner.get_drawer(id)
    }

    /// Knowledge-graph counts via the approved read-only KG path.
    pub fn kg_stats(&self) -> Result<serde_json::Value, MemoryError> {
        KnowledgeGraph::new(&self.inner).stats()
    }

    /// Run a metrics report — delegates to [`run_report`] so CLI and dashboard
    /// semantics stay identical.
    pub fn run_report(&self, opts: &ReportOptions) -> Result<Report, MemoryError> {
        run_report(&self.inner, opts)
    }
}

#[cfg(test)]
mod tests {
    use crate::db::schema::Database;

    /// Documents that `ReadOnlyDb` exposes the read methods the dashboard needs
    /// and that they return the same data as the underlying `Database`.
    ///
    /// The compile-time guarantee is the real contract: a write method (e.g.
    /// `ro.insert_drawer(..)` or `ro.migrate()`) does not exist on `ReadOnlyDb`,
    /// so it cannot be called from dashboard code. The commented line below would
    /// fail to compile if uncommented:
    ///
    /// ```compile_fail
    /// # use ironmem::db::read_only::ReadOnlyDb;
    /// fn writes(ro: &ReadOnlyDb) {
    ///     ro.migrate().unwrap();
    /// }
    /// ```
    #[test]
    fn read_only_db_exposes_reads_only() {
        // Build a populated DB, then re-open it read-only.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.sqlite3");
        {
            let db = Database::open(&path).unwrap();
            db.migrate().unwrap();
            let emb = vec![0.0f32; ironrace_embed::embedder::EMBED_DIM];
            let id = crate::db::drawers::generate_id("hello", "w", "r");
            db.insert_drawer(&id, "hello", &emb, "w", "r", "src/a.rs", "test")
                .unwrap();
        }

        let ro = Database::open_read_only(&path).unwrap();

        // Reads work and match what was written.
        assert_eq!(
            ro.schema_version().unwrap(),
            Database::open(&path).unwrap().schema_version().unwrap()
        );
        assert_eq!(ro.count_drawers(None).unwrap(), 1);
        assert_eq!(ro.wing_counts().unwrap().len(), 1);
        assert_eq!(ro.get_drawers(None, None, 10).unwrap().len(), 1);
        assert!(ro.kg_stats().is_ok());
        let n: i64 = ro
            .with_connection(|conn| {
                Ok(conn.query_row("SELECT COUNT(*) FROM drawers", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(n, 1);
    }
}

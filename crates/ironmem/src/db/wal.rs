use rusqlite::{params, Transaction};

use super::schema::Database;
use crate::error::MemoryError;

/// Default WAL retention period in days.
const WAL_RETENTION_DAYS: i64 = 90;

impl Database {
    /// Log a write operation to the audit trail.
    #[allow(dead_code)]
    pub fn wal_log(
        &self,
        operation: &str,
        params_json: &serde_json::Value,
        result_json: Option<&serde_json::Value>,
    ) -> Result<(), MemoryError> {
        Self::wal_log_conn(&self.conn, operation, params_json, result_json)
    }

    pub(crate) fn wal_log_tx(
        tx: &Transaction<'_>,
        operation: &str,
        params_json: &serde_json::Value,
        result_json: Option<&serde_json::Value>,
    ) -> Result<(), MemoryError> {
        Self::wal_log_conn(tx, operation, params_json, result_json)
    }

    fn wal_log_conn(
        conn: &rusqlite::Connection,
        operation: &str,
        params_json: &serde_json::Value,
        result_json: Option<&serde_json::Value>,
    ) -> Result<(), MemoryError> {
        let result_str = result_json.map(|v| v.to_string());

        conn.execute(
            "INSERT INTO wal_log (operation, params, result) VALUES (?1, ?2, ?3)",
            params![operation, params_json.to_string(), result_str],
        )?;

        Ok(())
    }

    /// Prune WAL entries older than the retention period.
    pub fn wal_prune(&self, retention_days: Option<i64>) -> Result<usize, MemoryError> {
        let days = retention_days.unwrap_or(WAL_RETENTION_DAYS);
        let count = self.conn.execute(
            "DELETE FROM wal_log WHERE timestamp < datetime('now', ?1)",
            params![format!("-{days} days")],
        )?;
        if count > 0 {
            tracing::info!("Pruned {count} WAL entries older than {days} days");
        }
        Ok(count)
    }
}

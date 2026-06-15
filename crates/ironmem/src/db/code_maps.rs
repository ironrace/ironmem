//! `code_maps` sidecar table — DB helpers for the lazy per-area code-map
//! feature (issue #94). Each code map is a drawer in room `"code-maps"` plus
//! one row here keyed `(repo, area)`. Workers reach maps only through MCP
//! tools; this module is the raw storage layer.

use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use super::schema::Database;
use crate::error::MemoryError;

/// A stored `code_maps` row (immutable snapshot; never mutated in place).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeMap {
    pub repo: String,
    pub area: String,
    pub drawer_id: String,
    pub head_sha: String,
    pub source_files: Vec<String>,
    pub built_by: String,
    pub built_at: String,
}

fn map_code_map(row: &rusqlite::Row<'_>) -> rusqlite::Result<CodeMap> {
    let source_files_json: String = row.get(4)?;
    let source_files: Vec<String> = serde_json::from_str(&source_files_json).unwrap_or_default();
    Ok(CodeMap {
        repo: row.get(0)?,
        area: row.get(1)?,
        drawer_id: row.get(2)?,
        head_sha: row.get(3)?,
        source_files,
        built_by: row.get(5)?,
        built_at: row.get(6)?,
    })
}

impl Database {
    /// Upsert a `code_maps` sidecar row. On conflict `(repo, area)`, replaces
    /// the prior row. If `drawer_id` changes (content refresh), deletes the
    /// superseded prior drawer via `delete_drawer_tx` (FTS-clean) inside the
    /// same transaction so no orphan stale map drawer accumulates.
    ///
    /// Callers MUST ensure the new `drawer_id` already exists in `drawers`
    /// before calling this (FK constraint). The MCP `code_map_write` tool
    /// writes the drawer and calls this in one transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_code_map(
        &self,
        repo: &str,
        area: &str,
        drawer_id: &str,
        head_sha: &str,
        source_files: &[String],
        built_by: &str,
        built_at: &str,
    ) -> Result<(), MemoryError> {
        self.with_transaction(|tx| {
            Self::upsert_code_map_tx(
                tx,
                repo,
                area,
                drawer_id,
                head_sha,
                source_files,
                built_by,
                built_at,
            )
        })
    }

    /// Transaction-scoped variant for use inside the MCP `code_map_write`
    /// handler (drawer insert + sidecar upsert in one atomic transaction).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn upsert_code_map_tx(
        tx: &Transaction<'_>,
        repo: &str,
        area: &str,
        drawer_id: &str,
        head_sha: &str,
        source_files: &[String],
        built_by: &str,
        built_at: &str,
    ) -> Result<(), MemoryError> {
        let source_files_json = serde_json::to_string(source_files)
            .map_err(|e| MemoryError::Validation(format!("source_files serialization: {e}")))?;

        // Fetch prior drawer_id (if any) before overwriting.
        let prior_drawer_id: Option<String> = tx
            .query_row(
                "SELECT drawer_id FROM code_maps WHERE repo = ?1 AND area = ?2",
                params![repo, area],
                |row| row.get(0),
            )
            .optional()?;

        // Upsert the sidecar row (replace-not-append per design).
        tx.execute(
            "INSERT INTO code_maps (repo, area, drawer_id, head_sha, source_files, built_by, built_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(repo, area) DO UPDATE SET
                 drawer_id    = excluded.drawer_id,
                 head_sha     = excluded.head_sha,
                 source_files = excluded.source_files,
                 built_by     = excluded.built_by,
                 built_at     = excluded.built_at",
            params![repo, area, drawer_id, head_sha, source_files_json, built_by, built_at],
        )?;

        // If the drawer changed, delete the superseded one (FTS-clean via delete_drawer_tx).
        if let Some(prior) = prior_drawer_id {
            if prior != drawer_id {
                Self::delete_drawer_tx(tx, &prior)?;
            }
        }

        Ok(())
    }

    /// Fetch the current `code_maps` row for `(repo, area)`. Returns `None`
    /// when no map has been built yet.
    pub fn get_code_map(&self, repo: &str, area: &str) -> Result<Option<CodeMap>, MemoryError> {
        self.conn
            .query_row(
                "SELECT repo, area, drawer_id, head_sha, source_files, built_by, built_at
                 FROM code_maps
                 WHERE repo = ?1 AND area = ?2",
                params![repo, area],
                map_code_map,
            )
            .optional()
            .map_err(MemoryError::from)
    }
}

#[cfg(test)]
mod tests {
    use crate::db::drawers::generate_id;
    use crate::db::schema::Database;

    fn dummy_embedding() -> Vec<f32> {
        vec![0.0f32; ironrace_embed::embedder::EMBED_DIM]
    }

    fn insert_test_drawer(db: &Database, content: &str, wing: &str, room: &str) -> String {
        let id = generate_id(content, wing, room);
        let emb = dummy_embedding();
        db.insert_drawer(&id, content, &emb, wing, room, "src/test.rs", "test")
            .unwrap();
        id
    }

    #[test]
    fn test_write_load_round_trip() {
        let db = Database::open_in_memory().unwrap();

        let drawer_id = insert_test_drawer(&db, "code map content alpha", "myrepo", "code-maps");
        let files = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];

        db.upsert_code_map(
            "myrepo",
            "core",
            &drawer_id,
            "abc123",
            &files,
            "test-agent",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        let result = db.get_code_map("myrepo", "core").unwrap().unwrap();
        assert_eq!(result.repo, "myrepo");
        assert_eq!(result.area, "core");
        assert_eq!(result.drawer_id, drawer_id);
        assert_eq!(result.head_sha, "abc123");
        assert_eq!(result.source_files, files);
        assert_eq!(result.built_by, "test-agent");
        assert_eq!(result.built_at, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn test_pk_returns_single_current_map() {
        let db = Database::open_in_memory().unwrap();

        let drawer_id1 = insert_test_drawer(&db, "content v1", "repo1", "code-maps");
        let files1 = vec!["src/a.rs".to_string()];
        db.upsert_code_map(
            "repo1",
            "core",
            &drawer_id1,
            "sha1",
            &files1,
            "agent",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        let drawer_id2 = insert_test_drawer(&db, "content v2 updated", "repo1", "code-maps");
        let files2 = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        db.upsert_code_map(
            "repo1",
            "core",
            &drawer_id2,
            "sha2",
            &files2,
            "agent",
            "2026-01-02T00:00:00Z",
        )
        .unwrap();

        // Only one map should exist for (repo1, core)
        let result = db.get_code_map("repo1", "core").unwrap().unwrap();
        assert_eq!(result.drawer_id, drawer_id2);
        assert_eq!(result.head_sha, "sha2");
        assert_eq!(result.source_files, files2);

        // Verify there's exactly one row
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM code_maps WHERE repo = 'repo1' AND area = 'core'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_refresh_swaps_drawer_and_deletes_old() {
        let db = Database::open_in_memory().unwrap();

        let old_drawer_id =
            insert_test_drawer(&db, "old code map content unique", "repo-x", "code-maps");
        let files = vec!["src/old.rs".to_string()];
        db.upsert_code_map(
            "repo-x",
            "auth",
            &old_drawer_id,
            "old-sha",
            &files,
            "agent",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        // Verify old drawer exists
        assert!(db.get_drawer(&old_drawer_id).unwrap().is_some());

        // Now refresh with a new drawer
        let new_drawer_id =
            insert_test_drawer(&db, "new code map content unique", "repo-x", "code-maps");
        let new_files = vec!["src/new.rs".to_string()];
        db.upsert_code_map(
            "repo-x",
            "auth",
            &new_drawer_id,
            "new-sha",
            &new_files,
            "agent",
            "2026-01-02T00:00:00Z",
        )
        .unwrap();

        // Old drawer must be gone
        assert!(db.get_drawer(&old_drawer_id).unwrap().is_none());

        // New drawer must exist
        assert!(db.get_drawer(&new_drawer_id).unwrap().is_some());

        // code_maps row must reference new drawer
        let result = db.get_code_map("repo-x", "auth").unwrap().unwrap();
        assert_eq!(result.drawer_id, new_drawer_id);
    }

    #[test]
    fn test_fk_cascade_deletes_sidecar() {
        let db = Database::open_in_memory().unwrap();

        let drawer_id = insert_test_drawer(
            &db,
            "cascade test content unique",
            "cascade-repo",
            "code-maps",
        );
        let files = vec!["src/c.rs".to_string()];
        db.upsert_code_map(
            "cascade-repo",
            "network",
            &drawer_id,
            "deadbeef",
            &files,
            "agent",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        // Verify sidecar row exists
        assert!(db
            .get_code_map("cascade-repo", "network")
            .unwrap()
            .is_some());

        // Delete the drawer — FK cascade should remove the sidecar
        db.delete_drawer(&drawer_id).unwrap();

        // Sidecar must be gone via FK cascade
        assert!(db
            .get_code_map("cascade-repo", "network")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_source_files_json_round_trip() {
        let db = Database::open_in_memory().unwrap();

        let drawer_id =
            insert_test_drawer(&db, "json round trip content", "json-repo", "code-maps");
        let files = vec![
            "a/b.rs".to_string(),
            "c/d.rs".to_string(),
            "e.rs".to_string(),
        ];

        db.upsert_code_map(
            "json-repo",
            "parser",
            &drawer_id,
            "cafe1234",
            &files,
            "agent",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        let result = db.get_code_map("json-repo", "parser").unwrap().unwrap();
        assert_eq!(result.source_files, files);
        assert_eq!(result.source_files.len(), 3);
        assert_eq!(result.source_files[0], "a/b.rs");
        assert_eq!(result.source_files[1], "c/d.rs");
        assert_eq!(result.source_files[2], "e.rs");
    }

    #[test]
    fn test_two_repos_same_area_no_collision() {
        let db = Database::open_in_memory().unwrap();

        let drawer_id1 = insert_test_drawer(&db, "repo1 core content unique", "repo1", "code-maps");
        let files1 = vec!["repo1/src.rs".to_string()];
        db.upsert_code_map(
            "repo1",
            "core",
            &drawer_id1,
            "sha-repo1",
            &files1,
            "agent",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        let drawer_id2 = insert_test_drawer(&db, "repo2 core content unique", "repo2", "code-maps");
        let files2 = vec!["repo2/src.rs".to_string()];
        db.upsert_code_map(
            "repo2",
            "core",
            &drawer_id2,
            "sha-repo2",
            &files2,
            "agent",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        let result1 = db.get_code_map("repo1", "core").unwrap().unwrap();
        let result2 = db.get_code_map("repo2", "core").unwrap().unwrap();

        assert_eq!(result1.repo, "repo1");
        assert_eq!(result1.source_files, files1);
        assert_eq!(result2.repo, "repo2");
        assert_eq!(result2.source_files, files2);

        // They must have different drawers
        assert_ne!(result1.drawer_id, result2.drawer_id);
    }

    #[test]
    fn test_old_content_gone_after_refresh_fts() {
        let db = Database::open_in_memory().unwrap();

        // Use space-separated words so FTS5 tokenizer and query sanitizer both
        // see the same tokens. Underscores are stripped by fts5_sanitize, so use
        // plain alphabetic words that are distinct enough to not collide.
        let old_content = "zephyrold stalactite oblique codemap drawer";
        let old_drawer_id = insert_test_drawer(&db, old_content, "fts-repo", "code-maps");
        let files = vec!["src/old.rs".to_string()];
        db.upsert_code_map(
            "fts-repo",
            "search",
            &old_drawer_id,
            "old-sha",
            &files,
            "agent",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        // Verify old content is searchable via FTS using a distinctive word from it
        let hits_before = db.bm25_search("stalactite", 10, None, None).unwrap();
        assert!(
            !hits_before.is_empty(),
            "old content should appear in FTS before refresh"
        );

        // Refresh with new drawer having different content
        let new_content = "vortexnew crevasse pinnacle codemap drawer";
        let new_drawer_id = insert_test_drawer(&db, new_content, "fts-repo", "code-maps");
        let new_files = vec!["src/new.rs".to_string()];
        db.upsert_code_map(
            "fts-repo",
            "search",
            &new_drawer_id,
            "new-sha",
            &new_files,
            "agent",
            "2026-01-02T00:00:00Z",
        )
        .unwrap();

        // Old content must not appear in FTS after refresh
        let hits_after = db.bm25_search("stalactite", 10, None, None).unwrap();
        assert!(
            hits_after.is_empty(),
            "old drawer content must be purged from FTS after refresh"
        );

        // New content should be searchable
        let new_hits = db.bm25_search("crevasse", 10, None, None).unwrap();
        assert!(
            !new_hits.is_empty(),
            "new drawer content should appear in FTS"
        );
    }
}

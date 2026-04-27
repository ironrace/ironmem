-- Drop the now-zombified `current_task_index` column.
--
-- Migration 005 added it for the per-task v2/v3 coding loop. The v3 batch
-- refactor (commit `feat(collab): replace per-task v3 loop`) removed all
-- per-task phases, so the column has been written as NULL and never read
-- since. Dropping it now keeps the schema honest before we accumulate
-- more code paths that might re-introduce a dependency on it.
--
-- SQLite supports `ALTER TABLE ... DROP COLUMN` from 3.35+. The
-- accompanying SELECT/UPDATE in queue.rs is updated in the same commit.
ALTER TABLE collab_sessions DROP COLUMN current_task_index;

INSERT OR IGNORE INTO schema_version (version) VALUES (7);

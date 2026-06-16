-- Migration 011: lazy per-area code maps (issue #94).
-- Adds the code_maps sidecar table and three nullable columns on token_usage
-- for map-hit / map-miss exploration-token attribution (Phase 5).
-- All DDL is idempotent (IF NOT EXISTS / ALTER ADD guarded by version check).

CREATE TABLE IF NOT EXISTS code_maps (
    repo         TEXT NOT NULL,
    area         TEXT NOT NULL,
    drawer_id    TEXT NOT NULL,
    head_sha     TEXT NOT NULL,
    source_files TEXT NOT NULL,
    built_by     TEXT NOT NULL,
    built_at     TEXT NOT NULL,
    PRIMARY KEY (repo, area),
    FOREIGN KEY (drawer_id) REFERENCES drawers(id) ON DELETE CASCADE
);

-- Three nullable columns for exploration-token attribution.
-- ALTER ADD COLUMN is not IF-NOT-EXISTS in SQLite; the version < 11 gate
-- in schema.rs prevents re-application.
ALTER TABLE token_usage ADD COLUMN map_status TEXT
    CHECK (map_status IS NULL OR map_status IN ('map_hit','map_miss'));
ALTER TABLE token_usage ADD COLUMN turn_id TEXT;
ALTER TABLE token_usage ADD COLUMN area TEXT;

INSERT OR IGNORE INTO schema_version (version) VALUES (11);

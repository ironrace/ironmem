-- ironmem schema v2: FTS5 full-text search index for hybrid BM25+vector retrieval.
--
-- This migration is guarded by schema_version in schema.rs and runs exactly once.
-- The porter tokenizer applies stemming so "running" matches "run", "searched" matches "search".

CREATE VIRTUAL TABLE IF NOT EXISTS drawers_fts USING fts5(
    content,
    drawer_id UNINDEXED,
    tokenize='porter ascii'
);

-- Backfill all existing drawers into the FTS index.
-- WHERE NOT EXISTS guard makes this idempotent: if the migration is retried
-- (e.g. after a SQLITE_BUSY interruption), we skip re-inserting rows that
-- were already written, preventing duplicate FTS entries.
INSERT INTO drawers_fts(content, drawer_id)
SELECT content, id FROM drawers
WHERE NOT EXISTS (SELECT 1 FROM drawers_fts LIMIT 1);

INSERT OR IGNORE INTO schema_version (version) VALUES (2);

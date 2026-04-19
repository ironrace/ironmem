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
INSERT INTO drawers_fts(content, drawer_id)
SELECT content, id FROM drawers;

INSERT OR IGNORE INTO schema_version (version) VALUES (2);

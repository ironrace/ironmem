-- Migration 023: indexes for the prompt-recall hook's per-prompt reads (issue #375).
--
-- The hook has a 150 ms budget for everything it does on a prompt, and two of its
-- reads scanned a wing row by row:
--
-- * Whether a workspace has memory of its own. Rows a path doesn't place, the ones
--   that aren't mined or carry no `source_file`, are found by their wing. A mined
--   wing is almost all mined rows, so `idx_drawers_wing` led to every one of them,
--   each read through its content and embedding. The partial index holds only the
--   rows that question is about; its WHERE is repeated verbatim by the query
--   (`UNPLACED_BY_PATH` in db/drawers.rs) so SQLite can use it.
-- * The newest diary entries, which were all read and sorted on every prompt. The
--   Stop hook adds one per session, so that grew without bound.
--
-- Both are plain `CREATE INDEX IF NOT EXISTS`: no data changes.

CREATE INDEX IF NOT EXISTS idx_drawers_wing_unplaced
    ON drawers(wing)
    WHERE added_by <> 'mine' OR source_file = '';

CREATE INDEX IF NOT EXISTS idx_drawers_current_wing_filed
    ON drawers(wing, filed_at)
    WHERE superseded_by IS NULL;

INSERT OR IGNORE INTO schema_version (version) VALUES (23);

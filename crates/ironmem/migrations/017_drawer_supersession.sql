-- Migration 017: retain supersession lineage for drawers (issue #211).
--
-- Existing drawers stay current: NULL means a drawer has not been superseded.
-- The partial index keeps current-drawer lookups within a wing/room efficient.
-- schema.rs applies this non-idempotent ALTER TABLE once behind its version gate.

ALTER TABLE drawers ADD COLUMN superseded_by TEXT;

CREATE INDEX idx_drawers_current_wing_room
    ON drawers(wing, room)
    WHERE superseded_by IS NULL;

INSERT OR IGNORE INTO schema_version (version) VALUES (17);

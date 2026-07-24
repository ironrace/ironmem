-- Migration 016: persist drawer references for collab messages (issue #206).
--
-- New collab messages retain a drawer id alongside their inline content, so
-- receivers can resolve the drawer when it is available. The column is
-- deliberately nullable: every message sent before this migration remains
-- readable through the legacy inline-content path, with drawer_id NULL.
--
-- `ALTER TABLE … ADD COLUMN` is not idempotent, so schema.rs applies this file
-- once behind `current_version < 16` while holding its migration write lock.

ALTER TABLE messages ADD COLUMN drawer_id TEXT;

INSERT OR IGNORE INTO schema_version (version) VALUES (16);

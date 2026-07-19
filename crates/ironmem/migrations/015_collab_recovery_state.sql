-- Migration 015: recoverable tooling-failure state on collab_sessions (issue #197).
--
-- Collab tooling failures (an MCP call erroring mid-phase, a dropped daemon
-- connection, etc.) currently have no persisted record — the session just
-- stalls with no trace of what broke or who is trying to recover it. These
-- six nullable columns let a session capture that state without disturbing
-- the existing phase/owner machinery:
--
--   pending_failure        classified failure kind (see failure_class.rs,
--                           issue #197 task 1), NULL when no failure pending.
--   failed_from_phase       the `Phase` the session was in when the failure
--                           was recorded, so recovery can resume in place.
--   recovery_phase          sub-phase of the recovery flow itself (distinct
--                           from the session's normal `phase` column).
--   recovery_owner          which `Agent` currently drives recovery.
--   recovery_origin_owner   which `Agent` owned the session when the failure
--                           occurred, so recovery can hand control back.
--   recovery_attempts       how many recovery attempts have been made; NULL
--                           on legacy (pre-015) rows, read back as a default
--                           of 0 by the application layer.
--
-- All six are NULL for every existing row (no failure in flight) and stay
-- NULL for the common case going forward — recovery state is the exception,
-- not the rule.
--
-- `ALTER TABLE … ADD COLUMN` is NOT idempotent (it errors with
-- `duplicate column` on replay), so — exactly like 009/014 — this migration
-- is version-gated in schema.rs::migrate() behind `current_version < 15` and
-- runs exactly once under the BEGIN IMMEDIATE write lock.

ALTER TABLE collab_sessions ADD COLUMN pending_failure TEXT;
ALTER TABLE collab_sessions ADD COLUMN failed_from_phase TEXT;
ALTER TABLE collab_sessions ADD COLUMN recovery_phase TEXT;
ALTER TABLE collab_sessions ADD COLUMN recovery_owner TEXT;
ALTER TABLE collab_sessions ADD COLUMN recovery_origin_owner TEXT;
ALTER TABLE collab_sessions ADD COLUMN recovery_attempts INTEGER;

INSERT OR IGNORE INTO schema_version (version) VALUES (15);

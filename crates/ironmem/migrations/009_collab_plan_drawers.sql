-- Migration 009: plan-by-reference drawer-id columns on collab_sessions (issue #90).
--
-- Two nullable columns each hold the deterministic 32-char drawer id of an
-- accepted plan body that has been stored as a drawer rather than inlined into
-- `collab_status`: `canonical_plan_drawer_id` for the canonical (working) plan
-- and `final_plan_drawer_id` for the final (locked) plan. NULL means the
-- legacy full-text path — the plan body still lives inline and no drawer was
-- written. There is no backfill: existing sessions keep NULL and continue to
-- serve their inline plan text.
--
-- `ALTER TABLE … ADD COLUMN` is NOT idempotent (it errors with
-- `duplicate column` on replay), so — exactly like 006/007 — this migration is
-- version-gated in schema.rs::migrate() behind `current_version < 9` and runs
-- exactly once under the BEGIN IMMEDIATE write lock.

ALTER TABLE collab_sessions ADD COLUMN canonical_plan_drawer_id TEXT;
ALTER TABLE collab_sessions ADD COLUMN final_plan_drawer_id TEXT;

INSERT OR IGNORE INTO schema_version (version) VALUES (9);

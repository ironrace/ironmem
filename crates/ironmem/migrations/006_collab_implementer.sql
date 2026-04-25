-- Collab v3.1: per-session `implementer` selection.
--
-- Lets `/collab start --implementer=codex` route the v3
-- `CodeImplementPending` phase to Codex (running its own
-- subagent-driven-development end-to-end) instead of Claude. v1 planning
-- and the v3 global review stage are unchanged — only the batch
-- implementation phase's owner shifts.
--
-- The column is NOT NULL with `'claude'` as the default so existing
-- sessions and the `/collab start` callers that omit the field keep the
-- original behavior. The CHECK constraint pins the allowed values at the
-- DB level so a malformed direct write cannot put the state machine into
-- an unreachable owner.
ALTER TABLE collab_sessions
    ADD COLUMN implementer TEXT NOT NULL DEFAULT 'claude'
    CHECK (implementer IN ('claude', 'codex'));

INSERT OR IGNORE INTO schema_version (version) VALUES (6);

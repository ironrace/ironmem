-- Collab protocol role genericity: per-session `pilot` agent selection.
--
-- Lets `/collab start --implementer=codex` (and future agent variants)
-- configure which agent drives the v3 batch implementation phase.
-- Prior to 019, `implementer` controlled this; 019 decouples the role from
-- the implementation agent so the protocol can flow through different roles
-- (e.g., reviewer, auditor) without tying to a specific coder.
--
-- Every pre-019 row reads `pilot='claude'`, so no data migration is needed.
-- The column is NOT NULL with `'claude'` as the default so existing
-- sessions and the `/collab start` callers that omit the field keep the
-- original behavior. The CHECK constraint pins the allowed values at the
-- DB level so a malformed direct write cannot put the state machine into
-- an unreachable pilot role.
ALTER TABLE collab_sessions
    ADD COLUMN pilot TEXT NOT NULL DEFAULT 'claude'
    CHECK (pilot IN ('claude', 'codex'));

INSERT OR IGNORE INTO schema_version (version) VALUES (19);

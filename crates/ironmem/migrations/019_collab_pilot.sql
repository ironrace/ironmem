-- Collab protocol role genericity: per-session `pilot` agent selection.
--
-- Lets `/collab start --pilot=codex` (and future agent variants) configure
-- which agent leads the session: the pilot synthesizes and finalizes the
-- plan (`canonical` / `final` / `task_list`) and audits the copilot's
-- commits (`review_local` / `final_review`). The *copilot* is always the
-- pilot's counterpart — derived on the fly, never stored, hence no
-- `copilot` column. Prior to 019 the lead was hard-wired to Claude.
--
-- `pilot` is orthogonal to the existing `implementer` column and is
-- deliberately NOT validated against it. `implementer` remains untouched
-- and authoritative for the v3 batch phase (`CodeImplementPending`): it
-- still decides who writes the code, while `pilot` decides who leads the
-- plan and reviews the result. Any combination is legal — e.g.
-- `pilot='codex'` with `implementer='claude'`. See the `pilot` field doc on
-- `CollabSession` (src/collab/session.rs) for the full four-knob vocabulary
-- (dispatcher / pilot / copilot / implementer).
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

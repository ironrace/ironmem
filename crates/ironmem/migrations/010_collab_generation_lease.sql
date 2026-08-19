-- Migration 010: per-actor generation lease for session_handoff (issue #91).
-- Separate table keeps collab_sessions untouched. One row per (session_id, agent),
-- created lazily at generation 0 — no backfill. issue sets pending fields WITHOUT
-- bumping generation; claim advances generation = pending_handoff_generation.
-- The migration runner (schema.rs) skips this file when current_version >= 10;
-- CREATE TABLE IF NOT EXISTS keeps the DDL safe if executed directly.

CREATE TABLE IF NOT EXISTS collab_actor_generations (
    session_id TEXT NOT NULL REFERENCES collab_sessions(id) ON DELETE CASCADE,
    agent TEXT NOT NULL CHECK (agent IN ('claude','codex')),
    generation INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0),
    pending_handoff_token TEXT,
    pending_handoff_generation INTEGER CHECK (pending_handoff_generation IS NULL OR pending_handoff_generation >= 0),
    -- Written by issue/claim ops. Originally audit-only; since #297 both are
    -- also read by collab::queue::session_last_activity as the fourth
    -- liveness source, so a session mid-recovery does not read dead to
    -- collab_end's abandon arm. Do not repurpose or stop writing them.
    pending_handoff_issued_at TEXT,
    pending_handoff_claimed_at TEXT,
    PRIMARY KEY (session_id, agent)
);

CREATE INDEX IF NOT EXISTS idx_collab_actor_generations_session
    ON collab_actor_generations(session_id);

INSERT OR IGNORE INTO schema_version (version) VALUES (10);

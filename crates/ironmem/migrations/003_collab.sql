CREATE TABLE IF NOT EXISTS collab_sessions (
    id                   TEXT PRIMARY KEY,
    phase                TEXT NOT NULL DEFAULT 'PlanParallelDrafts',
    current_owner        TEXT NOT NULL DEFAULT 'claude',
    repo_path            TEXT NOT NULL,
    branch               TEXT NOT NULL,
    claude_draft_hash    TEXT,
    codex_draft_hash     TEXT,
    canonical_plan_hash  TEXT,
    final_plan_hash      TEXT,
    codex_review_verdict TEXT,
    created_at           TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at           TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS messages (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES collab_sessions(id),
    sender      TEXT NOT NULL,
    receiver    TEXT NOT NULL,
    topic       TEXT NOT NULL,
    content     TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending',
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_messages_receiver_status ON messages(receiver, status);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);

CREATE TABLE IF NOT EXISTS agent_capabilities (
    id            TEXT PRIMARY KEY,
    session_id    TEXT NOT NULL REFERENCES collab_sessions(id),
    agent         TEXT NOT NULL,
    capability    TEXT NOT NULL,
    description   TEXT,
    registered_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(session_id, agent, capability)
);
CREATE INDEX IF NOT EXISTS idx_caps_session_agent ON agent_capabilities(session_id, agent);

INSERT OR IGNORE INTO schema_version (version) VALUES (3);

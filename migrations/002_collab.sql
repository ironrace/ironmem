-- ironrace-memory collab protocol schema v2

-- Collaboration sessions track the lifecycle of a Claude↔Codex planning loop.
CREATE TABLE IF NOT EXISTS collab_sessions (
    id            TEXT PRIMARY KEY,
    phase         TEXT NOT NULL DEFAULT 'PlanDraft',
    current_owner TEXT NOT NULL DEFAULT 'claude',
    round         INTEGER NOT NULL DEFAULT 0,
    max_rounds    INTEGER NOT NULL DEFAULT 5,
    repo_path     TEXT NOT NULL,
    branch        TEXT NOT NULL,
    claude_ok        INTEGER NOT NULL DEFAULT 0,
    codex_ok         INTEGER NOT NULL DEFAULT 0,
    content_hash     TEXT,
    rejected_hashes  TEXT NOT NULL DEFAULT '[]',
    created_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Async message queue between Claude and Codex within a session.
CREATE TABLE IF NOT EXISTS messages (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES collab_sessions(id) ON DELETE CASCADE,
    sender      TEXT NOT NULL,
    receiver    TEXT NOT NULL,
    topic       TEXT NOT NULL,
    content     TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending',
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_messages_receiver_status ON messages(receiver, status);
CREATE INDEX IF NOT EXISTS idx_messages_session         ON messages(session_id);
CREATE INDEX IF NOT EXISTS idx_messages_created         ON messages(created_at);

-- Per-session registry of each agent's available sub-agents/tools.
CREATE TABLE IF NOT EXISTS agent_capabilities (
    id            TEXT PRIMARY KEY,
    session_id    TEXT NOT NULL REFERENCES collab_sessions(id) ON DELETE CASCADE,
    agent         TEXT NOT NULL,
    capability    TEXT NOT NULL,
    description   TEXT,
    registered_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(session_id, agent, capability)
);

CREATE INDEX IF NOT EXISTS idx_caps_session_agent ON agent_capabilities(session_id, agent);

INSERT OR IGNORE INTO schema_version (version) VALUES (2);

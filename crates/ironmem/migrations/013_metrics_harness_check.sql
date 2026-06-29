-- Migration 013: value-preserving rebuild of the migration-008 metrics tables
-- to relax the harness CHECK from the hard-coded 'claude'/'codex' domain to the
-- registry slug form:
--
--   harness GLOB '[a-z0-9]*' AND harness NOT GLOB '*[^a-z0-9_-]*'
--
-- This mirrors the HarnessId slug validator used by issue #155 (multi-harness
-- generalisation) so any registered harness can persist metrics rows.
--
-- Migrations 006 (collab implementer CHECK claude|codex) and 010
-- (generation-lease agent CHECK claude|codex) are protocol-specific and are
-- NOT touched here — they stay claude/codex by design.
--
-- Rows are copied byte-for-byte (no value change). The table-rebuild dance is
-- required because SQLite does not support DROP/MODIFY CONSTRAINT. All three
-- rebuilt tables use soft foreign keys (no REFERENCES), so no
-- PRAGMA foreign_keys dance is needed.

-- ─── token_usage ────────────────────────────────────────────────────────────

CREATE TABLE token_usage_new (
    id                          INTEGER PRIMARY KEY,
    ts                          TEXT NOT NULL,
    source                      TEXT NOT NULL CHECK (source IN ('llm_rerank','pref_extract','transcript','mcp_response')),
    harness                     TEXT NOT NULL CHECK (harness GLOB '[a-z0-9]*' AND harness NOT GLOB '*[^a-z0-9_-]*'),
    model                       TEXT,
    session_id                  TEXT,
    collab_session_id           TEXT,
    collab_phase                TEXT CHECK (collab_phase IS NULL OR collab_phase IN ('planning','impl','review','rework','other')),
    task_tag                    TEXT,
    input_tokens                INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens               INTEGER NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cache_creation_input_tokens >= 0),
    cache_read_input_tokens     INTEGER NOT NULL DEFAULT 0 CHECK (cache_read_input_tokens >= 0),
    estimated                   INTEGER NOT NULL DEFAULT 0 CHECK (estimated IN (0, 1)),
    chars                       INTEGER NOT NULL DEFAULT 0 CHECK (chars >= 0),
    cost_usd                    REAL CHECK (cost_usd IS NULL OR cost_usd >= 0),
    map_status                  TEXT CHECK (map_status IS NULL OR map_status IN ('map_hit','map_miss')),
    turn_id                     TEXT,
    area                        TEXT
);

INSERT INTO token_usage_new (
    id, ts, source, harness, model, session_id, collab_session_id, collab_phase,
    task_tag, input_tokens, output_tokens, cache_creation_input_tokens,
    cache_read_input_tokens, estimated, chars, cost_usd, map_status, turn_id, area
)
SELECT
    id, ts, source, harness, model, session_id, collab_session_id, collab_phase,
    task_tag, input_tokens, output_tokens, cache_creation_input_tokens,
    cache_read_input_tokens, estimated, chars, cost_usd, map_status, turn_id, area
FROM token_usage;

DROP TABLE token_usage;
ALTER TABLE token_usage_new RENAME TO token_usage;

CREATE INDEX idx_token_usage_task_ts
    ON token_usage (task_tag, ts);
CREATE INDEX idx_token_usage_collab_phase
    ON token_usage (collab_session_id, collab_phase);

-- ─── occupancy_samples ──────────────────────────────────────────────────────

CREATE TABLE occupancy_samples_new (
    id                      INTEGER PRIMARY KEY,
    ts                      TEXT NOT NULL,
    harness                 TEXT NOT NULL CHECK (harness GLOB '[a-z0-9]*' AND harness NOT GLOB '*[^a-z0-9_-]*'),
    session_id              TEXT,
    workspace_root          TEXT,
    hook_event              TEXT CHECK (hook_event IS NULL OR hook_event IN ('session-start','session-stop','precompact','user-prompt-submit')),
    input_tokens            INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    cache_read_input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cache_read_input_tokens >= 0),
    context_window          INTEGER NOT NULL DEFAULT 200000 CHECK (context_window > 0),
    occupancy_pct           REAL CHECK (occupancy_pct IS NULL OR occupancy_pct >= 0)
);

INSERT INTO occupancy_samples_new (
    id, ts, harness, session_id, workspace_root, hook_event,
    input_tokens, cache_read_input_tokens, context_window, occupancy_pct
)
SELECT
    id, ts, harness, session_id, workspace_root, hook_event,
    input_tokens, cache_read_input_tokens, context_window, occupancy_pct
FROM occupancy_samples;

DROP TABLE occupancy_samples;
ALTER TABLE occupancy_samples_new RENAME TO occupancy_samples;

CREATE INDEX idx_occupancy_session_ts
    ON occupancy_samples (session_id, ts);

-- ─── session_summary ────────────────────────────────────────────────────────

CREATE TABLE session_summary_new (
    session_id          TEXT PRIMARY KEY,
    harness             TEXT NOT NULL CHECK (harness GLOB '[a-z0-9]*' AND harness NOT GLOB '*[^a-z0-9_-]*'),
    workspace_root      TEXT,
    started_at          TEXT,
    ended_at            TEXT,
    peak_occupancy_pct  REAL CHECK (peak_occupancy_pct IS NULL OR peak_occupancy_pct >= 0),
    total_input_tokens  INTEGER NOT NULL DEFAULT 0 CHECK (total_input_tokens >= 0),
    total_output_tokens INTEGER NOT NULL DEFAULT 0 CHECK (total_output_tokens >= 0),
    mcp_chars_served    INTEGER NOT NULL DEFAULT 0 CHECK (mcp_chars_served >= 0),
    compactions         INTEGER NOT NULL DEFAULT 0 CHECK (compactions >= 0)
);

INSERT INTO session_summary_new (
    session_id, harness, workspace_root, started_at, ended_at,
    peak_occupancy_pct, total_input_tokens, total_output_tokens,
    mcp_chars_served, compactions
)
SELECT
    session_id, harness, workspace_root, started_at, ended_at,
    peak_occupancy_pct, total_input_tokens, total_output_tokens,
    mcp_chars_served, compactions
FROM session_summary;

DROP TABLE session_summary;
ALTER TABLE session_summary_new RENAME TO session_summary;

-- session_summary has no secondary index.

INSERT OR IGNORE INTO schema_version (version) VALUES (13);

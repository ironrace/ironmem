-- Migration 008: metrics counter tables (METRICS_SPEC §5, §8).
--
-- Four tables backing the tokens-to-done metric program, created
-- version-gated at schema version 8. `collab_session_id` is a SOFT foreign
-- key (no REFERENCES) so metrics rows survive collab-session pruning
-- (METRICS_SPEC §5). Enum domains are pinned with per-column CHECK
-- constraints (same style as 006's `implementer` CHECK) so a malformed
-- direct write cannot land an out-of-domain value. All DDL is idempotent
-- (IF NOT EXISTS) so the migration is replay-safe under the BEGIN IMMEDIATE
-- race path documented in schema.rs::migrate().

CREATE TABLE IF NOT EXISTS token_usage (
    id                          INTEGER PRIMARY KEY,
    ts                          TEXT NOT NULL,
    source                      TEXT NOT NULL CHECK (source IN ('llm_rerank','pref_extract','transcript','mcp_response')),
    harness                     TEXT NOT NULL CHECK (harness IN ('claude','codex')),
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
    cost_usd                    REAL CHECK (cost_usd IS NULL OR cost_usd >= 0)
);

CREATE INDEX IF NOT EXISTS idx_token_usage_task_ts
    ON token_usage (task_tag, ts);
CREATE INDEX IF NOT EXISTS idx_token_usage_collab_phase
    ON token_usage (collab_session_id, collab_phase);

CREATE TABLE IF NOT EXISTS occupancy_samples (
    id                      INTEGER PRIMARY KEY,
    ts                      TEXT NOT NULL,
    harness                 TEXT NOT NULL CHECK (harness IN ('claude','codex')),
    session_id              TEXT,
    workspace_root          TEXT,
    hook_event              TEXT CHECK (hook_event IS NULL OR hook_event IN ('session-start','session-stop','precompact','user-prompt-submit')),
    input_tokens            INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    cache_read_input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cache_read_input_tokens >= 0),
    context_window          INTEGER NOT NULL DEFAULT 200000 CHECK (context_window > 0),
    occupancy_pct           REAL CHECK (occupancy_pct IS NULL OR occupancy_pct >= 0)
);

CREATE INDEX IF NOT EXISTS idx_occupancy_session_ts
    ON occupancy_samples (session_id, ts);

CREATE TABLE IF NOT EXISTS session_summary (
    session_id          TEXT PRIMARY KEY,
    harness             TEXT NOT NULL CHECK (harness IN ('claude','codex')),
    workspace_root      TEXT,
    started_at          TEXT,
    ended_at            TEXT,
    peak_occupancy_pct  REAL CHECK (peak_occupancy_pct IS NULL OR peak_occupancy_pct >= 0),
    total_input_tokens  INTEGER NOT NULL DEFAULT 0 CHECK (total_input_tokens >= 0),
    total_output_tokens INTEGER NOT NULL DEFAULT 0 CHECK (total_output_tokens >= 0),
    mcp_chars_served    INTEGER NOT NULL DEFAULT 0 CHECK (mcp_chars_served >= 0),
    compactions         INTEGER NOT NULL DEFAULT 0 CHECK (compactions >= 0)
);

CREATE TABLE IF NOT EXISTS task_outcomes (
    id                INTEGER PRIMARY KEY,
    task_tag          TEXT NOT NULL UNIQUE,
    collab_session_id TEXT,
    started_at        TEXT,
    done_at           TEXT,
    outcome           TEXT CHECK (outcome IS NULL OR outcome IN ('merged','failed','abandoned')),
    review_rounds     INTEGER NOT NULL DEFAULT 0 CHECK (review_rounds >= 0),
    fix_commits       INTEGER NOT NULL DEFAULT 0 CHECK (fix_commits >= 0),
    handoffs          INTEGER NOT NULL DEFAULT 0 CHECK (handoffs >= 0),
    pr_url            TEXT
);

CREATE INDEX IF NOT EXISTS idx_task_outcomes_collab
    ON task_outcomes (collab_session_id);

INSERT OR IGNORE INTO schema_version (version) VALUES (8);

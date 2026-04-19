-- Collab v2: post-PlanLocked coding loop.
--
-- Extends collab_sessions with the columns needed to track the per-task
-- 5-phase debate, the 2-pass global review, branch drift checks, and the
-- PR handoff. The session that reached PlanLocked in v1 is reused here — no
-- new row is created for the coding phase.
--
-- All new columns are nullable / default-zero so existing v1 sessions (which
-- never leave PlanLocked) remain valid after this migration.
--
-- CHECK length bounds mirror the application-layer caps so direct inserts
-- (migrations, test helpers, future maintenance scripts) cannot bypass them.
-- Git SHAs are bounded generously (64 chars for SHA-256 forward-compat);
-- pr_url is capped at 2 KiB and coding_failure at the same cap that the MCP
-- layer enforces pre-store.
ALTER TABLE collab_sessions ADD COLUMN task_list           TEXT;
ALTER TABLE collab_sessions ADD COLUMN current_task_index  INTEGER;
ALTER TABLE collab_sessions ADD COLUMN task_review_round   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE collab_sessions ADD COLUMN global_review_round INTEGER NOT NULL DEFAULT 0;
ALTER TABLE collab_sessions ADD COLUMN base_sha            TEXT
    CHECK (base_sha IS NULL OR length(base_sha) <= 64);
ALTER TABLE collab_sessions ADD COLUMN last_head_sha       TEXT
    CHECK (last_head_sha IS NULL OR length(last_head_sha) <= 64);
ALTER TABLE collab_sessions ADD COLUMN pr_url              TEXT
    CHECK (pr_url IS NULL OR length(pr_url) <= 2048);
ALTER TABLE collab_sessions ADD COLUMN coding_failure      TEXT
    CHECK (coding_failure IS NULL OR length(coding_failure) <= 2048);

-- wait_my_turn polls (current_owner, phase, task_list) every 500 ms per active
-- session. This composite index keeps the poll path O(1) once multiple
-- concurrent sessions exist.
CREATE INDEX IF NOT EXISTS idx_collab_sessions_owner_phase
    ON collab_sessions(current_owner, phase);

INSERT OR IGNORE INTO schema_version (version) VALUES (5);

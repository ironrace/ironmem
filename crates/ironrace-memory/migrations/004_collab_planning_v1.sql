-- Planning protocol v1 final: task description stored on the session,
-- bounded review round counter, and an ended_at timestamp used by the
-- (future v2) coding phase to close out a session once planning +
-- implementation are done.
ALTER TABLE collab_sessions ADD COLUMN task TEXT;
ALTER TABLE collab_sessions ADD COLUMN review_round INTEGER NOT NULL DEFAULT 0;
ALTER TABLE collab_sessions ADD COLUMN ended_at TEXT;

-- Safety net: any PlanEscalated rows left over from the v1 prototype are
-- collapsed to PlanLocked. v1 final drops PlanEscalated entirely since the
-- revision cap guarantees Claude always writes the final plan.
UPDATE collab_sessions SET phase = 'PlanLocked' WHERE phase = 'PlanEscalated';

INSERT OR IGNORE INTO schema_version (version) VALUES (4);

-- Migration 020: first-class collab implementation checkpoints (issue #273).
--
-- Before this migration a checkpoint was an agent-side *convention*: the
-- implementer wrote a `collab-checkpoint:<session_id>` drawer via `add_drawer`
-- and nothing verified it. A controller could commit 28 changes and hand off
-- while the drawer still described task 1, and the server would accept the
-- handoff and present the stale drawer as current progress. That made crash
-- recovery unsafe and the operator-facing progress report materially false.
--
-- This table makes the checkpoint a protocol object the server can enforce
-- against. `session_id` is the PRIMARY KEY, not an autoincrement id, because
-- the contract is "exactly one *current* checkpoint per session" — the same
-- one-logical-keyed-drawer semantics the convention had, now enforced by the
-- schema instead of by prompt discipline. It is explicitly `NOT NULL` in
-- addition to `PRIMARY KEY`: unlike `INTEGER PRIMARY KEY` or a
-- `WITHOUT ROWID` table, plain SQLite does not treat a `TEXT PRIMARY KEY` as
-- implicitly NOT NULL, so without this a direct write could insert an
-- orphaned NULL-keyed row that a FK cascade can never reach and no
-- `WHERE session_id = ?` lookup can ever find. This migration adds the table
-- and nothing else: as of it there is no writer, so a reader who finds no
-- checkpoint rows and no checkpoint entries in `wal_log` is looking at an
-- unbuilt feature, not a bug. Writes will go through
-- `queue::upsert_checkpoint` (INSERT … ON CONFLICT DO UPDATE); history will be
-- the git log and the `wal_log` audit trail, subject to its retention window,
-- deliberately not a second table, because a checkpoint ledger nobody reads
-- is exactly the kind of drawer accumulation `ironmem memory gc` exists to
-- prune.
--
--   task_id / task_title      the task the checkpoint describes; NULL for a
--                             `batch_complete` checkpoint, which describes the
--                             batch rather than any single task.
--   status                    started | completed | blocked | batch_complete.
--                             CHECK-constrained so a direct SQL write cannot
--                             park a session on a status the state machine has
--                             no handling for.
--   head_sha                  the repo HEAD the checkpoint was taken at. This
--                             is the column the whole issue turns on: the
--                             divergence check compares it against live git
--                             HEAD.
--   commit_sha                the commit this task produced, when the status is
--                             `completed`. NULL otherwise.
--   completed_task_ids        comma-separated, cumulative, carried forward on
--                             every write. Empty string (not NULL) when nothing
--                             has completed yet, so the "no tasks done" case is
--                             distinguishable from a legacy/absent value.
--   next_task_id              resume pointer; NULL at `batch_complete`.
--   gates_result              not_run | passed | failed: <reason>. Free text
--                             after the prefix, so NOT CHECK-constrained.
--   gates_sha                 the HEAD the gates actually ran against. Distinct
--                             from head_sha on purpose: gates passing at an
--                             older SHA than the checkpoint's HEAD is precisely
--                             the "gates are stale" case the
--                             `implementation_done` gate must catch.
--   gates_commands            the exact gate command set, " && "-joined, so a
--                             resumer can tell a changed gate set from a
--                             reusable gate proof.
--   attested_by               implementer | operator. `operator` marks a
--                             human-attested backfill over a divergence the
--                             protocol never witnessed. The distinction is
--                             auditable precisely because it is stored, not
--                             inferred.
--   acknowledged_divergence   NULL for every implementer-attested checkpoint.
--                             For an operator attestation it records the SHA
--                             range being vouched for, as `<from>..<to>`. The
--                             CHECK below pins the correlation at the DB level:
--                             an implementer row can never carry one. The
--                             CHECK is intentionally one-directional — it
--                             permits `attested_by='operator'` with this
--                             column still NULL. Requiring operator writes to
--                             populate it is a tool-layer rule, not a schema
--                             guarantee; a reader must not conflate the two.
--   updated_at                unix seconds, NOT the `TEXT`/`datetime('now')`
--                             convention every other `_at` column in this
--                             schema uses (collab_sessions.updated_at,
--                             messages.created_at, code_maps.built_at,
--                             schema_version.applied_at). Deliberately the one
--                             integer timestamp in the schema: the value is
--                             server-stamped for programmatic comparison
--                             (divergence checks, staleness), not display, and
--                             a TEXT column with a DEFAULT invites a writer
--                             that forgot to pass it to get a silently-filled
--                             timestamp anyway — exactly the
--                             unverified-bookkeeping failure mode this table
--                             exists to kill.
--
-- CREATE TABLE is idempotent under IF NOT EXISTS, but this migration is still
-- version-gated behind `current_version < 20` in schema.rs::migrate() for
-- consistency with the rest of the ladder.

CREATE TABLE IF NOT EXISTS collab_checkpoints (
    session_id              TEXT NOT NULL PRIMARY KEY
                            REFERENCES collab_sessions(id) ON DELETE CASCADE,
    task_id                 INTEGER,
    task_title              TEXT,
    status                  TEXT NOT NULL
                            CHECK (status IN ('started', 'completed', 'blocked', 'batch_complete')),
    head_sha                TEXT NOT NULL,
    commit_sha              TEXT,
    completed_task_ids      TEXT NOT NULL DEFAULT '',
    next_task_id            INTEGER,
    gates_result            TEXT NOT NULL DEFAULT 'not_run',
    gates_sha               TEXT,
    gates_commands          TEXT,
    summary                 TEXT,
    attested_by             TEXT NOT NULL DEFAULT 'implementer'
                            CHECK (attested_by IN ('implementer', 'operator')),
    acknowledged_divergence TEXT,
    updated_at              INTEGER NOT NULL,
    CHECK (acknowledged_divergence IS NULL OR attested_by = 'operator')
);

INSERT OR IGNORE INTO schema_version (version) VALUES (20);

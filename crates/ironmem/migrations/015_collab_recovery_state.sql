-- Migration 015: recoverable tooling-failure state on collab_sessions (issue #197).
--
-- Collab tooling failures (an MCP call erroring mid-phase, a dropped daemon
-- connection, etc.) currently have no persisted record — the session just
-- stalls with no trace of what broke or who is trying to recover it. These
-- seven nullable columns let a session capture that state without disturbing
-- the existing phase/owner machinery:
--
--   pending_failure         the verbatim `coding_failure` diagnostic of the
--                           in-flight recoverable failure — the reported
--                           string itself, NOT the FailureClass it classifies
--                           as. NULL when no failure is pending. Callers
--                           re-derive the class via failure_class.rs.
--   failed_from_phase       the `Phase` the session was in when the failure
--                           was recorded, so recovery can resume in place.
--   recovery_phase          the phase the in-flight recovery is scoped to —
--                           i.e. the interrupted phase the session stays
--                           parked in. Not a sub-phase of a separate recovery
--                           flow: while recovery is live this equals the
--                           session's own `phase` column, and the delegated
--                           completion is admitted only on that equality.
--   recovery_owner          which `Agent` currently drives recovery.
--   recovery_origin_owner   which `Agent` owned the interrupted turn when the
--                           failure occurred. Attribution only — control is
--                           NOT handed back to it; the recovery owner
--                           completes the turn itself and the phase's normal
--                           completion event picks the next owner.
--   recovery_attempts       handoffs made against the current resume budget;
--                           reset to 0 by a successful delegated completion
--                           and by collab_resume. NULL on legacy (pre-015)
--                           rows, read back as a default of 0 by the
--                           application layer.
--   total_recovery_attempts handoffs made over the session's entire lifetime.
--                           Monotonic — reset by nothing, including
--                           collab_resume — so it, not recovery_attempts, is
--                           what bounds a session. Same NULL/0 legacy
--                           treatment.
--
-- All seven are NULL for every existing row (no failure in flight) and stay
-- NULL for the common case going forward — recovery state is the exception,
-- not the rule.
--
-- `ALTER TABLE … ADD COLUMN` is NOT idempotent (it errors with
-- `duplicate column` on replay), so — exactly like 009/014 — this migration
-- is version-gated in schema.rs::migrate() behind `current_version < 15` and
-- runs exactly once under the BEGIN IMMEDIATE write lock.
--
-- Amended-in-place caveat (read this if a v15 DB is missing a column below).
-- The last two columns — recovery_origin_owner and total_recovery_attempts —
-- were added to THIS file during issue #197 review rather than as a new
-- migration 016. That was correct because 015 was still unreleased: no
-- published build had ever stamped schema_version 15, so no database could be
-- gated past it. The risk it leaves behind: any database that ran an earlier
-- development build of this branch already sits at version 15, the
-- `current_version < 15` gate is false for it, and it will therefore NEVER
-- receive the two late columns — reads fail with `no such column` rather than
-- degrading to NULL. That population is development-only and the fix is to
-- delete the local DB (or hand-apply the two ALTERs and leave the version
-- alone). Once this branch ships, 015 is frozen: every subsequent column goes
-- in a new numbered migration with its own version gate, no exceptions.

ALTER TABLE collab_sessions ADD COLUMN pending_failure TEXT;
ALTER TABLE collab_sessions ADD COLUMN failed_from_phase TEXT;
ALTER TABLE collab_sessions ADD COLUMN recovery_phase TEXT;
ALTER TABLE collab_sessions ADD COLUMN recovery_owner TEXT;
ALTER TABLE collab_sessions ADD COLUMN recovery_origin_owner TEXT;
ALTER TABLE collab_sessions ADD COLUMN recovery_attempts INTEGER;
ALTER TABLE collab_sessions ADD COLUMN total_recovery_attempts INTEGER;

INSERT OR IGNORE INTO schema_version (version) VALUES (15);

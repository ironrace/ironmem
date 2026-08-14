-- Migration 021: record what the server actually established about an
-- operator attestation's acknowledged range (issue #273 Task 10).
--
-- Migration 020 stores `attested_by` and `acknowledged_divergence` and nothing
-- about whether either was ever checked. The tool layer resolves the range
-- against the repository at write time — endpoints exist, the range ends at the
-- checkpoint's own head_sha, it covers at least one commit, and it spans the
-- gap left by the checkpoint it replaces — but two of those checks are
-- deliberately skippable, because a legitimate attestation must stay writable
-- when the repository momentarily cannot answer:
--
--   * live HEAD unreadable            -> only the range's syntax was checked
--   * the previous checkpoint's head  -> the endpoints held, but coverage of
--     unresolvable, or not an              the gap was not established
--     ancestor of the new head
--
-- Without this column that verdict lives only in the write response (already
-- consumed by the time anyone reads the row) and the wal_log detail blob. Every
-- reader — `session_handoff`, `collab_status`, `collab_resume` — would then
-- render `attested_by: operator` and a range that reads exactly like a checked
-- one. That is the unverified-claim-presented-as-verified failure this whole
-- issue exists to end, arriving one layer below the checkpoint itself.
--
--   attestation_check   NULL | verified | verified_without_span |
--                       unverified_repo_unreadable.
--
--                       NULL means "no verdict was recorded": every
--                       implementer-attested row (there is no range to
--                       resolve), and any row written before this migration.
--                       Readers MUST render NULL on an *operator* row as
--                       unchecked rather than as absent — see
--                       `CollabCheckpoint::attestation_verdict`, which is the
--                       single statement of that fail-safe default. Storing
--                       "not applicable" as its own literal was rejected for
--                       exactly this reason: it would make the pre-migration
--                       and the implementer cases indistinguishable from a
--                       positive finding.
--
-- The value is server-derived, never caller-supplied, in the same way
-- `updated_at` is: `CollabCheckpoint::from_json` leaves it NULL and the MCP
-- handler stamps it from its own git reads. A caller therefore cannot label its
-- own attestation `verified`.
--
-- The vocabulary is CHECK-constrained here and parsed by
-- `AttestationCheck::from_str` on the load path, the same belt-and-braces
-- `status` and `attested_by` get in 020;
-- `attestation_check_variants_match_migration_021` pins the two lists
-- together. The correlation "only an operator row may carry a verdict" is NOT
-- a schema constraint: SQLite's ALTER TABLE ADD COLUMN cannot add a
-- table-level CHECK spanning two columns, so that rule lives in
-- `CollabCheckpoint::validate` alone. A reader must not mistake the CHECK
-- below for enforcing it.
--
-- A separate migration rather than an edit to 020: 020 has shipped far enough
-- that a development database can already report schema_version 20, and its
-- `CREATE TABLE IF NOT EXISTS` would silently skip such a database, leaving it
-- with a table this code then queries a missing column on. The ladder is the
-- mechanism that makes adding a column to an existing table safe.

ALTER TABLE collab_checkpoints ADD COLUMN attestation_check TEXT
    CHECK (attestation_check IS NULL
           OR attestation_check IN ('verified',
                                    'verified_without_span',
                                    'unverified_repo_unreadable'));

INSERT OR IGNORE INTO schema_version (version) VALUES (21);

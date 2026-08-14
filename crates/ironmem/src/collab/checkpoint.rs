//! `CollabCheckpoint` — the durable, server-verifiable record of how far the
//! v3 batch implementation has actually got.
//!
//! Before issue #273 this was a `collab-checkpoint:<session_id>` drawer written
//! by prompt convention and verified by nothing. The type here is the parsed,
//! validated shape of that same record, now backed by the `collab_checkpoints`
//! table (migration 020) and demanded as proof by `implementation_done`.
//!
//! This module only *models and validates* a checkpoint. Persistence lives in
//! [`crate::collab::queue`], and the git-HEAD comparison lives in the MCP
//! layer — the type deliberately knows nothing about git or SQL, so it stays a
//! pure parse/validate unit.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde_json::Value;

/// How far the batch has got, as recorded at a task boundary.
///
/// The variants and their [`CheckpointStatus::as_str`] spellings are one half
/// of a contract whose other half is migration 020's
/// `CHECK (status IN (...))`; `status_variants_match_migration_020` pins them
/// together, because a variant the SQL rejects fails on a live session
/// mid-batch rather than in CI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointStatus {
    /// Work on `task_id` has begun and has not yet been committed.
    Started,
    /// `task_id` is implemented, reviewed, committed, and pushed.
    Completed,
    /// `task_id` hit an unrecoverable failure; the batch cannot continue.
    Blocked,
    /// Every task is done and the gates were run. The only status
    /// `implementation_done` accepts.
    BatchComplete,
}

impl CheckpointStatus {
    /// The wire and storage spelling. Must appear verbatim in migration 020's
    /// `status` CHECK list.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::BatchComplete => "batch_complete",
        }
    }
}

impl fmt::Display for CheckpointStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CheckpointStatus {
    type Err = CheckpointError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "started" => Ok(Self::Started),
            "completed" => Ok(Self::Completed),
            "blocked" => Ok(Self::Blocked),
            "batch_complete" => Ok(Self::BatchComplete),
            other => Err(CheckpointError(format!(
                "status must be one of started|completed|blocked|batch_complete, got {other:?}"
            ))),
        }
    }
}

/// Who vouches for this checkpoint. An `Operator` attestation is the *only*
/// way a checkpoint may cover commits the protocol did not witness, and it
/// must name the range it is vouching for.
///
/// Like [`CheckpointStatus`], the spellings are pinned to migration 020's
/// `attested_by` CHECK list by a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttestedBy {
    /// The implementing agent's own record of work the protocol witnessed.
    /// The default, and the only attestation an agent can produce for itself.
    Implementer,
    /// A human vouching for a divergence the protocol never witnessed — the
    /// deliberate, auditable escape hatch from the head-consistency gate.
    Operator,
}

impl AttestedBy {
    /// The wire and storage spelling. Must appear verbatim in migration 020's
    /// `attested_by` CHECK list.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Implementer => "implementer",
            Self::Operator => "operator",
        }
    }
}

impl FromStr for AttestedBy {
    type Err = CheckpointError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "implementer" => Ok(Self::Implementer),
            "operator" => Ok(Self::Operator),
            other => Err(CheckpointError(format!(
                "attested_by must be implementer|operator, got {other:?}"
            ))),
        }
    }
}

/// A checkpoint that failed validation. A newtype rather than a bare `String`
/// so the MCP layer converts it once, at the boundary, into
/// `MemoryError::Validation`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointError(pub String);

impl fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CheckpointError {}

/// One session's current checkpoint: the parsed, validated form of a
/// `collab_checkpoints` row.
///
/// The field set mirrors the table in migration 020 column for column, so this
/// type is what both the MCP tool payload and the stored row round-trip
/// through. Every field is `pub` because the type carries no invariant that
/// survives construction — [`CollabCheckpoint::from_json`] is where validation
/// happens, and callers that build one field-by-field (the loader in
/// `collab::queue`) are reconstructing an already-validated row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollabCheckpoint {
    pub session_id: String,
    pub task_id: Option<u32>,
    pub task_title: Option<String>,
    pub status: CheckpointStatus,
    /// The repo HEAD this checkpoint was taken at. The divergence check
    /// compares this against live git HEAD; it is the field the whole issue
    /// turns on.
    pub head_sha: String,
    pub commit_sha: Option<String>,
    /// Cumulative and carried forward on every write. Deduplicated and sorted
    /// at parse time so coverage arithmetic cannot be fooled by repeats.
    pub completed_task_ids: Vec<u32>,
    pub next_task_id: Option<u32>,
    /// `not_run`, `passed`, or `failed: <reason>` — free text after the
    /// prefix, which is why it is a `String` rather than an enum.
    pub gates_result: String,
    /// The HEAD the gates actually ran against, distinct from `head_sha` on
    /// purpose. See [`CollabCheckpoint::gates_are_green_at_head`].
    pub gates_sha: Option<String>,
    /// The exact gate command set, `" && "`-joined, so a resumer can tell a
    /// changed gate set from a reusable gate proof.
    pub gates_commands: Option<String>,
    pub summary: Option<String>,
    pub attested_by: AttestedBy,
    /// The SHA range an operator is vouching for, as `<from>..<to>`. Always
    /// `None` for an implementer attestation — see
    /// [`CollabCheckpoint::from_json`].
    pub acknowledged_divergence: Option<String>,
    /// Unix seconds, server-stamped at write time rather than parsed from the
    /// payload.
    pub updated_at: i64,
}

impl CollabCheckpoint {
    /// Parse and validate a checkpoint from the MCP tool payload.
    ///
    /// Absent optional fields may arrive either omitted, JSON `null`, or as
    /// the literal string `"none"` — the last because the collab turn
    /// templates spell absent values `<N|none>` (see the checkpoint block in
    /// `docs/COLLAB.md`), and an agent transcribing that template will send
    /// the sentinel rather than dropping the key.
    ///
    /// `updated_at` is deliberately NOT read from the payload — the server
    /// stamps it at write time in `queue::upsert_checkpoint`, so a caller
    /// cannot backdate a checkpoint to make a stale one look fresh.
    pub fn from_json(value: &Value) -> Result<Self, CheckpointError> {
        let session_id = require_str(value, "session_id")?;
        let head_sha = require_str(value, "head_sha")?;
        let status = CheckpointStatus::from_str(&require_str(value, "status")?)?;

        let attested_by = AttestedBy::from_str(str_or_default(
            value,
            "attested_by",
            AttestedBy::Implementer.as_str(),
        )?)?;
        let acknowledged_divergence = optional_string(value, "acknowledged_divergence");

        // Enforced here as well as by the migration-020 CHECK so the caller
        // gets a validation message rather than a raw SQL error. The operator
        // direction is *only* enforced here: the schema's CHECK is
        // deliberately one-directional and permits an operator row with a NULL
        // range.
        match (attested_by, acknowledged_divergence.as_deref()) {
            (AttestedBy::Implementer, Some(_)) => {
                return Err(CheckpointError(
                    "acknowledged_divergence is only valid with attested_by=operator: an \
                     implementer cannot self-attest over commits the protocol never witnessed"
                        .to_string(),
                ));
            }
            (AttestedBy::Operator, None) => {
                return Err(CheckpointError(
                    "attested_by=operator requires acknowledged_divergence naming the range \
                     being vouched for, as <from_sha>..<to_sha>"
                        .to_string(),
                ));
            }
            _ => {}
        }

        Ok(Self {
            session_id,
            task_id: optional_u32(value, "task_id")?,
            task_title: optional_string(value, "task_title"),
            status,
            head_sha,
            commit_sha: optional_string(value, "commit_sha"),
            completed_task_ids: parse_completed_task_ids(str_or_default(
                value,
                "completed_task_ids",
                "",
            )?)?,
            next_task_id: optional_u32(value, "next_task_id")?,
            gates_result: str_or_default(value, "gates_result", "not_run")?.to_string(),
            gates_sha: optional_string(value, "gates_sha"),
            gates_commands: optional_string(value, "gates_commands"),
            summary: optional_string(value, "summary"),
            attested_by,
            acknowledged_divergence,
            updated_at: 0,
        })
    }

    /// Whether this checkpoint carries a *reusable* gate proof: the gates
    /// passed AND they ran at exactly this checkpoint's HEAD.
    ///
    /// The SHA equality is the load-bearing half. Gates green at an older SHA
    /// means commits landed after the last green run — the proof describes a
    /// tree that no longer exists.
    ///
    /// This is only half of the reuse rule in `docs/COLLAB.md`: the caller
    /// still has to compare `head_sha` against live git HEAD and match
    /// `gates_commands` against the current required gate set, neither of
    /// which this type can see.
    pub fn gates_are_green_at_head(&self) -> bool {
        self.gates_result.starts_with("passed")
            && self.gates_sha.as_deref() == Some(self.head_sha.as_str())
    }

    /// Whether `completed_task_ids` covers every task id in `1..=total`.
    ///
    /// Checks set membership rather than length: `1,2,4` with `total = 3` has
    /// the right count and the wrong contents, and must not pass. A `total` of
    /// zero is not "vacuously covered" — a batch with no tasks is a
    /// malformed task list, not a finished one.
    pub fn covers_all_tasks(&self, total: u32) -> bool {
        if total == 0 {
            return false;
        }
        let present: BTreeSet<u32> = self.completed_task_ids.iter().copied().collect();
        (1..=total).all(|id| present.contains(&id))
    }
}

/// Read a mandatory string field. Whitespace-only is treated as absent rather
/// than as a value, so a checkpoint can never claim a `head_sha` of `" "`.
fn require_str(value: &Value, field: &str) -> Result<String, CheckpointError> {
    let raw = value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if raw.is_empty() {
        return Err(CheckpointError(format!(
            "{field} is required and must be a non-empty string"
        )));
    }
    Ok(raw.to_string())
}

/// Read a string field that falls back to `default` when absent or `null`,
/// but *rejects* a present value of some other JSON type.
///
/// The rejection is the point. `completed_task_ids` is a comma-separated
/// string on the wire, and the obvious caller mistake is sending
/// `[1, 2, 3]` instead; silently reading that as `""` would persist an empty
/// cumulative list over a real one and hand the next resumer a checkpoint
/// claiming no task ever finished. This module exists because unverified
/// bookkeeping was believed once already.
fn str_or_default<'a>(
    value: &'a Value,
    field: &str,
    default: &'a str,
) -> Result<&'a str, CheckpointError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(CheckpointError(format!("{field} must be a string"))),
    }
}

/// Read an optional string field, treating absent, empty, and the template's
/// `"none"` sentinel alike. Nothing here can fail: an optional string field
/// has no shape to violate, so the absent case is a `None`, not an error.
fn optional_string(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "none")
        .map(str::to_string)
}

/// Read an optional task id, accepting both a JSON number and the string form
/// an agent transcribing the `<N|none>` template will produce.
fn optional_u32(value: &Value, field: &str) -> Result<Option<u32>, CheckpointError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() || s.trim() == "none" => Ok(None),
        Some(Value::String(s)) => s
            .trim()
            .parse::<u32>()
            .map(Some)
            .map_err(|_| CheckpointError(format!("{field} must be a non-negative integer"))),
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .map(Some)
            .ok_or_else(|| CheckpointError(format!("{field} must be a non-negative integer"))),
        Some(_) => Err(CheckpointError(format!(
            "{field} must be a non-negative integer"
        ))),
    }
}

/// Parse the cumulative `completed_task_ids` list from its wire form (a
/// comma-separated string), normalizing to a deduplicated, sorted vec so that
/// coverage arithmetic in [`CollabCheckpoint::covers_all_tasks`] cannot be
/// inflated by repeated ids.
fn parse_completed_task_ids(raw: &str) -> Result<Vec<u32>, CheckpointError> {
    let mut ids = BTreeSet::new();
    for piece in raw.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let id = piece.parse::<u32>().map_err(|_| {
            CheckpointError(format!(
                "completed_task_ids must be a comma-separated list of integers, got {piece:?}"
            ))
        })?;
        ids.insert(id);
    }
    Ok(ids.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Migration 020 is the other half of this module's vocabulary: its
    /// `CHECK (... IN (...))` lists and the enums here must name exactly the
    /// same strings. Read at compile time so the lockstep tests below cannot
    /// drift from the migration that actually ships.
    const MIGRATION_020: &str = include_str!("../../migrations/020_collab_checkpoints.sql");

    /// Every [`CheckpointStatus`] variant. The exhaustiveness guard in
    /// [`status_variants_match_migration_020`] is what keeps this list honest:
    /// adding a variant to the enum without adding it here stops that test
    /// compiling.
    const ALL_STATUSES: &[CheckpointStatus] = &[
        CheckpointStatus::Started,
        CheckpointStatus::Completed,
        CheckpointStatus::Blocked,
        CheckpointStatus::BatchComplete,
    ];

    /// Every [`AttestedBy`] variant. See [`ALL_STATUSES`] for why the list is
    /// spelled out rather than derived.
    const ALL_ATTESTATIONS: &[AttestedBy] = &[AttestedBy::Implementer, AttestedBy::Operator];

    /// Pull the quoted literals out of `CHECK (<column> IN ('a', 'b'))` in the
    /// migration source. Deliberately a dumb textual scan rather than a SQL
    /// parse: the point is to read the same bytes SQLite will, so a typo in the
    /// migration shows up as a missing literal here.
    fn check_in_list(column: &str) -> Vec<String> {
        let needle = format!("CHECK ({column} IN (");
        let start = MIGRATION_020
            .find(&needle)
            .unwrap_or_else(|| panic!("migration 020 has no `CHECK ({column} IN (...))` clause"))
            + needle.len();
        let end = start
            + MIGRATION_020[start..]
                .find(')')
                .unwrap_or_else(|| panic!("unterminated `CHECK ({column} IN (` in migration 020"));
        MIGRATION_020[start..end]
            .split(',')
            .map(|piece| piece.trim().trim_matches('\'').to_string())
            .collect()
    }

    fn valid() -> serde_json::Value {
        json!({
            "session_id": "s1",
            "task_id": 3,
            "task_title": "Add the gate",
            "status": "completed",
            "head_sha": "abc123",
            "commit_sha": "abc123",
            "completed_task_ids": "1,2,3",
            "next_task_id": 4,
            "gates_result": "passed",
            "gates_sha": "abc123",
            "gates_commands": "cargo fmt --all -- --check && cargo test --workspace",
            "summary": "Task 3 done"
        })
    }

    #[test]
    fn parses_a_complete_checkpoint() {
        let cp = CollabCheckpoint::from_json(&valid()).unwrap();
        assert_eq!(cp.status, CheckpointStatus::Completed);
        assert_eq!(cp.completed_task_ids, vec![1, 2, 3]);
        assert_eq!(cp.attested_by, AttestedBy::Implementer);
        assert_eq!(cp.acknowledged_divergence, None);
    }

    #[test]
    fn completed_task_ids_accepts_an_empty_list() {
        let mut v = valid();
        v["completed_task_ids"] = json!("");
        assert!(CollabCheckpoint::from_json(&v)
            .unwrap()
            .completed_task_ids
            .is_empty());
    }

    #[test]
    fn completed_task_ids_rejects_non_numeric_entries() {
        let mut v = valid();
        v["completed_task_ids"] = json!("1,two,3");
        let err = CollabCheckpoint::from_json(&v).unwrap_err();
        assert!(err.to_string().contains("completed_task_ids"));
    }

    /// A JSON array is the obvious way to get `completed_task_ids` wrong, and
    /// reading it as an empty list would persist "no task ever finished" over
    /// a real cumulative record. It has to be a rejection, not a fallback.
    #[test]
    fn completed_task_ids_rejects_a_non_string_value() {
        let mut v = valid();
        v["completed_task_ids"] = json!([1, 2, 3]);
        let err = CollabCheckpoint::from_json(&v).unwrap_err();
        assert!(err.to_string().contains("completed_task_ids"), "got: {err}");
    }

    #[test]
    fn head_sha_is_required_and_non_empty() {
        let mut v = valid();
        v["head_sha"] = json!("");
        assert!(CollabCheckpoint::from_json(&v)
            .unwrap_err()
            .to_string()
            .contains("head_sha"));
    }

    #[test]
    fn unknown_status_is_rejected() {
        let mut v = valid();
        v["status"] = json!("nearly_done");
        assert!(CollabCheckpoint::from_json(&v)
            .unwrap_err()
            .to_string()
            .contains("status"));
    }

    /// D1: an implementer may never self-attest over a divergence. Only an
    /// operator attestation may carry `acknowledged_divergence`, and the
    /// parser — not just the DB CHECK — refuses the combination, so the
    /// error surfaces as a validation message rather than a SQL error.
    #[test]
    fn implementer_attestation_cannot_acknowledge_divergence() {
        let mut v = valid();
        v["acknowledged_divergence"] = json!("aaa..bbb");
        let err = CollabCheckpoint::from_json(&v).unwrap_err();
        assert!(
            err.to_string().contains("acknowledged_divergence"),
            "got: {err}"
        );
    }

    #[test]
    fn operator_attestation_requires_an_acknowledged_range() {
        let mut v = valid();
        v["attested_by"] = json!("operator");
        let err = CollabCheckpoint::from_json(&v).unwrap_err();
        assert!(
            err.to_string().contains("acknowledged_divergence"),
            "got: {err}"
        );
    }

    #[test]
    fn operator_attestation_with_a_range_parses() {
        let mut v = valid();
        v["attested_by"] = json!("operator");
        v["acknowledged_divergence"] = json!("b9c2ce0..75a4ea3");
        let cp = CollabCheckpoint::from_json(&v).unwrap();
        assert_eq!(cp.attested_by, AttestedBy::Operator);
        assert_eq!(
            cp.acknowledged_divergence.as_deref(),
            Some("b9c2ce0..75a4ea3")
        );
    }

    /// The gate proof is only reusable when the gates ran at the very SHA the
    /// checkpoint describes. Gates green at an *older* SHA is the stale-proof
    /// case that `implementation_done` must reject.
    #[test]
    fn gates_are_green_only_when_they_ran_at_this_head() {
        let cp = CollabCheckpoint::from_json(&valid()).unwrap();
        assert!(cp.gates_are_green_at_head());

        let mut stale = valid();
        stale["gates_sha"] = json!("older99");
        assert!(!CollabCheckpoint::from_json(&stale)
            .unwrap()
            .gates_are_green_at_head());

        let mut failed = valid();
        failed["gates_result"] = json!("failed: 3 tests red");
        assert!(!CollabCheckpoint::from_json(&failed)
            .unwrap()
            .gates_are_green_at_head());

        let mut unrun = valid();
        unrun["gates_result"] = json!("not_run");
        unrun["gates_sha"] = json!(null);
        assert!(!CollabCheckpoint::from_json(&unrun)
            .unwrap()
            .gates_are_green_at_head());
    }

    #[test]
    fn covers_all_tasks_requires_every_id_from_one_to_total() {
        let cp = CollabCheckpoint::from_json(&valid()).unwrap(); // 1,2,3
        assert!(cp.covers_all_tasks(3));
        assert!(!cp.covers_all_tasks(4));

        // A gap must not pass just because the count matches.
        let mut gapped = valid();
        gapped["completed_task_ids"] = json!("1,2,4");
        assert!(!CollabCheckpoint::from_json(&gapped)
            .unwrap()
            .covers_all_tasks(3));
    }

    #[test]
    fn duplicate_ids_do_not_inflate_coverage() {
        let mut dupes = valid();
        dupes["completed_task_ids"] = json!("1,2,2,2");
        assert!(!CollabCheckpoint::from_json(&dupes)
            .unwrap()
            .covers_all_tasks(3));
    }

    /// The Rust enum and migration 020's `CHECK` list are two independent
    /// statements of the same vocabulary, and nothing else couples them. A
    /// variant added here but not there fails as a constraint violation on a
    /// *live* session mid-batch, at the moment a checkpoint is written — the
    /// worst possible time and place to discover it. This test moves that
    /// failure to compile/test time, in both directions.
    #[test]
    fn status_variants_match_migration_020() {
        let sql_values = check_in_list("status");

        for status in ALL_STATUSES {
            // Exhaustiveness guard: a new variant stops this match compiling,
            // which forces it into ALL_STATUSES and so into the assertion.
            match status {
                CheckpointStatus::Started
                | CheckpointStatus::Completed
                | CheckpointStatus::Blocked
                | CheckpointStatus::BatchComplete => {}
            }
            assert!(
                sql_values.iter().any(|v| v == status.as_str()),
                "CheckpointStatus::{status:?} serializes to {:?}, which migration 020's \
                 CHECK (status IN ...) list {sql_values:?} would reject",
                status.as_str()
            );
        }

        // And the other direction: a value SQL permits but no variant parses
        // would let a hand-written row load as an unrepresentable status.
        for value in &sql_values {
            assert!(
                value.parse::<CheckpointStatus>().is_ok(),
                "migration 020 permits status {value:?}, which CheckpointStatus cannot parse"
            );
        }
    }

    /// The `attested_by` half of the same lockstep contract. See
    /// [`status_variants_match_migration_020`] for why it is enforced here
    /// rather than left to the database.
    #[test]
    fn attested_by_variants_match_migration_020() {
        let sql_values = check_in_list("attested_by");

        for attestation in ALL_ATTESTATIONS {
            match attestation {
                AttestedBy::Implementer | AttestedBy::Operator => {}
            }
            assert!(
                sql_values.iter().any(|v| v == attestation.as_str()),
                "AttestedBy::{attestation:?} serializes to {:?}, which migration 020's \
                 CHECK (attested_by IN ...) list {sql_values:?} would reject",
                attestation.as_str()
            );
        }

        for value in &sql_values {
            assert!(
                value.parse::<AttestedBy>().is_ok(),
                "migration 020 permits attested_by {value:?}, which AttestedBy cannot parse"
            );
        }
    }
}

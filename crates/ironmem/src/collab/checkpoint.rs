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

impl fmt::Display for AttestedBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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
/// through — which is why every field is `pub`: the loader in `collab::queue`
/// rebuilds one field-by-field from a row rather than from JSON. That open
/// construction is exactly why the cross-field rule lives in
/// [`CollabCheckpoint::validate`] rather than only inside
/// [`CollabCheckpoint::from_json`]; see that method for what every builder
/// owes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollabCheckpoint {
    pub session_id: String,
    pub task_id: Option<u32>,
    pub task_title: Option<String>,
    pub status: CheckpointStatus,
    /// The repo HEAD this checkpoint was taken at. The divergence check
    /// compares this against live git HEAD; it is the field the whole issue
    /// turns on. Required to be non-blank — see
    /// [`CollabCheckpoint::validate`], which is what makes that true of a
    /// struct built field-by-field rather than parsed, migration 020 having
    /// only `NOT NULL` on the column.
    pub head_sha: String,
    pub commit_sha: Option<String>,
    /// Cumulative and carried forward on every write, deduplicated and sorted
    /// at parse time so that equal progress stores as an equal string. Note
    /// that it is [`CollabCheckpoint::covers_all_tasks`]'s own set, not this
    /// normalization, that stops repeats inflating coverage.
    pub completed_task_ids: Vec<u32>,
    pub next_task_id: Option<u32>,
    /// `not_run`, `passed`, or `failed: <reason>` — free text after the
    /// prefix, which is why it is a `String` rather than an enum. Free text
    /// still is not *no* text: [`CollabCheckpoint::validate`] requires it
    /// non-blank, "nothing to say" being the `not_run` default rather than an
    /// empty string.
    pub gates_result: String,
    /// The HEAD the gates actually ran against, distinct from `head_sha` on
    /// purpose. See [`CollabCheckpoint::gates_are_green_at_head`].
    pub gates_sha: Option<String>,
    /// The exact gate command set, `" && "`-joined, so a resumer can tell a
    /// changed gate set from a reusable gate proof.
    pub gates_commands: Option<String>,
    pub summary: Option<String>,
    pub attested_by: AttestedBy,
    /// The SHA range an operator is vouching for, as `<from>..<to>`. It *must*
    /// be `None` for an implementer attestation and a *non-blank* `Some` for
    /// an operator one — `Some("")` being the same claim as `None` dressed to
    /// pass a presence check — but this being a `pub` field on a struct anyone
    /// can name, that is a rule rather than a guarantee: it holds only of a
    /// checkpoint that has been through [`CollabCheckpoint::validate`], which
    /// is why that method exists and why every builder owes a call to it.
    pub acknowledged_divergence: Option<String>,
    /// Unix seconds, server-stamped at write time rather than parsed from the
    /// payload. `0` means "not yet stamped": that is what
    /// [`CollabCheckpoint::from_json`] leaves here, and
    /// `queue::upsert_checkpoint` overwrites it unconditionally, so a `0` in
    /// hand means the checkpoint has not been through a write.
    pub updated_at: i64,
}

impl CollabCheckpoint {
    /// Parse and validate a checkpoint from the MCP tool payload.
    ///
    /// A caller with nothing to say for an optional field may omit it, send
    /// JSON `null`, or send the literal string `"none"` — see
    /// [`ABSENT_SENTINEL`]. `session_id`, `head_sha`, and `status` are
    /// mandatory and reject all three alike; `completed_task_ids` and
    /// `gates_result` are the two columns migration 020 gives a `NOT NULL
    /// DEFAULT`, so for them "nothing to say" means that default (`""` and
    /// `not_run`) rather than a `None`.
    ///
    /// `updated_at` is deliberately NOT read from the payload — the server
    /// stamps it at write time in `queue::upsert_checkpoint`, so a caller
    /// cannot backdate a checkpoint to make a stale one look fresh. That is a
    /// direct defense against the incident in issue #273, where a frozen
    /// checkpoint was presented as a current progress report.
    pub fn from_json(value: &Value) -> Result<Self, CheckpointError> {
        let checkpoint = Self {
            session_id: require_str(value, "session_id")?,
            task_id: optional_task_id(value, "task_id")?,
            task_title: optional_string(value, "task_title")?,
            status: CheckpointStatus::from_str(&require_str(value, "status")?)?,
            head_sha: require_str(value, "head_sha")?,
            commit_sha: optional_string(value, "commit_sha")?,
            completed_task_ids: parse_completed_task_ids(str_or_default(
                value,
                "completed_task_ids",
                "",
            )?)?,
            next_task_id: optional_task_id(value, "next_task_id")?,
            gates_result: str_or_default(value, "gates_result", "not_run")?.to_string(),
            gates_sha: optional_string(value, "gates_sha")?,
            gates_commands: optional_string(value, "gates_commands")?,
            summary: optional_string(value, "summary")?,
            attested_by: AttestedBy::from_str(str_or_default(
                value,
                "attested_by",
                AttestedBy::Implementer.as_str(),
            )?)?,
            acknowledged_divergence: optional_string(value, "acknowledged_divergence")?,
            // Not read from the payload; see this function's doc comment.
            updated_at: 0,
        };
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    /// Check the cross-field rules that hold for every checkpoint, however it
    /// was built.
    ///
    /// Separate from [`CollabCheckpoint::from_json`] because every field is
    /// `pub`: `queue::load_current_checkpoint` reconstructs a checkpoint
    /// field-by-field from a row and never goes through the parser, so without
    /// this it could hand Tasks 7-10 a combination the MCP path would have
    /// rejected. `task_list::validate_task_list_body` is factored out of its
    /// own parser for exactly this reason. **Both entry points must call it.**
    ///
    /// It enforces two things.
    ///
    /// **The required-field rules.** `session_id`, `head_sha`, and
    /// `gates_result` are the type's three non-`Option` `String` fields, and
    /// [`CollabCheckpoint::from_json`] can never produce a blank one: the
    /// first two go through [`require_str`], and the third through
    /// [`str_or_default`], which substitutes migration 020's `not_run` default
    /// exactly when the caller had nothing to say. So "empty, whitespace-only,
    /// or the [`ABSENT_SENTINEL`]" is a property of every *parsed* checkpoint,
    /// and a struct built field-by-field must be held to it too. `head_sha` is
    /// the field the whole issue turns on: migration 020 gives it `NOT NULL`
    /// and no `CHECK (head_sha <> '')`, so without this a checkpoint whose
    /// recorded HEAD is `""` or the word `none` would write and load clean.
    /// That direction is fail-safe — such a value can never equal live git
    /// HEAD, so the divergence gate blocks rather than passes — but the
    /// resulting gate failure describes a checkpoint nobody can explain, which
    /// is the unverified bookkeeping this module exists to end. Note the rule
    /// is *requiredness*, not shape: this type cannot tell a real SHA from any
    /// other word, and `gates_result` is free text after its prefix.
    ///
    /// The rules live here rather than being restated in `from_json`, which
    /// inherits them through its call below. `from_json`'s readers reject the
    /// same inputs earlier and with a field-specific parse message, because
    /// they are answering a different question (what did this JSON payload
    /// say?) and can distinguish an absent key from a wrong-typed one; this is
    /// the backstop that holds when nobody parsed anything.
    ///
    /// **The attestation correlation.** An
    /// `Operator` attestation is the escape hatch from the head-consistency
    /// gate, so a checkpoint claiming it while naming no range it vouches for
    /// is the one combination that must never reach the gate — and migration
    /// 020 deliberately will not catch it, its `CHECK` being one-directional
    /// by design (it forbids an implementer row carrying a range, and permits
    /// an operator row without one). The implementer direction is checked here
    /// too, redundantly with the schema, so the caller gets a validation
    /// message rather than a raw SQL error.
    ///
    /// "Names a range" means a *non-blank* one, held to the same [`is_blank`]
    /// rule as the required fields above. `Some("")` is not a weaker version
    /// of naming a range, it is the same claim as `None` dressed to pass:
    /// Tasks 7-10 read an operator attestation as a human having inspected a
    /// commit range and taken responsibility for it, so an empty one asserts
    /// that a human vouched for nothing. Unlike the other blank-value holes
    /// this type can have, that one is *not* fail-safe — a blank `head_sha`
    /// or `gates_sha` can never match live git HEAD and so blocks the gate,
    /// whereas this defeats it — which is why the presence check is a content
    /// check.
    ///
    /// Requiredness, not shape: as with `head_sha`, this type cannot tell a
    /// real `<from_sha>..<to_sha>` from any other non-blank string, and it has
    /// no repo to resolve one against. Checking that the range parses, that
    /// both endpoints exist, and that it actually spans the divergence is the
    /// `implementation_done` gate's job in Task 10, which has the repo —
    /// **that check is yours to add there, not here.**
    ///
    /// The `status`-shaped correlations — `batch_complete` should carry no
    /// `task_id`, `completed` should carry a `commit_sha` — are deliberately
    /// *not* here. They are conventions in migration 020's header rather than
    /// constraints, and the useful version of each ("every task is covered")
    /// needs the session's task count, which this type cannot see. They belong
    /// to the `implementation_done` gate, which has it.
    pub fn validate(&self) -> Result<(), CheckpointError> {
        for (field, value) in [
            ("session_id", self.session_id.as_str()),
            ("head_sha", self.head_sha.as_str()),
            ("gates_result", self.gates_result.as_str()),
        ] {
            if is_blank(value) {
                return Err(CheckpointError(format!(
                    "{field} must carry a real value: {value:?} is empty, whitespace-only, or \
                     the {ABSENT_SENTINEL:?} absent-sentinel"
                )));
            }
        }

        match (self.attested_by, self.acknowledged_divergence.as_deref()) {
            (AttestedBy::Implementer, Some(_)) => Err(CheckpointError(
                "acknowledged_divergence is only valid with attested_by=operator: an \
                 implementer cannot self-attest over commits the protocol never witnessed"
                    .to_string(),
            )),
            (AttestedBy::Operator, None) => Err(CheckpointError(
                "attested_by=operator requires acknowledged_divergence naming the range \
                 being vouched for, as <from_sha>..<to_sha>"
                    .to_string(),
            )),
            (AttestedBy::Operator, Some(range)) if is_blank(range) => {
                Err(CheckpointError(format!(
                    "attested_by=operator requires acknowledged_divergence naming the \
                     range being vouched for, as <from_sha>..<to_sha>: {range:?} names \
                     nothing, so the checkpoint claims a human vouched for no commits at all"
                )))
            }
            _ => Ok(()),
        }
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

/// The collab turn templates spell an absent value `none` (see the checkpoint
/// block in `docs/COLLAB.md`), so an agent transcribing one sends the literal
/// string rather than dropping the key.
const ABSENT_SENTINEL: &str = "none";

/// "The caller said nothing here", for a value that has already been extracted
/// from JSON — or was never JSON at all.
///
/// One statement of the rule [`optional_str`] applies at parse time, so
/// [`CollabCheckpoint::validate`] holds a struct built field-by-field to the
/// same standard a parsed one meets by construction. Deliberately shared
/// rather than restated: a `validate` that disagreed with the parser about
/// what counts as blank would be the drift the split entry points already
/// invite.
fn is_blank(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.is_empty() || trimmed == ABSENT_SENTINEL
}

/// Read a string field, normalizing "the caller has nothing to say here" —
/// absent, `null`, empty/whitespace, or the [`ABSENT_SENTINEL`] — to `None`,
/// and rejecting a present value of some other JSON type.
///
/// Every other string reader in this module is built on this one, so the type
/// strictness is uniform. That strictness is the point. `completed_task_ids`
/// is a comma-separated string on the wire and the obvious caller mistake is
/// sending `[1, 2, 3]`; `commit_sha` is the evidence of what a `completed`
/// task produced. Reading either wrong-typed value as "absent" would persist a
/// checkpoint that quietly contradicts what the caller believed it sent — the
/// unverified bookkeeping this module exists to end.
///
/// Values are trimmed, so a `" completed "` transcribed out of a template is
/// the same value as `"completed"` rather than a parse failure or a gate proof
/// that silently fails to match.
fn optional_str<'a>(value: &'a Value, field: &str) -> Result<Option<&'a str>, CheckpointError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(raw)) => {
            let trimmed = raw.trim();
            Ok((!trimmed.is_empty() && trimmed != ABSENT_SENTINEL).then_some(trimmed))
        }
        Some(_) => Err(CheckpointError(format!("{field} must be a string"))),
    }
}

/// Read a mandatory string field. Absent, whitespace-only, and the
/// [`ABSENT_SENTINEL`] are all rejected alike, so a checkpoint can never claim
/// a `head_sha` of `" "` or `"none"`.
fn require_str(value: &Value, field: &str) -> Result<String, CheckpointError> {
    optional_str(value, field)?
        .map(str::to_string)
        .ok_or_else(|| {
            CheckpointError(format!(
                "{field} is required and must be a non-empty string"
            ))
        })
}

/// Read an optional string field.
fn optional_string(value: &Value, field: &str) -> Result<Option<String>, CheckpointError> {
    Ok(optional_str(value, field)?.map(str::to_string))
}

/// Read a string field that falls back to `default` when the caller supplied
/// nothing — the reader for the two columns migration 020 gives a `NOT NULL
/// DEFAULT`, where "absent" means the default rather than `None`.
fn str_or_default<'a>(
    value: &'a Value,
    field: &str,
    default: &'a str,
) -> Result<&'a str, CheckpointError> {
    Ok(optional_str(value, field)?.unwrap_or(default))
}

/// Read an optional task id, accepting both a JSON number and the string form
/// an agent transcribing the `<N|none>` template will produce.
///
/// Zero is rejected: task ids are 1-based, which is a property of the id
/// itself and so checkable here, unlike "id exceeds the session's task count",
/// which needs a task list this type deliberately cannot see.
///
/// `pub(crate)` purely so a test can reach it, not because anything outside
/// this module should call it. `queue::checked_task_id_column` is an
/// independent statement of this function's refusal set on the *load* path,
/// and `queue::tests::task_id_column_loader_mirrors_the_parser` feeds the same
/// candidates through both to pin them together — the same lockstep idiom
/// `status_variants_match_migration_020` uses for the enum/SQL vocabulary.
/// Without it, relaxing either side silently stops the loader mirroring the
/// parser and reopens the gap Task 3 closed.
pub(crate) fn optional_task_id(value: &Value, field: &str) -> Result<Option<u32>, CheckpointError> {
    let invalid = || CheckpointError(format!("{field} must be a task id of 1 or greater"));
    let parsed = match value.get(field) {
        None | Some(Value::Null) => None,
        Some(Value::String(_)) => optional_str(value, field)?
            .map(|raw| raw.parse::<u32>().map_err(|_| invalid()))
            .transpose()?,
        Some(Value::Number(n)) => Some(
            n.as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(invalid)?,
        ),
        Some(_) => return Err(invalid()),
    };
    match parsed {
        Some(0) => Err(invalid()),
        other => Ok(other),
    }
}

/// Parse the cumulative `completed_task_ids` list from its wire form (a
/// comma-separated string) into a deduplicated, sorted vec.
///
/// The normalization is for storage and comparison, not for coverage:
/// [`CollabCheckpoint::covers_all_tasks`] builds its own set and is safe
/// either way. What it buys is that two checkpoints recording the same
/// progress round-trip to the same string, so an equality or diff over stored
/// checkpoints reflects real progress rather than the order the ids were
/// appended in.
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
        if id == 0 {
            return Err(CheckpointError(
                "completed_task_ids entries must be task ids of 1 or greater, got 0".to_string(),
            ));
        }
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

    /// The parsed *field* is normalized, not just the coverage arithmetic
    /// derived from it — the test above passes either way, because
    /// `covers_all_tasks` builds its own set. This is what pins the
    /// normalization the field doc promises, and with it the property that
    /// equal progress stores as an equal string.
    #[test]
    fn completed_task_ids_are_deduplicated_and_sorted_in_the_parsed_field() {
        let mut v = valid();
        v["completed_task_ids"] = json!("3,1,2,2");
        assert_eq!(
            CollabCheckpoint::from_json(&v).unwrap().completed_task_ids,
            vec![1, 2, 3]
        );
    }

    /// Task ids are 1-based, so a zero is a malformed id however it arrives.
    #[test]
    fn task_ids_of_zero_are_rejected() {
        let mut v = valid();
        v["task_id"] = json!(0);
        assert!(CollabCheckpoint::from_json(&v)
            .unwrap_err()
            .to_string()
            .contains("task_id"));

        let mut v = valid();
        v["completed_task_ids"] = json!("0,1");
        assert!(CollabCheckpoint::from_json(&v)
            .unwrap_err()
            .to_string()
            .contains("completed_task_ids"));
    }

    /// A batch with no tasks is a malformed task list, not a finished one, so
    /// `total = 0` must not be vacuously covered — an
    /// `implementation_done` gate that read a zero task count would otherwise
    /// wave the batch through on an empty checkpoint.
    #[test]
    fn covers_all_tasks_is_false_for_a_zero_total() {
        let mut v = valid();
        v["completed_task_ids"] = json!("");
        assert!(!CollabCheckpoint::from_json(&v).unwrap().covers_all_tasks(0));
        // Not merely because the list is empty:
        assert!(!CollabCheckpoint::from_json(&valid())
            .unwrap()
            .covers_all_tasks(0));
    }

    /// The anti-backdating property, and the most direct defense against the
    /// incident in issue #273: a caller cannot set its own `updated_at` and so
    /// cannot make a frozen checkpoint look freshly written. The server stamps
    /// it in `queue::upsert_checkpoint`.
    #[test]
    fn updated_at_is_never_read_from_the_payload() {
        let mut v = valid();
        v["updated_at"] = json!(99999);
        assert_eq!(CollabCheckpoint::from_json(&v).unwrap().updated_at, 0);
    }

    /// The turn templates spell an absent value `none`, so the sentinel means
    /// absent for every field that *may* be absent — and therefore means
    /// rejected for the three that may not. Both halves are behavior the
    /// helpers introduced: a length-only check would accept `"none"` as a
    /// literal `head_sha`. That direction is fail-safe (a `head_sha` of
    /// `"none"` can never equal live git HEAD, so the gate blocks rather than
    /// passes) but it would store a checkpoint whose recorded HEAD is a word.
    #[test]
    fn the_none_sentinel_is_rejected_for_the_three_mandatory_fields() {
        for field in ["session_id", "head_sha", "status"] {
            let mut v = valid();
            v[field] = json!("none");
            let err = CollabCheckpoint::from_json(&v).unwrap_err();
            assert!(err.to_string().contains(field), "got: {err}");
        }
    }

    /// `session_id` is the checkpoint's primary key and its FK to
    /// `collab_sessions`, so the database does backstop a missing one. This
    /// asserts the Rust half, whose whole justification is that the caller
    /// gets a validation message instead of a raw SQL constraint error.
    #[test]
    fn session_id_is_required_and_non_empty() {
        let mut v = valid();
        v["session_id"] = json!("");
        assert!(CollabCheckpoint::from_json(&v)
            .unwrap_err()
            .to_string()
            .contains("session_id"));
    }

    /// The other half of the sentinel rule: for every field that may be
    /// absent it means absent — including the two defaulted columns, where it
    /// means the column default rather than a parse failure.
    #[test]
    fn the_none_sentinel_means_absent_for_every_optional_field() {
        let mut v = valid();
        for field in [
            "task_id",
            "task_title",
            "commit_sha",
            "next_task_id",
            "gates_sha",
            "gates_commands",
            "summary",
        ] {
            v[field] = json!("none");
        }
        v["completed_task_ids"] = json!("none");
        v["gates_result"] = json!("none");
        v["attested_by"] = json!("none");

        let cp = CollabCheckpoint::from_json(&v).unwrap();
        assert_eq!(cp.task_id, None);
        assert_eq!(cp.task_title, None);
        assert_eq!(cp.commit_sha, None);
        assert_eq!(cp.next_task_id, None);
        assert_eq!(cp.gates_sha, None);
        assert_eq!(cp.gates_commands, None);
        assert_eq!(cp.summary, None);
        assert!(cp.completed_task_ids.is_empty());
        // The defaulted columns fall back to migration 020's defaults rather
        // than storing a value outside their documented vocabulary.
        assert_eq!(cp.gates_result, "not_run");
        assert_eq!(cp.attested_by, AttestedBy::Implementer);
    }

    /// Values arrive transcribed out of a template, so stray whitespace is
    /// routine. Trimming everywhere keeps `" passed "` a reusable gate proof
    /// instead of silently forcing a gate rerun.
    #[test]
    fn values_are_trimmed_before_they_are_interpreted() {
        let mut v = valid();
        v["status"] = json!(" completed ");
        v["attested_by"] = json!(" implementer ");
        v["gates_result"] = json!(" passed ");
        v["head_sha"] = json!(" abc123 ");

        let cp = CollabCheckpoint::from_json(&v).unwrap();
        assert_eq!(cp.status, CheckpointStatus::Completed);
        assert_eq!(cp.attested_by, AttestedBy::Implementer);
        assert_eq!(cp.head_sha, "abc123");
        assert!(cp.gates_are_green_at_head());
    }

    /// The same silent-drop failure `completed_task_ids` refuses, in the
    /// fields next door. A wrong-typed `commit_sha` read as `None` would
    /// record a `completed` task that produced no commit.
    #[test]
    fn optional_string_fields_reject_a_non_string_value() {
        for (field, wrong) in [
            ("commit_sha", json!(12345)),
            ("gates_sha", json!(["abc123"])),
            ("summary", json!(true)),
            ("task_title", json!({"text": "Add the gate"})),
        ] {
            let mut v = valid();
            v[field] = wrong;
            let err = CollabCheckpoint::from_json(&v).unwrap_err();
            assert!(err.to_string().contains(field), "got: {err}");
        }
    }

    /// `validate` exists because every field is `pub`: the `collab::queue`
    /// loader rebuilds a checkpoint from a row without going through
    /// `from_json`, and migration 020's one-directional CHECK deliberately
    /// permits the row below. Nothing else stands between a fabricated
    /// operator attestation and the head-consistency gate it exempts.
    ///
    /// Written as a full struct literal naming all fifteen fields rather than
    /// as a mutation of a parsed value, because the openness of the type is
    /// the thing under test — this is the construction Task 3's loader will
    /// perform. It therefore also stops compiling if a field is later made
    /// private or the struct gains `#[non_exhaustive]`, which is the change
    /// that would invalidate the reasoning above.
    #[test]
    fn validate_catches_an_operator_attestation_built_without_the_parser() {
        let mut smuggled = CollabCheckpoint {
            session_id: "s1".to_string(),
            task_id: Some(3),
            task_title: Some("Add the gate".to_string()),
            status: CheckpointStatus::BatchComplete,
            head_sha: "75a4ea3".to_string(),
            commit_sha: Some("75a4ea3".to_string()),
            completed_task_ids: vec![1, 2, 3],
            next_task_id: None,
            gates_result: "passed".to_string(),
            gates_sha: Some("75a4ea3".to_string()),
            gates_commands: Some("cargo test --workspace".to_string()),
            summary: Some("Batch complete".to_string()),
            attested_by: AttestedBy::Operator,
            acknowledged_divergence: None,
            updated_at: 1_760_000_000,
        };

        let err = smuggled.validate().unwrap_err();
        assert!(
            err.to_string().contains("acknowledged_divergence"),
            "got: {err}"
        );

        smuggled.acknowledged_divergence = Some("b9c2ce0..75a4ea3".to_string());
        assert!(smuggled.validate().is_ok());
    }

    /// The required-field half of `validate`, and the reason it is there: the
    /// three non-`Option` `String` fields are exactly the ones a
    /// field-by-field builder can leave blank, and migration 020 gives them
    /// `NOT NULL` without any `CHECK (<col> <> '')`. `head_sha` is the field
    /// the whole issue turns on — a checkpoint recording a HEAD of `""` or the
    /// word `none` fails the divergence gate in a way nobody can diagnose.
    ///
    /// Every field/value pair is checked on its own: a single struct mutated
    /// in three places at once would let one rule mask the other two.
    #[test]
    fn validate_rejects_a_blank_required_field() {
        for blank in ["", "   ", "none", "  none  "] {
            for field in ["session_id", "head_sha", "gates_result"] {
                let mut cp = CollabCheckpoint::from_json(&valid()).unwrap();
                match field {
                    "session_id" => cp.session_id = blank.to_string(),
                    "head_sha" => cp.head_sha = blank.to_string(),
                    _ => cp.gates_result = blank.to_string(),
                }
                let err = match cp.validate() {
                    Ok(()) => panic!("{field} = {blank:?} must be rejected"),
                    Err(err) => err.to_string(),
                };
                assert!(
                    err.contains(field),
                    "{field} = {blank:?} was rejected without naming the field: {err}"
                );
            }
        }
    }

    /// The blank-value hole in its one *non*-fail-safe position. Everywhere
    /// else a blank value makes the gate block — a `head_sha` of `""` can
    /// never equal live git HEAD — but `acknowledged_divergence` is the
    /// escape hatch *from* that gate, so a blank one makes the gate pass while
    /// vouching for nothing. `Some("")` is not a weaker claim than `None`, it
    /// is the same claim shaped to survive a presence check, and neither
    /// migration 020's one-directional CHECK nor `from_json` (which never
    /// produces it, `optional_string` normalizing blanks to `None`) stands in
    /// its way. Only a struct built field-by-field can reach this state, which
    /// is exactly what the loader does.
    ///
    /// One value per constructed struct, so no case is carried by another.
    #[test]
    fn validate_rejects_a_blank_operator_range() {
        for blank in ["", "   ", "none", "  none  "] {
            let mut cp = CollabCheckpoint::from_json(&valid()).unwrap();
            cp.attested_by = AttestedBy::Operator;
            cp.acknowledged_divergence = Some(blank.to_string());

            let err = match cp.validate() {
                Ok(()) => panic!("an operator range of {blank:?} must be rejected"),
                Err(err) => err.to_string(),
            };
            assert!(
                err.contains("acknowledged_divergence"),
                "operator range {blank:?} was rejected without naming the field: {err}"
            );
        }

        // And the rule is requiredness, not shape: a non-blank range passes
        // here even though this type cannot tell a real SHA range from any
        // other string. Resolving it against the repo is Task 10's job.
        let mut ok = CollabCheckpoint::from_json(&valid()).unwrap();
        ok.attested_by = AttestedBy::Operator;
        ok.acknowledged_divergence = Some("b9c2ce0..75a4ea3".to_string());
        assert!(ok.validate().is_ok());
    }

    /// The other direction, so the rule above cannot be satisfied by a
    /// `validate` that rejects everything: a legitimately-defaulted
    /// `gates_result` is *not* blank. `from_json` substitutes migration 020's
    /// `not_run` default when the caller says nothing, so "absent" and
    /// "empty" are different states for this column and only the second is an
    /// error.
    #[test]
    fn validate_accepts_the_defaulted_gates_result() {
        let mut v = valid();
        v.as_object_mut().unwrap().remove("gates_result");
        let cp = CollabCheckpoint::from_json(&v).unwrap();
        assert_eq!(cp.gates_result, "not_run");
        assert!(cp.validate().is_ok());
    }

    /// `validate`'s required-field rules are a backstop for struct builders,
    /// not a replacement for the parser's own errors: a bad JSON payload must
    /// still fail in `require_str`, with its field-specific "is required"
    /// message, rather than falling through to `validate`'s. Pinned because
    /// the two now overlap, and a refactor that deleted the parser's check
    /// would still leave every `contains(field)` assertion in this module
    /// green while silently changing what a tool caller is told.
    #[test]
    fn from_json_reports_the_parser_error_for_a_blank_mandatory_field() {
        for field in ["session_id", "head_sha"] {
            let mut v = valid();
            v[field] = json!("");
            let err = CollabCheckpoint::from_json(&v).unwrap_err().to_string();
            assert_eq!(
                err,
                format!("{field} is required and must be a non-empty string"),
                "from_json must report its own parse error, not validate's"
            );
        }
    }

    /// The mirror image: an implementer row carrying a range. The schema does
    /// catch this one, but a direct Rust caller never reaches the schema.
    #[test]
    fn validate_catches_an_implementer_attestation_carrying_a_range() {
        let mut smuggled = CollabCheckpoint::from_json(&valid()).unwrap();
        smuggled.acknowledged_divergence = Some("aaa..bbb".to_string());

        let err = smuggled.validate().unwrap_err();
        assert!(
            err.to_string().contains("acknowledged_divergence"),
            "got: {err}"
        );
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

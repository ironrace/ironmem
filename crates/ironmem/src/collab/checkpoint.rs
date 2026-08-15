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

use super::MAX_TASKS_PER_COLLAB_ISSUE;

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

/// What the server actually established about an operator attestation's
/// `acknowledged_divergence`, resolved against the repository at write time.
///
/// **Three states, deliberately not two**, for the same reason
/// `mcp::tools::collab_session::HeadCheck` has three: "the server checked this
/// range and it holds" and "the server could not check" must never render as
/// the same word. An attestation is a claim that a human inspected specific
/// commits; a label that conflated a resolved range with an unresolved one
/// would let the unresolved case inherit the resolved one's credibility.
///
/// There is no `NotApplicable` variant, and its absence is the point. "No
/// verdict" is `Option::None` — an implementer row (no range to resolve) and a
/// row written before migration 021 are both genuinely verdict-less, and a
/// literal spelling would make them indistinguishable from a positive finding.
/// [`CollabCheckpoint::attestation_verdict`] is the single place the fail-safe
/// default for `None` on an *operator* row is stated.
///
/// The spellings are pinned to migration 021's `CHECK` list by
/// `attestation_check_variants_match_migration_021`, exactly as
/// [`CheckpointStatus`] and [`AttestedBy`] are pinned to 020's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttestationCheck {
    /// Every rule ran and held: both endpoints resolve, the range ends at the
    /// checkpoint's own `head_sha`, it covers at least one commit, `from` is an
    /// ancestor of `to`, and it spans the gap left by the checkpoint it
    /// replaced.
    Verified,
    /// The endpoint rules held, but **coverage of the gap was not
    /// established** — the checkpoint being replaced is not behind this one in
    /// a way that defines a gap to cover (its head no longer resolves, or it is
    /// not an ancestor of this checkpoint's head). Those two are branch-drift
    /// shapes; the attestation is well-formed but nobody has checked that it
    /// leaves no commits unaccounted for.
    ///
    /// A third producer is not a repository shape but a race: the span is
    /// judged against the checkpoint read before the write transaction opens,
    /// and if the row the write actually replaces is not that one, the verdict
    /// is re-qualified down to this label inside the transaction rather than
    /// reporting a `Verified` that describes a checkpoint no longer there.
    VerifiedWithoutSpan,
    /// Live HEAD could not be read at all, so **only the range's syntax was
    /// checked**. The write is accepted — a transient filesystem problem must
    /// not make a legitimate attestation unwritable — and labelled as what it
    /// is.
    UnverifiedRepoUnreadable,
}

impl AttestationCheck {
    /// The wire and storage spelling. Must appear verbatim in migration 021's
    /// `attestation_check` CHECK list.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::VerifiedWithoutSpan => "verified_without_span",
            Self::UnverifiedRepoUnreadable => "unverified_repo_unreadable",
        }
    }
}

impl fmt::Display for AttestationCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AttestationCheck {
    type Err = CheckpointError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "verified" => Ok(Self::Verified),
            "verified_without_span" => Ok(Self::VerifiedWithoutSpan),
            "unverified_repo_unreadable" => Ok(Self::UnverifiedRepoUnreadable),
            other => Err(CheckpointError(format!(
                "attestation_check must be one of verified|verified_without_span|\
                 unverified_repo_unreadable, got {other:?}"
            ))),
        }
    }
}

/// What a reader is told about an operator attestation whose verdict was never
/// recorded — a row from before migration 021, or one a future write path
/// forgot to stamp.
///
/// It reads as *unchecked*, never as absent, and that direction is the whole
/// safety property: an unstamped operator attestation must not be mistaken for
/// a verified one. See [`CollabCheckpoint::attestation_verdict`].
pub const ATTESTATION_UNRECORDED: &str = "unrecorded";

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
    /// What the server established about `acknowledged_divergence` by resolving
    /// it against the repository — see [`AttestationCheck`].
    ///
    /// Server-derived, never parsed from the payload, exactly like
    /// `updated_at`: [`CollabCheckpoint::from_json`] leaves this `None` and the
    /// MCP handler stamps it from its own git reads, so a caller cannot label
    /// its own attestation `verified`.
    ///
    /// `None` means no verdict was recorded. Read it through
    /// [`CollabCheckpoint::attestation_verdict`] rather than directly, so the
    /// fail-safe default for an unstamped *operator* row is applied once rather
    /// than at each of the three reader surfaces.
    pub attestation_check: Option<AttestationCheck>,
    /// Unix seconds, server-stamped at write time rather than parsed from the
    /// payload. `0` means "not yet stamped": that is what
    /// [`CollabCheckpoint::from_json`] leaves here, and
    /// `queue::upsert_checkpoint` overwrites it unconditionally, so a `0` in
    /// hand means the checkpoint has not been through a write.
    pub updated_at: i64,
}

/// Maximum length (chars) for a checkpoint's free-text fields, the same 2048 as
/// `mcp::tools::collab_events::MAX_CODING_FAILURE_CHARS` — the sibling cap on
/// the other unbounded string an agent files about its own run.
///
/// The cost of an uncapped field here is not the one write. The row is re-read
/// and re-rendered by `collab_status`, `collab_resume` and the
/// `session_handoff` block on every poll for the rest of the session, so a
/// pasted-in gate log is paid for again at each of them. `gates_result` is the
/// field this is really about: `failed: <reason>` invites a paste of the whole
/// test output, which is exactly the shape `MAX_CODING_FAILURE_CHARS` exists to
/// stop next door.
const MAX_CHECKPOINT_TEXT_CHARS: usize = 2048;

/// Maximum length (chars) for the SHA-shaped fields, and for the
/// `<from_sha>..<to_sha>` range built out of two of them.
///
/// Generous next to the 130 chars two full object ids and a `..` take, because
/// the rule this type enforces on these fields is requiredness rather than
/// shape (see [`CollabCheckpoint::validate`]) and git accepts revision
/// expressions as well as object ids — but far below the size at which a "SHA"
/// is really a payload.
///
/// The 130 is sha256 arithmetic (64 + 2 + 64), not sha1's 82. A cap derived
/// from sha1 would sit *below* a legitimate `acknowledged_divergence` range in
/// a sha256 repository, so the tool's own paste-ready attestation template
/// would be refused by this very constant — a cap must not be narrower than
/// the widest value the code that reads it emits.
const MAX_CHECKPOINT_SHA_CHARS: usize = 160;

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
    ///
    /// This is also the one place the per-field size caps are applied — see
    /// [`CollabCheckpoint::check_payload_caps`] for why here and nowhere else.
    pub fn from_json(value: &Value) -> Result<Self, CheckpointError> {
        let checkpoint = Self {
            session_id: parse_session_id(value)?,
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
            // Neither of the two below is read from the payload; see this
            // function's doc comment. A caller that could stamp its own
            // `attestation_check` could label a fabricated range `verified`.
            attestation_check: None,
            updated_at: 0,
        };
        checkpoint.check_payload_caps()?;
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    /// Refuse a payload field that is longer than its cap.
    ///
    /// Called from [`CollabCheckpoint::from_json`] and deliberately **not**
    /// from [`CollabCheckpoint::validate`], which is the rule that matters
    /// here. `validate` also runs on the *load* path
    /// (`queue::load_current_checkpoint`), where a refusal is fatal to every
    /// reader at once: a cap there would make an over-long row that is already
    /// in the table permanently unloadable, so `collab_status`,
    /// `collab_resume` and `session_handoff` would each refuse the session and
    /// leave no surface from which to see the row, let alone replace it. A cap
    /// belongs where the value can still be refused — at the parse of the
    /// payload that would create it.
    ///
    /// Refusing rather than truncating, for the same reason the sibling
    /// `coding_failure` cap refuses: a truncated field is a stored row
    /// misstating what the caller sent, and every later reader renders it as
    /// though the caller had sent that. Note that the MCP layer *may* compact
    /// a long failure log before it reaches its own cap
    /// (`mcp::compact::compact_failure_log`); no equivalent runs here, because
    /// this module is a pure parse/validate unit and reaching up into the MCP
    /// layer for it would invert the dependency the module doc turns on.
    ///
    /// Two payload fields are absent from the list below, both deliberately.
    /// `completed_task_ids` is bounded in [`parse_completed_task_ids`], which
    /// is where its raw wire form still exists and where its *domain* bound
    /// ([`MAX_TASKS_PER_COLLAB_ISSUE`]) can be stated. `session_id` is a
    /// lookup key rather than content: `handle_collab_checkpoint` resolves it
    /// through `queue::ensure_active` before anything is written, so an
    /// oversized one names no session and is refused there.
    fn check_payload_caps(&self) -> Result<(), CheckpointError> {
        for (field, value, cap) in [
            (
                "task_title",
                self.task_title.as_deref(),
                MAX_CHECKPOINT_TEXT_CHARS,
            ),
            (
                "gates_result",
                Some(self.gates_result.as_str()),
                MAX_CHECKPOINT_TEXT_CHARS,
            ),
            (
                "gates_commands",
                self.gates_commands.as_deref(),
                MAX_CHECKPOINT_TEXT_CHARS,
            ),
            (
                "summary",
                self.summary.as_deref(),
                MAX_CHECKPOINT_TEXT_CHARS,
            ),
            (
                "head_sha",
                Some(self.head_sha.as_str()),
                MAX_CHECKPOINT_SHA_CHARS,
            ),
            (
                "commit_sha",
                self.commit_sha.as_deref(),
                MAX_CHECKPOINT_SHA_CHARS,
            ),
            (
                "gates_sha",
                self.gates_sha.as_deref(),
                MAX_CHECKPOINT_SHA_CHARS,
            ),
            (
                "acknowledged_divergence",
                self.acknowledged_divergence.as_deref(),
                MAX_CHECKPOINT_SHA_CHARS,
            ),
        ] {
            let Some(value) = value else { continue };
            let length = value.chars().count();
            if length > cap {
                return Err(CheckpointError(format!(
                    "{field} is {length} chars, over the {cap}-char limit: the checkpoint is \
                     refused rather than truncated, so the stored row cannot misstate what you \
                     sent"
                )));
            }
        }
        Ok(())
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
    /// both endpoints exist, and that it actually spans the divergence needs
    /// the repo, and lives in
    /// `mcp::tools::collab_checkpoint::verify_acknowledged_range` — at the
    /// **write**, not at the `implementation_done` gate this comment used to
    /// point at. See that function for why: verifying at the gate would leave a
    /// fabricated range sitting in `collab_checkpoints` in the meantime, where
    /// `session_handoff`, `collab_status` and `collab_resume` all render it to
    /// a human as `attested_by: operator`; and `require_checkpoint_proof` is
    /// deliberately pure with respect to the filesystem, so it has no repo
    /// either.
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
            (AttestedBy::Implementer, Some(_)) => {
                return Err(CheckpointError(
                    "acknowledged_divergence is only valid with attested_by=operator: an \
                     implementer cannot self-attest over commits the protocol never witnessed"
                        .to_string(),
                ))
            }
            (AttestedBy::Operator, None) => {
                return Err(CheckpointError(
                    "attested_by=operator requires acknowledged_divergence naming the range \
                     being vouched for, as <from_sha>..<to_sha>"
                        .to_string(),
                ))
            }
            (AttestedBy::Operator, Some(range)) if is_blank(range) => {
                return Err(CheckpointError(format!(
                    "attested_by=operator requires acknowledged_divergence naming the \
                     range being vouched for, as <from_sha>..<to_sha>: {range:?} names \
                     nothing, so the checkpoint claims a human vouched for no commits at all"
                )))
            }
            _ => {}
        }

        // The same one-directional rule, for the server's own verdict.
        // Migration 021 cannot express it — SQLite's ALTER TABLE ADD COLUMN
        // takes no table-level CHECK spanning two columns — so this is the only
        // thing standing between a row and a claim it cannot have earned. An
        // implementer checkpoint has no range to resolve, so a verdict on one
        // describes a check that never happened, and every reader surface would
        // render it beside `attested_by: implementer` as though the server had
        // vouched for something.
        if self.attested_by == AttestedBy::Implementer && self.attestation_check.is_some() {
            return Err(CheckpointError(format!(
                "attestation_check is only valid with attested_by=operator: an implementer \
                 checkpoint names no range for the server to resolve, so {:?} describes a \
                 check that never ran",
                self.attestation_check.map(AttestationCheck::as_str)
            )));
        }
        Ok(())
    }

    /// What a **reader** should be told about this row's attestation, or `None`
    /// when the row makes no attestation claim at all.
    ///
    /// The one statement of the fail-safe default, so the three reader surfaces
    /// (`collab_status`, `collab_resume`, the `session_handoff` block) cannot
    /// drift apart on it: an operator attestation whose verdict was never
    /// recorded reads as [`ATTESTATION_UNRECORDED`] — *unchecked* — never as
    /// absent and never as verified.
    ///
    /// This exists because storing the verdict is only half the fix. The
    /// argument for resolving the range at the write rather than at the
    /// `implementation_done` gate is that otherwise a fabricated range sits in
    /// the table while `session_handoff`, `collab_status` and `collab_resume`
    /// render `attested_by: operator` to a human as though it meant something.
    /// A verdict that reached the `wal_log` and nothing else would leave that
    /// argument one step short of the readers it is written about.
    pub fn attestation_verdict(&self) -> Option<&'static str> {
        match self.attested_by {
            AttestedBy::Implementer => None,
            AttestedBy::Operator => Some(
                self.attestation_check
                    .map_or(ATTESTATION_UNRECORDED, AttestationCheck::as_str),
            ),
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
    ///
    /// Treating a *count* as the id range `1..=total` is only sound because
    /// `collab::task_list::validate_task_list_body` requires task ids to be
    /// exactly `1..=N` in order, so the count and the id set are the same
    /// fact. That is an invariant this type cannot check and does not own: if
    /// task lists are ever allowed sparse or non-1-based ids again, this
    /// method must take the id set instead, or a batch numbered `4,5,6` can
    /// never satisfy it while one numbered `1,5,9` is satisfied by a ledger
    /// that skipped two tasks.
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

/// Read the `session_id` a checkpoint tool call is about.
///
/// Exists so `collab_checkpoint`'s read-only `inspect_divergence` mode — which
/// parses no checkpoint and so never reaches [`CollabCheckpoint::from_json`] —
/// reads the key through the *same* rule the write path does, rather than
/// through `mcp::tools::shared::require_str`, which neither trims nor rejects
/// the [`ABSENT_SENTINEL`]. Two readings of one key in one tool is exactly the
/// asymmetry `handle_collab_checkpoint`'s own comment argues against: a
/// `" <id> "` transcribed out of a turn template would inspect one session and
/// be looked up under another.
pub(crate) fn parse_session_id(value: &Value) -> Result<String, CheckpointError> {
    require_str(value, "session_id")
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
///
/// Bounded twice, because the two bounds answer different questions. The
/// [`MAX_CHECKPOINT_TEXT_CHARS`] cap on the raw string bounds the *work*: a
/// list of one id repeated a million times normalizes to `"1"`, so a bound on
/// the parsed set alone would leave the scan of the payload unbounded. The
/// [`MAX_TASKS_PER_COLLAB_ISSUE`] cap on the distinct ids is the *domain*
/// bound — a checkpoint claiming more finished tasks than a collab session may
/// contain is describing a session that cannot exist — and is checked as the
/// set grows so an id-per-task flood stops at the sixteenth entry rather than
/// at the end of the string. Both refuse rather than truncate, for the reason
/// [`CollabCheckpoint::check_payload_caps`] gives.
fn parse_completed_task_ids(raw: &str) -> Result<Vec<u32>, CheckpointError> {
    let length = raw.chars().count();
    if length > MAX_CHECKPOINT_TEXT_CHARS {
        return Err(CheckpointError(format!(
            "completed_task_ids is {length} chars, over the {MAX_CHECKPOINT_TEXT_CHARS}-char \
             limit: at most {MAX_TASKS_PER_COLLAB_ISSUE} task ids can legitimately appear"
        )));
    }
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
        if ids.len() > MAX_TASKS_PER_COLLAB_ISSUE as usize {
            return Err(CheckpointError(format!(
                "completed_task_ids names more than {MAX_TASKS_PER_COLLAB_ISSUE} distinct tasks, \
                 which is the most one collab session may contain"
            )));
        }
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

    /// Migration 021 owns the `attestation_check` vocabulary, on the same
    /// terms: read at compile time so the lockstep test below cannot drift from
    /// the migration that actually ships.
    const MIGRATION_021: &str =
        include_str!("../../migrations/021_checkpoint_attestation_check.sql");

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

    /// Every [`AttestationCheck`] variant. See [`ALL_STATUSES`] for why the
    /// list is spelled out rather than derived.
    const ALL_ATTESTATION_CHECKS: &[AttestationCheck] = &[
        AttestationCheck::Verified,
        AttestationCheck::VerifiedWithoutSpan,
        AttestationCheck::UnverifiedRepoUnreadable,
    ];

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
    /// Written as a full struct literal naming all sixteen fields rather than
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
            attestation_check: None,
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

    /// Every free-text field a caller fills is capped, at the same 2048 chars
    /// as the sibling `coding_failure` field. `gates_result` is the one that
    /// invites the abuse — `failed: <paste of the whole test output>` — and
    /// the cost is not the one write: the row is re-read and re-rendered by
    /// `collab_status`, `collab_resume` and the `session_handoff` block on
    /// every poll for the rest of the session.
    #[test]
    fn free_text_fields_are_capped() {
        for field in ["task_title", "gates_result", "gates_commands", "summary"] {
            let mut v = valid();
            v[field] = json!("x".repeat(MAX_CHECKPOINT_TEXT_CHARS + 1));
            let err = CollabCheckpoint::from_json(&v).unwrap_err();
            assert!(err.to_string().contains(field), "got: {err}");

            // And the boundary is a limit rather than an off-by-one: a field
            // of exactly the cap is a legitimate value, not a refusal.
            v[field] = json!("x".repeat(MAX_CHECKPOINT_TEXT_CHARS));
            assert!(
                CollabCheckpoint::from_json(&v).is_ok(),
                "{field} at exactly {MAX_CHECKPOINT_TEXT_CHARS} chars must parse"
            );
        }
    }

    /// The SHA-shaped fields get the tighter cap, including the operator
    /// range built out of two of them. This type still cannot tell a real SHA
    /// from any other word — that is `verify_acknowledged_range`'s job — but
    /// 128 chars is far more than any object id or revision expression needs
    /// and far less than a payload.
    #[test]
    fn sha_shaped_fields_are_capped() {
        for field in ["head_sha", "commit_sha", "gates_sha"] {
            let mut v = valid();
            v[field] = json!("a".repeat(MAX_CHECKPOINT_SHA_CHARS + 1));
            let err = CollabCheckpoint::from_json(&v).unwrap_err();
            assert!(err.to_string().contains(field), "got: {err}");

            v[field] = json!("a".repeat(MAX_CHECKPOINT_SHA_CHARS));
            assert!(
                CollabCheckpoint::from_json(&v).is_ok(),
                "{field} at exactly {MAX_CHECKPOINT_SHA_CHARS} chars must parse"
            );
        }

        let mut v = valid();
        v["attested_by"] = json!("operator");
        v["acknowledged_divergence"] = json!(format!(
            "{}..{}",
            "b".repeat(MAX_CHECKPOINT_SHA_CHARS),
            "c".repeat(MAX_CHECKPOINT_SHA_CHARS)
        ));
        let err = CollabCheckpoint::from_json(&v).unwrap_err();
        assert!(
            err.to_string().contains("acknowledged_divergence"),
            "got: {err}"
        );
    }

    /// The id list is bounded by the session's own task limit, and separately
    /// by the raw string's length — a list of one id repeated normalizes to a
    /// single entry, so the domain bound alone would leave the scan of a
    /// multi-megabyte payload unbounded.
    #[test]
    fn completed_task_ids_is_bounded_by_the_sessions_task_limit() {
        let ids = |through: u32| {
            (1..=through)
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };

        let mut v = valid();
        v["completed_task_ids"] = json!(ids(MAX_TASKS_PER_COLLAB_ISSUE + 1));
        let err = CollabCheckpoint::from_json(&v).unwrap_err();
        assert!(err.to_string().contains("completed_task_ids"), "got: {err}");

        // A batch that legitimately finished every task the limit allows must
        // still be able to record it.
        v["completed_task_ids"] = json!(ids(MAX_TASKS_PER_COLLAB_ISSUE));
        assert_eq!(
            CollabCheckpoint::from_json(&v)
                .unwrap()
                .completed_task_ids
                .len(),
            MAX_TASKS_PER_COLLAB_ISSUE as usize
        );

        // The duplicate flood: one distinct id, so the domain bound never
        // fires and only the length cap stands between this and a full scan.
        v["completed_task_ids"] = json!("1,".repeat(MAX_CHECKPOINT_TEXT_CHARS));
        let err = CollabCheckpoint::from_json(&v).unwrap_err();
        assert!(err.to_string().contains("completed_task_ids"), "got: {err}");
    }

    /// The caps are enforced in `from_json` and *nowhere else*, which is the
    /// load-bearing half of the fix. `validate` also runs on the load path in
    /// `queue::load_current_checkpoint`, so a cap there would make an
    /// over-long row that is already stored permanently unloadable —
    /// `collab_status`, `collab_resume` and `session_handoff` would each
    /// refuse the session, leaving no surface from which to see the row, let
    /// alone replace it. A row this parser would refuse today must still load.
    #[test]
    fn the_caps_do_not_reach_the_load_path() {
        let mut stored = CollabCheckpoint::from_json(&valid()).unwrap();
        stored.gates_result = "x".repeat(MAX_CHECKPOINT_TEXT_CHARS * 4);
        stored.summary = Some("y".repeat(MAX_CHECKPOINT_TEXT_CHARS * 4));
        stored.head_sha = "z".repeat(MAX_CHECKPOINT_SHA_CHARS * 4);
        stored.completed_task_ids = (1..=MAX_TASKS_PER_COLLAB_ISSUE * 4).collect();
        assert!(
            stored.validate().is_ok(),
            "an over-long row already in the table must stay loadable"
        );
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

    /// The `attestation_check` half, against migration 021. Same reasoning as
    /// [`status_variants_match_migration_020`]: the enum and the SQL CHECK are
    /// two independent statements of one vocabulary, and a variant added to
    /// only one of them fails as a constraint violation on a live session at
    /// the moment an operator files an attestation.
    #[test]
    fn attestation_check_variants_match_migration_021() {
        let sql_values: Vec<String> = {
            let needle = "attestation_check IN (";
            let start = MIGRATION_021
                .rfind(needle)
                .expect("migration 021 must CHECK the attestation_check vocabulary")
                + needle.len();
            let end = start
                + MIGRATION_021[start..]
                    .find(')')
                    .expect("unterminated attestation_check IN ( in migration 021");
            MIGRATION_021[start..end]
                .split(',')
                .map(|piece| piece.trim().trim_matches('\'').to_string())
                .collect()
        };

        for check in ALL_ATTESTATION_CHECKS {
            // Exhaustiveness guard: a new variant stops this match compiling.
            match check {
                AttestationCheck::Verified
                | AttestationCheck::VerifiedWithoutSpan
                | AttestationCheck::UnverifiedRepoUnreadable => {}
            }
            assert!(
                sql_values.iter().any(|v| v == check.as_str()),
                "AttestationCheck::{check:?} serializes to {:?}, which migration 021's \
                 CHECK list {sql_values:?} would reject",
                check.as_str()
            );
        }

        for value in &sql_values {
            assert!(
                value.parse::<AttestationCheck>().is_ok(),
                "migration 021 permits attestation_check {value:?}, which AttestationCheck \
                 cannot parse"
            );
        }
    }

    /// The verdict is server-derived, exactly like `updated_at`. A caller that
    /// could stamp its own would label a fabricated range `verified` on all
    /// three reader surfaces at once — the single most valuable field to forge
    /// in this whole type.
    #[test]
    fn attestation_check_is_never_read_from_the_payload() {
        let mut v = valid();
        v["attested_by"] = json!("operator");
        v["acknowledged_divergence"] = json!("b9c2ce0..75a4ea3");
        v["attestation_check"] = json!("verified");
        assert_eq!(
            CollabCheckpoint::from_json(&v).unwrap().attestation_check,
            None
        );
    }

    /// The mirror of the `acknowledged_divergence` rule, for the server's own
    /// finding: an implementer checkpoint names no range to resolve, so a
    /// verdict on one describes a check that never ran. Migration 021 cannot
    /// express this (ALTER TABLE ADD COLUMN takes no two-column CHECK), so
    /// `validate` is the only thing enforcing it.
    #[test]
    fn validate_catches_an_attestation_check_on_an_implementer_row() {
        let mut cp = CollabCheckpoint::from_json(&valid()).unwrap();
        cp.attestation_check = Some(AttestationCheck::Verified);
        let err = cp.validate().unwrap_err();
        assert!(err.to_string().contains("attestation_check"), "got: {err}");
    }

    /// What every reader surface is told, and the fail-safe that matters: an
    /// operator attestation carrying no stored verdict reads as *unchecked*,
    /// never as absent and never as verified. Pre-021 rows and any future write
    /// path that forgets to stamp both land here.
    #[test]
    fn an_unstamped_operator_attestation_reads_as_unchecked() {
        let mut cp = CollabCheckpoint::from_json(&valid()).unwrap();
        assert_eq!(
            cp.attestation_verdict(),
            None,
            "an implementer row makes no attestation claim at all"
        );

        cp.attested_by = AttestedBy::Operator;
        cp.acknowledged_divergence = Some("b9c2ce0..75a4ea3".to_string());
        cp.attestation_check = None;
        assert_eq!(cp.attestation_verdict(), Some(ATTESTATION_UNRECORDED));
        assert_ne!(cp.attestation_verdict(), Some("verified"));

        cp.attestation_check = Some(AttestationCheck::UnverifiedRepoUnreadable);
        assert_eq!(cp.attestation_verdict(), Some("unverified_repo_unreadable"));
        cp.attestation_check = Some(AttestationCheck::Verified);
        assert_eq!(cp.attestation_verdict(), Some("verified"));
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

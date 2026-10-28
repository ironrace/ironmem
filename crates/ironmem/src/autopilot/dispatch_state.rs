//! Dispatch-state drawer — the Lead's crash-safe memory (spec's *Lead
//! crash-safe state* section).
//!
//! `logical_key` per in-flight issue, so a restarted Lead can reconcile this
//! set against `ListAgents`/`claude agents --json` (present → adopt, present
//! but dead → restart from checkpoint, absent but alive → flag as an
//! orphan). Rung 1 only stores and retrieves this record; the reconciliation
//! logic itself is rung 7's (*supervision + crash safety*).

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::schema::Database;
use crate::error::MemoryError;

use super::implementer::Implementer;
use super::invocation::InvocationArtifacts;
use super::{read_current, validate_repo, write_current, IssueRef};

/// One in-flight IC dispatch. Field names and the literal-shape fields
/// (`issue`, `repo`, `worktree_path`, `ic_session_name`, `dispatch_class`,
/// `attempt_n`, `state`, `started_at`) mirror the spec's storage table
/// exactly; `session_uuid` and `turn_n` are the two additions the spec calls
/// out by name (line 432) on top of that base shape, and `session_claimed`
/// is a third the resume path needs — see its own doc.
///
/// Unlike [`super::lineage::AttemptRecord`], the spec's shape here lists
/// `issue` and `repo` as *separate* sibling fields — so `issue` in the
/// persisted body is the bare issue number, not the `repo#number` canonical
/// string `AttemptRecord` uses. [`IssueRef`] still carries both; only the
/// wire shape differs, to track each kind's literal spec shape faithfully.
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchState {
    pub implementer: Implementer,
    pub native_session_id: Option<String>,
    pub invocation_artifacts: Option<InvocationArtifacts>,
    pub issue: IssueRef,
    pub worktree_path: String,
    pub ic_session_name: String,
    pub dispatch_class: String,
    pub attempt_n: u32,
    pub state: String,
    pub started_at: String,
    pub session_uuid: String,
    pub turn_n: u32,
    /// Whether `session_uuid` has actually been handed to a `claude`
    /// process yet. `--session-id` may only be used once and `--resume`
    /// only works for a session that exists, so "a drawer exists" is *not*
    /// the same question as "the session exists": a run can persist this
    /// record (crash safety demands it be written before the first launch)
    /// and then stop — on the daily budget, or on repeated launch
    /// failures — without ever starting a process. Resuming such a uuid
    /// would fail every time, forever.
    ///
    /// Defaulted on read so records written before this field existed
    /// deserialize as "not claimed", which is the conservative answer: a
    /// spurious `--session-id` on an existing session fails loudly on the
    /// next dispatch, whereas a spurious `--resume` on a session that was
    /// never opened wedges the issue.
    pub session_claimed: bool,
    /// The `--model` that `session_uuid` was last dispatched with. A harness
    /// that [rotates a refused session](Implementer::rotates_refused_session)
    /// opens a fresh one when the run's model differs: Muse refuses to
    /// resume a session across providers. `None` for records written before
    /// this field existed, which never rotate on it.
    pub session_model: Option<String>,
    /// Consecutive finished invocations the harness refused to resume
    /// `session_uuid` for ([`SessionRefusal::Unresumable`]). Kept across runs
    /// so a run that stops after one refusal still counts it.
    ///
    /// [`SessionRefusal::Unresumable`]: super::dispatch::SessionRefusal::Unresumable
    pub resume_refusals: u32,
    /// Whether the run that wrote this record saw the commit-signing policy
    /// on. Written before every launch, so a replacement controller that
    /// recovers the invocation still holds its `met` to the policy when the
    /// branch and repository flags could not be written (another agent held
    /// `.git/config`'s lock) and the policy has since been turned off.
    /// Defaulted on read: older records hold nothing beyond those flags.
    pub signing_held: bool,
}

impl DispatchState {
    /// The phase [`Self::state`] names, or `None` for a label no run of this
    /// binary writes.
    pub fn phase(&self) -> Option<DispatchPhase> {
        DispatchPhase::parse(&self.state)
    }

    /// Whether the run that wrote this state has certainly ended: it is
    /// parked. Any other label, unknown ones included, may be a run still
    /// going.
    pub fn is_parked(&self) -> bool {
        self.phase().is_some_and(DispatchPhase::is_parked)
    }

    /// Refuse a combination no harness writes: a Claude state carrying
    /// supervised-CLI invocation artifacts, whose artifacts every Claude path
    /// would silently ignore. Checked where a state is written and where one
    /// is read, so a hand-edited or corrupt drawer is refused at load
    /// rather than half-used.
    ///
    /// A supervised (Codex or Muse) state without artifacts is valid: it is
    /// written before the first launch and by several pauses, and liveness
    /// then judges it on the process table alone.
    pub fn validate(&self) -> Result<(), MemoryError> {
        if self.implementer == Implementer::Claude && self.invocation_artifacts.is_some() {
            return Err(MemoryError::Validation(format!(
                "the dispatch state for {} is a Claude state carrying supervised-CLI invocation \
                 artifacts, which no harness writes",
                self.issue.canonical()
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DispatchStateBody {
    #[serde(default)]
    implementer: Implementer,
    #[serde(default)]
    native_session_id: Option<String>,
    #[serde(default, rename = "codex_artifacts")]
    invocation_artifacts: Option<InvocationArtifacts>,
    issue: u64,
    repo: String,
    worktree_path: String,
    ic_session_name: String,
    dispatch_class: String,
    attempt_n: u32,
    state: String,
    started_at: String,
    session_uuid: String,
    turn_n: u32,
    #[serde(default)]
    session_claimed: bool,
    #[serde(default)]
    session_model: Option<String>,
    #[serde(default)]
    resume_refusals: u32,
    #[serde(default)]
    signing_held: bool,
    /// The token of the run that wrote this record ([`Claim::owner`]).
    /// Absent from records written before it existed, and from those written
    /// outside a run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    /// When the run wrote it ([`Claim::claimed_at`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claimed_at: Option<String>,
    /// The worktree's git common directory ([`Claim::common_dir`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    common_dir: Option<String>,
}

/// A dispatch state as stored, with the token of the run that wrote it.
///
/// Every state a run writes carries a token fresh to that run, so two runs
/// that would otherwise write byte-identical states (a Claude run's first
/// write reuses everything it read) never pass each other's
/// compare-and-set: what one run wrote is never what another read. Kept
/// beside [`DispatchState`] rather than in it so the state's public shape is
/// unchanged; only a run's own writes compare it.
///
/// It is also a lease: `claimed_at` says when its run last claimed the
/// issue, and another run never adopts a claim that is not parked while it
/// is younger than [`CLAIM_LEASE`] (see `run::run_issue`). And it names the
/// repository's git common directory, so whoever must take the issue's lock
/// ([`super::issue_lock`]) finds it even once the worktree is gone.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Claim {
    pub state: DispatchState,
    pub owner: Option<String>,
    /// RFC 3339, when the owning run last wrote it; `None` for a record
    /// written outside a run or before the lease existed, which holds no
    /// lease.
    pub claimed_at: Option<String>,
    /// The git common directory of the worktree it names; `None` when
    /// unknown.
    pub common_dir: Option<String>,
}

/// How long a claim that is not parked keeps another run from adopting it.
///
/// A run claims the issue and only then launches its implementer, and until
/// that implementer is visible in the process table (a process naming its
/// IC with `--name`) nothing distinguishes it from a crashed run. Spawning
/// and exec take well under a second, even on a loaded host, so two minutes
/// covers the window from claim to a process visible in `ps` many times
/// over; the cost is that a crashed run's issue waits two minutes before
/// another run adopts it. A run whose own claim is older than
/// [`CLAIM_LEASE`] less [`CLAIM_REFRESH_MARGIN`] claims again before it
/// launches, so a long gap between its claim and its launch never lets the
/// lease lapse first.
pub(crate) const CLAIM_LEASE: std::time::Duration = std::time::Duration::from_secs(120);

/// How much of [`CLAIM_LEASE`] must remain at a launch, or the run claims
/// again first.
pub(crate) const CLAIM_REFRESH_MARGIN: std::time::Duration = std::time::Duration::from_secs(30);

impl Claim {
    /// How long ago its run claimed it, or `None` when it holds no lease
    /// (no time, or one that does not parse). A time in the future is
    /// measured by its distance too: a far-future time (a clock set wrong
    /// on the claiming host) must not read as a fresh lease for ever.
    pub fn age(&self, now: chrono::DateTime<chrono::Utc>) -> Option<std::time::Duration> {
        let at = chrono::DateTime::parse_from_rfc3339(self.claimed_at.as_deref()?).ok()?;
        Some(
            now.signed_duration_since(at.with_timezone(&chrono::Utc))
                .abs()
                .to_std()
                .unwrap_or_default(),
        )
    }

    /// Whether its lease still keeps another run from adopting it: not
    /// parked, and claimed less than [`CLAIM_LEASE`] ago.
    pub fn leased(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        !self.state.is_parked() && self.age(now).is_some_and(|age| age < CLAIM_LEASE)
    }
}

/// Where a dispatch stands, as [`DispatchState::state`] spells it on the
/// wire. The one list of the labels a run writes, so a reader and a writer
/// cannot spell one differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DispatchPhase {
    /// Written before a launch, and between launches of one run.
    Dispatching,
    /// A launch failed; the run that wrote it may launch again.
    InfrastructureFailure,
    /// Parked at the end of a run: the day's dollar budget is spent.
    PausedDailyBudget,
    /// Parked at the end of a run by repeated infrastructure failures, the
    /// day's unpriced cap, or an escalation.
    PausedInfrastructureFailure,
    /// Parked: the goal condition is too long to dispatch.
    PausedConditionTooLong,
    /// Parked: the harness reported the session still in use.
    PausedSessionInUse,
}

impl DispatchPhase {
    /// Every phase, for a reader that must know them all.
    pub const ALL: [DispatchPhase; 6] = [
        DispatchPhase::Dispatching,
        DispatchPhase::InfrastructureFailure,
        DispatchPhase::PausedDailyBudget,
        DispatchPhase::PausedInfrastructureFailure,
        DispatchPhase::PausedConditionTooLong,
        DispatchPhase::PausedSessionInUse,
    ];

    /// The wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            DispatchPhase::Dispatching => "dispatching",
            DispatchPhase::InfrastructureFailure => "infrastructure-failure",
            DispatchPhase::PausedDailyBudget => "paused-daily-budget",
            DispatchPhase::PausedInfrastructureFailure => "paused-infrastructure-failure",
            DispatchPhase::PausedConditionTooLong => "paused-condition-too-long",
            DispatchPhase::PausedSessionInUse => "paused-session-in-use",
        }
    }

    /// The phase a wire label names, if any.
    pub fn parse(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|phase| phase.as_str() == label)
    }

    /// Whether only a run that has ended writes it. A run in progress may
    /// hold any other phase, and launch again from it.
    pub const fn is_parked(self) -> bool {
        match self {
            DispatchPhase::Dispatching | DispatchPhase::InfrastructureFailure => false,
            DispatchPhase::PausedDailyBudget
            | DispatchPhase::PausedInfrastructureFailure
            | DispatchPhase::PausedConditionTooLong
            | DispatchPhase::PausedSessionInUse => true,
        }
    }
}

impl std::fmt::Display for DispatchPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Logical-key prefix shared by every dispatch-state drawer. One constant
/// because [`all_dispatch_states`] enumerates by it: a prefix spelled
/// differently in the two places would silently enumerate nothing, and
/// "nothing is in flight" is a plausible-looking wrong answer rather than a
/// visible failure.
pub(crate) const DISPATCH_STATE_KEY_PREFIX: &str = "dispatch-state:";

pub(crate) fn dispatch_state_key(issue: &IssueRef) -> String {
    format!("{DISPATCH_STATE_KEY_PREFIX}{}", issue.slug())
}

/// Write (overwrite) the dispatch-state drawer for `state.issue`. Called at
/// dispatch time and at every state transition, per the spec.
pub fn upsert_dispatch_state(db: &Database, state: &DispatchState) -> Result<String, MemoryError> {
    validate_repo(&state.issue.repo)?;
    write_current(db, &dispatch_state_key(&state.issue), &content_of(state)?)
}

/// The persisted body of `state`, refused when [`DispatchState::validate`]
/// refuses it.
fn content_of(state: &DispatchState) -> Result<String, MemoryError> {
    content_of_claim(&Claim {
        state: state.clone(),
        owner: None,
        claimed_at: None,
        common_dir: None,
    })
}

/// [`content_of`], carrying what the claim adds.
fn content_of_claim(claim: &Claim) -> Result<String, MemoryError> {
    let state = &claim.state;
    state.validate()?;
    let body = DispatchStateBody {
        implementer: state.implementer,
        native_session_id: state.native_session_id.clone(),
        invocation_artifacts: state.invocation_artifacts.clone(),
        issue: state.issue.number,
        repo: state.issue.repo.clone(),
        worktree_path: state.worktree_path.clone(),
        ic_session_name: state.ic_session_name.clone(),
        dispatch_class: state.dispatch_class.clone(),
        attempt_n: state.attempt_n,
        state: state.state.clone(),
        started_at: state.started_at.clone(),
        session_uuid: state.session_uuid.clone(),
        turn_n: state.turn_n,
        session_claimed: state.session_claimed,
        session_model: state.session_model.clone(),
        resume_refusals: state.resume_refusals,
        signing_held: state.signing_held,
        owner: claim.owner.clone(),
        claimed_at: claim.claimed_at.clone(),
        common_dir: claim.common_dir.clone(),
    };
    Ok(serde_json::to_string(&body)?)
}

/// `claim` as it reads back from the drawer it would be written to, for an
/// exact comparison with what is stored.
fn normalized(claim: &Claim) -> Result<Claim, MemoryError> {
    claim_from_content(&content_of_claim(claim)?)
}

/// The issue's dispatch state and its owner token, if a drawer exists.
pub(crate) fn get_claim(db: &Database, issue: &IssueRef) -> Result<Option<Claim>, MemoryError> {
    let Some(drawer) = read_current(db, &dispatch_state_key(issue))? else {
        return Ok(None);
    };
    claim_from_content(&drawer.content).map(Some)
}

/// Whether, inside `tx`, the issue's drawer holds exactly `expected` (`None`:
/// no drawer), owner token included.
pub(crate) fn claim_is_tx(
    tx: &rusqlite::Transaction<'_>,
    issue: &IssueRef,
    expected: Option<&Claim>,
) -> Result<bool, MemoryError> {
    use rusqlite::OptionalExtension;
    let expected = expected.map(normalized).transpose()?;
    let id = super::logical_drawer_id(&dispatch_state_key(issue));
    let current: Option<String> = tx
        .query_row("SELECT content FROM drawers WHERE id = ?1", [&id], |row| {
            row.get(0)
        })
        .optional()?;
    let current = current.map(|c| claim_from_content(&c)).transpose()?;
    Ok(current == expected)
}

/// Write `claim` as the issue's dispatch state inside `tx`.
pub(crate) fn write_claim_tx(
    tx: &rusqlite::Transaction<'_>,
    claim: &Claim,
) -> Result<(), MemoryError> {
    validate_repo(&claim.state.issue.repo)?;
    let content = content_of_claim(claim)?;
    super::write_current_tx(tx, &dispatch_state_key(&claim.state.issue), &content)?;
    Ok(())
}

/// Delete the issue's dispatch state inside `tx`, logged as
/// [`clear_dispatch_state`] logs it.
pub(crate) fn delete_claim_tx(
    tx: &rusqlite::Transaction<'_>,
    issue: &IssueRef,
) -> Result<bool, MemoryError> {
    let id = super::logical_drawer_id(&dispatch_state_key(issue));
    let deleted = Database::delete_drawer_tx(tx, &id)?;
    Database::wal_log_tx(
        tx,
        "autopilot_clear_dispatch_state",
        &json!({
            "drawer_id": &id,
            "issue": issue.canonical(),
            "deleted": deleted,
            "conditional": true,
        }),
        None,
    )?;
    Ok(deleted)
}

/// Write `next` only while the drawer holds exactly `expected` (`None`: no
/// drawer), owner token included, in one `BEGIN IMMEDIATE` transaction.
/// Returns `false`, writing nothing, when it changed.
pub(crate) fn claim_if_unchanged(
    db: &Database,
    expected: Option<&Claim>,
    next: &Claim,
) -> Result<bool, MemoryError> {
    db.with_immediate_transaction(|tx| {
        if !claim_is_tx(tx, &next.state.issue, expected)? {
            return Ok(false);
        }
        write_claim_tx(tx, next)?;
        Ok(true)
    })
}

/// Whether the drawer holds exactly `expected`, owner token included: a
/// cheap look before work a later [`claim_if_unchanged`] claims for good.
pub(crate) fn claim_is(
    db: &Database,
    issue: &IssueRef,
    expected: Option<&Claim>,
) -> Result<bool, MemoryError> {
    let expected = expected.map(normalized).transpose()?;
    Ok(get_claim(db, issue)? == expected)
}

/// Delete the drawer only while it holds exactly `expected`, owner token
/// included, in one `BEGIN IMMEDIATE` transaction. Returns `false`,
/// deleting nothing, when it changed or is gone.
pub(crate) fn clear_claim_if_unchanged(
    db: &Database,
    expected: &Claim,
) -> Result<bool, MemoryError> {
    db.with_immediate_transaction(|tx| {
        if !claim_is_tx(tx, &expected.state.issue, Some(expected))? {
            return Ok(false);
        }
        delete_claim_tx(tx, &expected.state.issue)
    })
}

/// Read the dispatch-state drawer for an issue, if one is currently in
/// flight.
pub fn get_dispatch_state(
    db: &Database,
    issue: &IssueRef,
) -> Result<Option<DispatchState>, MemoryError> {
    let Some(drawer) = read_current(db, &dispatch_state_key(issue))? else {
        return Ok(None);
    };
    state_from_content(&drawer.content).map(Some)
}

/// What an issue's dispatch-state drawer holds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StateRead {
    Absent,
    Readable(Box<DispatchState>),
    /// Corrupt, or written by a newer binary for a harness this one does
    /// not know, with why. It may name a live invocation.
    Unreadable(String),
}

/// [`get_dispatch_state`], telling a drawer that cannot be read apart from
/// a failure to read the database at all.
pub(crate) fn read_dispatch_state(
    db: &Database,
    issue: &IssueRef,
) -> Result<StateRead, MemoryError> {
    Ok(match raw_dispatch_state(db, issue)? {
        None => StateRead::Absent,
        Some(raw) => match parse_dispatch_state(&raw) {
            Parsed::Readable(state) => StateRead::Readable(state),
            Parsed::Unreadable(why) => StateRead::Unreadable(why),
        },
    })
}

/// An issue's dispatch-state drawer exactly as stored, for a caller that must
/// act on a drawer it cannot read ([`clear_raw_dispatch_state_if_unchanged`]).
pub(crate) fn raw_dispatch_state(
    db: &Database,
    issue: &IssueRef,
) -> Result<Option<String>, MemoryError> {
    Ok(read_current(db, &dispatch_state_key(issue))?.map(|drawer| drawer.content))
}

/// What a drawer that exists holds: [`StateRead`] without `Absent`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Parsed {
    Readable(Box<DispatchState>),
    /// Why it could not be read.
    Unreadable(String),
}

/// What a raw dispatch-state drawer holds.
pub(crate) fn parse_dispatch_state(raw: &str) -> Parsed {
    match state_from_content(raw) {
        Ok(state) => Parsed::Readable(Box::new(state)),
        Err(e) => Parsed::Unreadable(e.to_string()),
    }
}

fn state_from_content(content: &str) -> Result<DispatchState, MemoryError> {
    claim_from_content(content).map(|claim| claim.state)
}

fn claim_from_content(content: &str) -> Result<Claim, MemoryError> {
    let body: DispatchStateBody = serde_json::from_str(content)?;
    let state = DispatchState {
        implementer: body.implementer,
        native_session_id: body.native_session_id.or_else(|| {
            (body.implementer == Implementer::Claude).then(|| body.session_uuid.clone())
        }),
        invocation_artifacts: body.invocation_artifacts,
        issue: IssueRef::new(body.repo, body.issue),
        worktree_path: body.worktree_path,
        ic_session_name: body.ic_session_name,
        dispatch_class: body.dispatch_class,
        attempt_n: body.attempt_n,
        state: body.state,
        started_at: body.started_at,
        session_uuid: body.session_uuid,
        turn_n: body.turn_n,
        session_claimed: body.session_claimed,
        session_model: body.session_model,
        resume_refusals: body.resume_refusals,
        signing_held: body.signing_held,
    };
    state.validate()?;
    Ok(Claim {
        state,
        owner: body.owner,
        claimed_at: body.claimed_at,
        common_dir: body.common_dir,
    })
}

/// Every in-flight issue's dispatch-state drawer.
///
/// This is the left-hand column of the spec's *Lead crash-safe state* restart
/// table — "what the Lead knew" — which [`super::supervise::reconcile`] joins
/// against the session registry's "who is alive".
///
/// Scoped by `source_file` prefix rather than read out of the room and
/// filtered afterwards: this room also holds every append-only attempt,
/// review and merge record, and a limit applied across all of them
/// newest-first would let ordinary lineage traffic push the in-flight
/// dispatch states out of the window. A reconciliation that cannot see a
/// dispatch state reports its live IC as an orphan — a wrong answer, not an
/// error. See `Database::get_drawers_by_source_prefix`.
///
/// A drawer whose body no longer deserializes is skipped rather than failing
/// the whole reconciliation: one unreadable record must not take every other
/// in-flight issue's supervision offline with it. The queue, which must not
/// treat it as a free slot, reads [`scan_dispatch_states`] instead.
pub fn all_dispatch_states(db: &Database, limit: usize) -> Result<Vec<DispatchState>, MemoryError> {
    Ok(scan_dispatch_states(db, limit)?.states)
}

/// Every dispatch-state drawer, read or not.
#[derive(Debug, Default)]
pub(crate) struct DispatchStateScan {
    /// The drawers that read as dispatch states.
    pub states: Vec<DispatchState>,
    /// The logical key ([`dispatch_state_key`]) of each drawer that did not,
    /// with why: corrupt, refused by [`DispatchState::validate`], or written
    /// by a newer binary (an implementer this one does not know). Any of them
    /// may be a live invocation.
    pub unreadable: Vec<(String, String)>,
}

/// How much of why a drawer cannot be read [`scan_dispatch_states`] keeps.
const UNREADABLE_REASON_CHARS: usize = 300;

/// [`all_dispatch_states`], also naming the drawers it could not read.
pub(crate) fn scan_dispatch_states(
    db: &Database,
    limit: usize,
) -> Result<DispatchStateScan, MemoryError> {
    let prefix = format!(
        "{}{}",
        crate::mcp::tools::LOGICAL_KEY_SOURCE_PREFIX,
        DISPATCH_STATE_KEY_PREFIX
    );
    let drawers = db.get_drawers_by_source_prefix(super::WING, super::ROOM, &prefix, limit)?;
    let mut scan = DispatchStateScan::default();
    for drawer in drawers {
        match state_from_content(&drawer.content) {
            Ok(state) => scan.states.push(state),
            // Scrubbed and bounded once, here: a parse error can quote the
            // drawer, and every reader (the queue, reconcile, the tick's
            // problems) reports it.
            Err(why) => scan.unreadable.push((
                drawer
                    .source_file
                    .strip_prefix(crate::mcp::tools::LOGICAL_KEY_SOURCE_PREFIX)
                    .unwrap_or(&drawer.source_file)
                    .to_string(),
                super::scrub::scrub_and_bound(&why.to_string(), UNREADABLE_REASON_CHARS).text,
            )),
        }
    }
    Ok(scan)
}

/// Clear an issue's dispatch-state drawer once it's no longer in flight
/// (terminal success, exhaustion, or hand-off to a human). Returns `true` if
/// a drawer was actually deleted.
///
/// Wrapped in a transaction with a `wal_log_tx` entry, like every other
/// mutating call in this module ([`upsert_dispatch_state`] via
/// [`write_current`], [`super::lineage::record_attempt`]): without it, a
/// dispatch's crash-safe record could vanish with no audit trail explaining
/// when or why, which is exactly the kind of gap rung 7's reconciliation
/// logic would need to debug around.
pub fn clear_dispatch_state(db: &Database, issue: &IssueRef) -> Result<bool, MemoryError> {
    let id = super::logical_drawer_id(&dispatch_state_key(issue));
    db.with_transaction(|tx| {
        let deleted = Database::delete_drawer_tx(tx, &id)?;
        Database::wal_log_tx(
            tx,
            "autopilot_clear_dispatch_state",
            &json!({
                "drawer_id": &id,
                "issue": issue.canonical(),
                "deleted": deleted,
            }),
            None,
        )?;
        Ok(deleted)
    })
}

/// Delete an issue's dispatch-state drawer only while its raw content is
/// still exactly `expected_raw`, compared byte for byte (owner token, lease
/// and all) in the same `BEGIN IMMEDIATE` transaction. For a drawer that may
/// not be readable; [`clear_claim_if_unchanged`] is the same for one that
/// is.
pub(crate) fn clear_raw_dispatch_state_if_unchanged(
    db: &Database,
    issue: &IssueRef,
    expected_raw: &str,
) -> Result<bool, MemoryError> {
    use rusqlite::OptionalExtension;
    let id = super::logical_drawer_id(&dispatch_state_key(issue));
    db.with_immediate_transaction(|tx| {
        let content: Option<String> = tx
            .query_row("SELECT content FROM drawers WHERE id = ?1", [&id], |row| {
                row.get(0)
            })
            .optional()?;
        let unchanged = content.as_deref() == Some(expected_raw);
        let deleted = unchanged && Database::delete_drawer_tx(tx, &id)?;
        Database::wal_log_tx(
            tx,
            "autopilot_clear_dispatch_state",
            &json!({
                "drawer_id": &id,
                "issue": issue.canonical(),
                "deleted": deleted,
                "conditional": true,
                "raw": true,
            }),
            None,
        )?;
        Ok(deleted)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(issue: &IssueRef, turn_n: u32) -> DispatchState {
        DispatchState {
            implementer: crate::autopilot::implementer::Implementer::Claude,
            native_session_id: None,
            invocation_artifacts: None,
            issue: issue.clone(),
            worktree_path: "/tmp/worktrees/ironmem-283".into(),
            ic_session_name: "ic-ironmem-283".into(),
            dispatch_class: "logic".into(),
            attempt_n: 1,
            state: "in_progress".into(),
            started_at: "2026-08-25T00:00:00Z".into(),
            session_uuid: "11111111-1111-1111-1111-111111111111".into(),
            turn_n,
            session_claimed: true,
            session_model: None,
            resume_refusals: 0,
            signing_held: false,
        }
    }

    /// A claim with no lease and no common directory, as tests write.
    fn bare(state: DispatchState, owner: Option<&str>) -> Claim {
        Claim {
            state,
            owner: owner.map(str::to_owned),
            claimed_at: None,
            common_dir: None,
        }
    }

    #[test]
    fn a_conditional_clear_never_deletes_a_state_rewritten_after_it_was_judged() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        // Callers judge the claim as read back, which normalizes legacy fields.
        upsert_dispatch_state(&db, &sample(&issue, 1)).unwrap();
        let judged = get_claim(&db, &issue).unwrap().unwrap();
        upsert_dispatch_state(&db, &sample(&issue, 2)).unwrap();
        let rewritten = get_claim(&db, &issue).unwrap().unwrap();
        assert!(!clear_claim_if_unchanged(&db, &judged).unwrap());
        assert_eq!(get_claim(&db, &issue).unwrap(), Some(rewritten.clone()));
        assert!(clear_claim_if_unchanged(&db, &rewritten).unwrap());
        assert!(get_dispatch_state(&db, &issue).unwrap().is_none());
        assert!(!clear_claim_if_unchanged(&db, &rewritten).unwrap());
    }
    #[test]
    fn a_conditional_write_never_overwrites_a_state_another_run_wrote() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let ours = bare(sample(&issue, 1), Some("ours"));
        let theirs = bare(sample(&issue, 2), Some("theirs"));
        let turn = |db: &Database| get_dispatch_state(db, &issue).unwrap().map(|s| s.turn_n);
        // Both judged "no drawer"; the first to write wins.
        assert!(claim_if_unchanged(&db, None, &theirs).unwrap());
        assert!(!claim_if_unchanged(&db, None, &ours).unwrap());
        assert_eq!(turn(&db), Some(2));
        // Judged against what is there, the write goes through, compared as
        // read back (a Claude record's absent native id reads as its uuid).
        assert!(theirs.state.native_session_id.is_none());
        assert!(claim_if_unchanged(&db, Some(&theirs), &ours).unwrap());
        assert_eq!(turn(&db), Some(1));
        assert!(!claim_if_unchanged(&db, Some(&theirs), &theirs).unwrap());
        assert_eq!(turn(&db), Some(1));
    }

    #[test]
    fn a_claim_is_leased_while_young_and_not_parked() {
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let now = chrono::Utc::now();
        let mut claim = bare(sample(&issue, 1), Some("a"));
        claim.state.state = DispatchPhase::Dispatching.as_str().into();
        assert!(!claim.leased(now), "no time, no lease");
        claim.claimed_at = Some((now - chrono::Duration::seconds(10)).to_rfc3339());
        assert!(claim.leased(now));
        claim.claimed_at = Some((now - chrono::Duration::seconds(121)).to_rfc3339());
        assert!(!claim.leased(now), "lapsed");
        claim.claimed_at = Some((now + chrono::Duration::seconds(30)).to_rfc3339());
        assert!(claim.leased(now), "a near future time is within the lease");
        claim.claimed_at = Some((now + chrono::Duration::seconds(3600)).to_rfc3339());
        assert!(!claim.leased(now), "a far-future time is not a fresh lease");
        claim.state.state = DispatchPhase::PausedDailyBudget.as_str().into();
        assert!(!claim.leased(now), "a parked claim's run has ended");
        // Round-tripped with its common directory.
        let db = Database::open_in_memory().unwrap();
        claim.common_dir = Some("/r/.git".into());
        assert!(claim_if_unchanged(&db, None, &claim).unwrap());
        assert_eq!(
            get_claim(&db, &issue).unwrap(),
            Some(normalized(&claim).unwrap())
        );
    }

    /// F47/F48: a conditional clear waits out another connection's write
    /// lock instead of failing with `SQLITE_BUSY` on its read-then-delete.
    #[test]
    fn a_conditional_clear_waits_for_a_concurrent_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        let db = Database::open(&path).unwrap();
        db.migrate().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        upsert_dispatch_state(&db, &sample(&issue, 1)).unwrap();
        let judged = get_claim(&db, &issue).unwrap().unwrap();
        let raw = raw_dispatch_state(&db, &issue).unwrap().unwrap();
        for raw_clear in [false, true] {
            if raw_clear {
                upsert_dispatch_state(&db, &sample(&issue, 1)).unwrap();
            }
            let holder = rusqlite::Connection::open(&path).unwrap();
            holder.execute_batch("BEGIN IMMEDIATE").unwrap();
            let clearer = {
                let (path, issue, judged, raw) =
                    (path.clone(), issue.clone(), judged.clone(), raw.clone());
                std::thread::spawn(move || {
                    let db = Database::open(&path).unwrap();
                    if raw_clear {
                        clear_raw_dispatch_state_if_unchanged(&db, &issue, &raw)
                    } else {
                        clear_claim_if_unchanged(&db, &judged)
                    }
                })
            };
            std::thread::sleep(std::time::Duration::from_millis(200));
            holder.execute_batch("COMMIT").unwrap();
            assert!(clearer.join().unwrap().unwrap(), "raw={raw_clear}");
            assert!(get_dispatch_state(&db, &issue).unwrap().is_none());
        }
    }

    /// Two runs that judged the same state and would write identical ones:
    /// their tokens tell the writes apart, so only the first lands, and the
    /// state each read is no longer there for the other.
    #[test]
    fn identical_states_written_by_two_runs_never_both_pass_the_compare_and_set() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let claim = |owner: &str| bare(sample(&issue, 1), Some(owner));
        let read = bare(sample(&issue, 1), None);
        upsert_dispatch_state(&db, &read.state).unwrap();
        let read = get_claim(&db, &issue).unwrap().unwrap();
        assert_eq!(read.owner, None);
        assert!(claim_if_unchanged(&db, Some(&read), &claim("a")).unwrap());
        assert!(!claim_if_unchanged(&db, Some(&read), &claim("b")).unwrap());
        assert_eq!(
            get_claim(&db, &issue).unwrap(),
            Some(normalized(&claim("a")).unwrap())
        );
        assert!(claim_is(&db, &issue, Some(&claim("a"))).unwrap());
        assert!(!claim_is(&db, &issue, Some(&claim("b"))).unwrap());
        assert!(!clear_claim_if_unchanged(&db, &claim("b")).unwrap());
        // The token is invisible to every reader of the state itself.
        assert_eq!(
            get_dispatch_state(&db, &issue).unwrap(),
            Some(normalized(&claim("a")).unwrap().state)
        );
        assert!(clear_claim_if_unchanged(&db, &claim("a")).unwrap());
        assert!(get_claim(&db, &issue).unwrap().is_none());
    }

    #[test]
    fn every_phase_round_trips_its_label_and_only_pauses_are_parked() {
        for phase in DispatchPhase::ALL {
            assert_eq!(DispatchPhase::parse(phase.as_str()), Some(phase));
            assert_eq!(
                serde_json::to_string(&phase).unwrap(),
                format!("\"{}\"", phase.as_str())
            );
            assert_eq!(phase.is_parked(), phase.as_str().starts_with("paused-"));
        }
        assert_eq!(DispatchPhase::parse("in_progress"), None);
    }

    #[test]
    fn codex_wire_roundtrip_keeps_native_identity_and_absolute_artifacts() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 283);
        let mut state = sample(&issue, 7);
        state.implementer = Implementer::Codex;
        state.native_session_id = Some("native-id".into());
        state.invocation_artifacts = Some(InvocationArtifacts {
            invocation_id: "invocation".into(),
            schema_path: "/original/schema".into(),
            journal_path: "/original/journal".into(),
            final_path: "/original/final".into(),
            stderr_path: "/original/stderr".into(),
            exit_path: "/original/exit".into(),
        });
        upsert_dispatch_state(&db, &state).unwrap();
        assert_eq!(get_dispatch_state(&db, &issue).unwrap(), Some(state));
        let drawer = read_current(&db, &dispatch_state_key(&issue))
            .unwrap()
            .unwrap();
        let invalid = drawer.content.replace("\"codex\"", "\"unknown-harness\"");
        write_current(&db, &dispatch_state_key(&issue), &invalid).unwrap();
        assert!(get_dispatch_state(&db, &issue).is_err());
    }

    fn sample_body() -> DispatchStateBody {
        DispatchStateBody {
            implementer: Implementer::Claude,
            native_session_id: None,
            invocation_artifacts: None,
            issue: 283,
            repo: "ironmem".into(),
            worktree_path: "/absolute/worktree".into(),
            ic_session_name: "ic".into(),
            dispatch_class: "logic".into(),
            attempt_n: 1,
            state: "dispatching".into(),
            started_at: "now".into(),
            session_uuid: "old-id".into(),
            turn_n: 3,
            session_claimed: true,
            session_model: None,
            resume_refusals: 0,
            signing_held: false,
            owner: None,
            claimed_at: None,
            common_dir: None,
        }
    }

    #[test]
    fn invocation_artifacts_keep_the_codex_artifacts_wire_key() {
        let artifacts = InvocationArtifacts {
            invocation_id: "11111111-2222-3333-4444-555555555555".into(),
            schema_path: "/r/.git/ironmem/autopilot/o-r-1/i/schema.json".into(),
            journal_path: "/r/.git/ironmem/autopilot/o-r-1/i/journal.jsonl".into(),
            final_path: "/r/.git/ironmem/autopilot/o-r-1/i/final.json".into(),
            stderr_path: "/r/.git/ironmem/autopilot/o-r-1/i/stderr.log".into(),
            exit_path: "/r/.git/ironmem/autopilot/o-r-1/i/exit.json".into(),
        };
        let body = DispatchStateBody {
            invocation_artifacts: Some(artifacts.clone()),
            ..sample_body()
        };
        let wire = serde_json::to_value(&body).unwrap();
        assert_eq!(
            wire["codex_artifacts"]["invocation_id"],
            artifacts.invocation_id
        );
        assert!(wire.get("invocation_artifacts").is_none());
        let back: DispatchStateBody = serde_json::from_value(wire).unwrap();
        assert_eq!(back.invocation_artifacts, Some(artifacts));
    }

    #[test]
    fn old_wire_defaults_to_claude_and_native_uuid() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 283);
        let mut body = serde_json::to_value(DispatchStateBody {
            implementer: Implementer::Claude,
            native_session_id: None,
            invocation_artifacts: None,
            issue: 283,
            repo: "ironmem".into(),
            worktree_path: "/absolute/worktree".into(),
            ic_session_name: "ic".into(),
            dispatch_class: "logic".into(),
            attempt_n: 1,
            state: "dispatching".into(),
            started_at: "now".into(),
            session_uuid: "old-id".into(),
            turn_n: 3,
            session_claimed: true,
            session_model: None,
            resume_refusals: 0,
            signing_held: false,
            owner: None,
            claimed_at: None,
            common_dir: None,
        })
        .unwrap();
        for key in [
            "implementer",
            "native_session_id",
            "codex_artifacts",
            "session_model",
            "resume_refusals",
            "signing_held",
        ] {
            body.as_object_mut().unwrap().remove(key);
        }
        write_current(&db, &dispatch_state_key(&issue), &body.to_string()).unwrap();
        let state = get_dispatch_state(&db, &issue).unwrap().unwrap();
        assert_eq!(
            state.implementer,
            super::super::implementer::Implementer::Claude
        );
        assert_eq!(state.native_session_id.as_deref(), Some("old-id"));
        assert!(state.session_claimed);
        // An older record names no model, so it never rotates on one.
        assert_eq!(state.session_model, None);
        assert_eq!(state.resume_refusals, 0);
        assert!(!state.signing_held);
    }

    #[test]
    fn the_session_model_and_refusal_count_round_trip() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let mut state = sample(&issue, 1);
        state.session_model = Some("muse-wonder-1.0".into());
        state.resume_refusals = 1;
        state.signing_held = true;
        upsert_dispatch_state(&db, &state).unwrap();
        let read = get_dispatch_state(&db, &issue).unwrap().unwrap();
        assert_eq!(read.session_model.as_deref(), Some("muse-wonder-1.0"));
        assert_eq!(read.resume_refusals, 1);
        assert!(read.signing_held);
    }

    #[test]
    fn dispatch_state_round_trips_and_overwrites() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 283);

        let id_first = upsert_dispatch_state(&db, &sample(&issue, 1)).unwrap();
        let id_second = upsert_dispatch_state(&db, &sample(&issue, 2)).unwrap();
        assert_eq!(
            id_first, id_second,
            "same issue must resolve to the same dispatch-state drawer"
        );

        let current = get_dispatch_state(&db, &issue).unwrap().unwrap();
        assert_eq!(current.turn_n, 2);
        assert_eq!(current.session_uuid, "11111111-1111-1111-1111-111111111111");
        assert_eq!(current.issue, issue);
    }

    #[test]
    fn missing_dispatch_state_is_none_not_an_error() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 9999);
        assert_eq!(get_dispatch_state(&db, &issue).unwrap(), None);
    }

    #[test]
    fn clear_dispatch_state_removes_it() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 283);
        upsert_dispatch_state(&db, &sample(&issue, 1)).unwrap();

        assert!(clear_dispatch_state(&db, &issue).unwrap());
        assert_eq!(get_dispatch_state(&db, &issue).unwrap(), None);
        // Idempotent: clearing an already-cleared issue is not an error.
        assert!(!clear_dispatch_state(&db, &issue).unwrap());
    }

    #[test]
    fn all_dispatch_states_returns_every_in_flight_issue() {
        let db = Database::open_in_memory().unwrap();
        let a = IssueRef::new("ironrace/ironmem", 283);
        let b = IssueRef::new("ironrace/other", 7);
        upsert_dispatch_state(&db, &sample(&a, 1)).unwrap();
        upsert_dispatch_state(&db, &sample(&b, 1)).unwrap();

        let states = all_dispatch_states(&db, 100).unwrap();
        assert_eq!(states.len(), 2);
        assert!(states.iter().any(|s| s.issue == a));
        assert!(states.iter().any(|s| s.issue == b));

        // Cleared issues drop out.
        clear_dispatch_state(&db, &a).unwrap();
        let states = all_dispatch_states(&db, 100).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].issue, b);
    }

    #[test]
    fn all_dispatch_states_ignores_other_kinds_in_the_same_room() {
        // The room also holds issue-status, gate-config, budget and every
        // append-only lineage record. Only dispatch states are in flight.
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        upsert_dispatch_state(&db, &sample(&issue, 1)).unwrap();
        super::super::write_current(&db, "issue-status:ironrace-ironmem-283", "{}").unwrap();
        super::super::write_current(&db, "budget:2026-09-01", "{}").unwrap();

        let states = all_dispatch_states(&db, 100).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].issue, issue);
    }

    #[test]
    fn an_undeserializable_dispatch_state_is_skipped_not_fatal() {
        // One corrupt record must not take every other in-flight issue's
        // supervision offline with it.
        let db = Database::open_in_memory().unwrap();
        let good = IssueRef::new("ironrace/ironmem", 283);
        upsert_dispatch_state(&db, &sample(&good, 1)).unwrap();
        super::super::write_current(
            &db,
            &format!("{DISPATCH_STATE_KEY_PREFIX}corrupt-1"),
            "not json",
        )
        .unwrap();

        let states = all_dispatch_states(&db, 100).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].issue, good);
    }

    // ── Regression: clearing must leave the same wal-log audit trail every
    // other mutation in this module leaves. ─────────────────────────────────
    #[test]
    fn clear_dispatch_state_writes_a_wal_log_entry() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 283);
        upsert_dispatch_state(&db, &sample(&issue, 1)).unwrap();

        assert!(clear_dispatch_state(&db, &issue).unwrap());

        let count: i64 = db
            .with_connection(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM wal_log WHERE operation = ?1",
                    rusqlite::params!["autopilot_clear_dispatch_state"],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(
            count, 1,
            "clearing a dispatch-state drawer must log exactly one wal entry"
        );

        // Clearing an already-cleared issue still logs (deleted: false), so
        // the audit trail records the no-op attempt too.
        assert!(!clear_dispatch_state(&db, &issue).unwrap());
        let count_after_noop: i64 = db
            .with_connection(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM wal_log WHERE operation = ?1",
                    rusqlite::params!["autopilot_clear_dispatch_state"],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(count_after_noop, 2);
    }

    #[test]
    fn a_claude_state_carrying_invocation_artifacts_is_refused_at_load() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let mut body: serde_json::Value =
            serde_json::from_str(&content_of(&sample(&issue, 1)).unwrap()).unwrap();
        body["codex_artifacts"] = serde_json::json!({
            "invocation_id": "i",
            "schema_path": "/s",
            "journal_path": "/j",
            "final_path": "/f",
            "stderr_path": "/e",
            "exit_path": "/x",
        });
        super::super::write_current(&db, &dispatch_state_key(&issue), &body.to_string()).unwrap();
        let err = get_dispatch_state(&db, &issue).unwrap_err().to_string();
        assert!(err.contains("Claude"), "{err}");
        let scan = scan_dispatch_states(&db, 100).unwrap();
        assert!(scan.states.is_empty());
        assert_eq!(scan.unreadable.len(), 1);
        let (key, why) = &scan.unreadable[0];
        assert_eq!(key, &dispatch_state_key(&issue));
        assert!(
            why.contains("Claude state carrying supervised-CLI invocation artifacts"),
            "{why}"
        );
    }

    #[test]
    fn an_unreadable_dispatch_state_is_reported_by_its_key() {
        let db = Database::open_in_memory().unwrap();
        let good = IssueRef::new("ironrace/ironmem", 283);
        upsert_dispatch_state(&db, &sample(&good, 1)).unwrap();
        let corrupt = format!("{DISPATCH_STATE_KEY_PREFIX}corrupt-1");
        super::super::write_current(&db, &corrupt, "not json").unwrap();
        let scan = scan_dispatch_states(&db, 100).unwrap();
        assert_eq!(scan.states.len(), 1);
        assert_eq!(scan.unreadable.len(), 1);
        assert_eq!(scan.unreadable[0].0, corrupt);
        assert!(!scan.unreadable[0].1.is_empty());
        // Bounded however long the parse error.
        let long = format!("{DISPATCH_STATE_KEY_PREFIX}long-1");
        super::super::write_current(&db, &long, &format!("{{\"x\": {}", "1".repeat(5000))).unwrap();
        let scan = scan_dispatch_states(&db, 100).unwrap();
        assert!(scan
            .unreadable
            .iter()
            .all(|(_, why)| why.chars().count() <= UNREADABLE_REASON_CHARS + 1));
    }
}

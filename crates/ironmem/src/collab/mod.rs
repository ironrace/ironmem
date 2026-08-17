//! State machine for the bounded two-party Claude↔Codex planning + coding flow.
//!
//! **Design boundary:** This collab protocol version is intentionally
//! Claude↔Codex-specific.  Generic harness identity is extensible via
//! [`crate::harness::HarnessId`] and the open [`crate::harness::REGISTRY`];
//! adding a third collab participant would require a new protocol version, not
//! a registry entry.  The [`Agent`] role enum is the compiler-enforced type
//! for collab protocol roles; harness-generic code uses `HarnessId` instead.
//!
//! v1 covers planning: `PlanParallelDrafts` → `PlanSynthesisPending`
//! → `PlanCopilotReviewPending` → `PlanFinalizePending` → `PlanLocked`.
//!
//! v3 extends `PlanLocked` with a human-approved coding loop. A single
//! pilot `task_list` send transitions out of `PlanLocked` into the batch
//! implementation phase (`CodeImplementPending`), where the selected
//! implementer orchestrates per-task subagents (via `iron-build`, or
//! directly when `execution_mode` says so) entirely on its side. A
//! single `implementation_done` send jumps to the global 3-phase review
//! flow (`CodeReviewFixGlobalPending` → `CodeReviewLocalPending` →
//! `CodeReviewFinalPending`) — the copilot reviews the raw
//! post-implementation diff first, then the pilot audits the copilot's
//! commits via `/ultrareview-local`, then the pilot opens the PR — and lands
//! directly in `CodingComplete` (terminal) on success — the final pilot turn
//! opens the PR and carries its URL. "Pilot" is the session's `pilot` agent
//! and "copilot" is its counterpart; the split is per-session, not a fixed
//! Claude/Codex assignment. `CodingFailed` is terminal for this session generation, but not
//! always permanent: a `Tooling`-classified failure with a recorded
//! `failed_from_phase` can be restored to that phase via `ResumeCoding`
//! (the `collab_resume` MCP tool), while a `Terminal`-classified failure
//! (unrecognized causes, `branch_drift:`, `subagent_failure:`, or a
//! recoverable report that exceeded the retry ceiling) is genuinely
//! unrecoverable. See [`failure_class::classify`] and
//! [`MAX_RECOVERY_ATTEMPTS`] for the exact rule.

pub mod checkpoint;
pub mod handoff;
pub mod queue;

mod agent;
mod error;
mod event;
mod failure_class;
mod phase;
mod session;
mod state_machine;
mod task_list;

pub use agent::{Agent, CollabRoles};
pub use checkpoint::{
    AttestationCheck, AttestedBy, CheckpointError, CheckpointStatus, CollabCheckpoint,
    ATTESTATION_UNRECORDED,
};
pub use error::CollabError;
/// Refusal-formatting helpers shared by the two `head_sha` seed sites.
/// Exported alongside [`CollabError`] because the bound is observable in
/// `MalformedHeadSha`'s payload: a caller comparing that field against what it
/// sent needs to know it may have been cut.
pub use error::{echo_head_sha, MAX_ECHOED_HEAD_SHA_CHARS};
pub use event::CollabEvent;
pub use failure_class::{classify, FailureClass};
#[cfg(test)]
pub use handoff::load_or_init_actor_generation;
pub use handoff::{
    claim_handoff_token, issue_or_reuse_handoff, read_actor_generation, ActorGeneration,
    HandoffIssue, PendingHandoff,
};
pub use phase::Phase;
pub use session::{tasks_count_from_list, CollabSession};
pub(crate) use state_machine::copilot;
pub use state_machine::{apply_event, start_global_review_session, MAX_RECOVERY_ATTEMPTS};
pub(crate) use task_list::{
    task_count_from_payload, validate_task_list_body, TaskListValidationError,
};

/// Maximum implementation tasks accepted by one collab session. Larger work
/// must be split into independently executable child issues before collab
/// planning is approved.
pub const MAX_TASKS_PER_COLLAB_ISSUE: u32 = 15;

/// The `coding_failure` column's hard bound, mirrored from
/// `migrations/005_collab_v2.sql:26-27`:
/// `CHECK (coding_failure IS NULL OR length(coding_failure) <= 2048)`.
///
/// **Characters, not bytes.** SQLite's `length()` on a TEXT value counts
/// characters, so this bound and the CHECK it mirrors are both
/// character-counted. Two callers derive from this single `2048` rather than
/// each restating it:
///
/// * [`crate::mcp::tools::collab_events::parse_failure_report_event`]
///   enforces it with `.chars().count()` — the correct measurement for a
///   character-counted CHECK — and that measurement is unchanged by this
///   constant's existence; do not switch it to bytes.
/// * [`MAX_ABANDON_REASON_BYTES`] below derives a *byte*-counted cap from
///   this same number — see its doc for why that unit mismatch is
///   deliberate and still safe.
pub const MAX_CODING_FAILURE_CHARS: usize = 2048;

/// The largest `reason` an abandon may carry, in **bytes** — deliberately a
/// different unit from [`MAX_CODING_FAILURE_CHARS`], which this is derived
/// from.
///
/// The DB CHECK and [`crate::mcp::tools::collab_events::parse_failure_report_event`]
/// both measure `coding_failure` in *characters* (`.chars().count()`, matching
/// SQLite's character-counted `length()`). This cap measures the `reason`
/// half of that string in *bytes* instead, via
/// [`crate::sanitize::sanitize_content`], which measures `str::len()`. That is
/// the conservative side: a UTF-8 string's byte count is always `>=` its char
/// count, so `bytes(reason) <= MAX_ABANDON_REASON_BYTES` implies
/// `chars(reason) <= MAX_ABANDON_REASON_BYTES` too. The stored value is
/// `ABANDONED_PREFIX + " " + reason`, and `ABANDONED_PREFIX` and the
/// separator are pure ASCII (byte count == char count for both), so
/// `chars(stored) = chars(reason) + 11 <= MAX_ABANDON_REASON_BYTES + 11 ==
/// MAX_CODING_FAILURE_CHARS` — a reason this cap admits can never be the
/// thing that trips the DB `CHECK`, exactly at the boundary as well as under
/// it. See `queue::tests::max_length_abandon_reason_clears_the_coding_failure_check`
/// for the exact-boundary case exercised against the real DB.
pub const MAX_ABANDON_REASON_BYTES: usize = MAX_CODING_FAILURE_CHARS - ABANDONED_PREFIX.len() - 1;

/// How long a collab session must show no activity before `collab_end`'s
/// abandon arm will end it: six hours.
///
/// Deliberately generous (D2). The false positive — ending a session that is
/// merely slow — is destructive and unrecoverable; the false negative costs
/// only a wait. The field case in #283 had been wedged for three days.
///
/// One constant serves both abandon here and lease recovery in #298; do not
/// fork a second threshold for that.
pub const COLLAB_DEAD_SESSION_SECS: i64 = 21_600;

/// Seconds since the session's newest activity, or `None` when there is no
/// signal at all — i.e. the session row does not exist
/// ([`queue::session_last_activity`] returns `None` for that case, and
/// nothing else does).
///
/// May be negative if the server clock moved backwards between the write and
/// this read; [`session_is_dead`] treats that as live.
///
/// Uses `saturating_sub`, not plain subtraction. `collab_checkpoints.updated_at`
/// is `INTEGER NOT NULL` with no range `CHECK`, so a hand-repaired row could in
/// principle hold `i64::MIN`; `now - i64::MIN` overflows `i64::MAX`, which
/// panics under debug assertions and wraps under release ones. Wrapping is the
/// dangerous direction here: two's-complement wraparound turns that overflow
/// into a large *negative* result, which reads as a session whose activity is
/// far in the future — live, on data that is actually nonsense. `saturating_sub`
/// removes both failure modes at once: it cannot panic, and it clamps toward
/// `i64::MAX` instead of wrapping past it, so pathologically-ancient input
/// reads as maximally idle (dead) rather than accidentally masquerading as
/// live. That is the correct classification for `i64::MIN` on its own
/// merits, not merely the safe fallback — there is no legitimate story in
/// which a session's true newest activity is the smallest representable
/// integer.
///
/// **A malformed timestamp does not produce `None` here.** SQLite's
/// `strftime('%s', ...)` returns NULL for an unparseable value, but
/// [`queue::session_last_activity`]'s mandated `coalesce(..., 0)` around every
/// term turns that NULL into `0` before it ever reaches `max()` — so a
/// corrupt `collab_sessions.updated_at` reads as epoch 0, not as a missing
/// signal, and this function returns `Some(huge_number)`, comfortably over
/// [`COLLAB_DEAD_SESSION_SECS`]. The practical effect: a session whose *only*
/// activity source is a corrupt timestamp reads dead **without** going
/// through `session_is_dead`'s `None` arm, so its `tracing::warn` never
/// fires — the corruption is silent. This is narrow in practice:
/// `collab_sessions.updated_at` is only ever written by `datetime('now')`, so
/// reaching this case requires direct DB corruption, and a session with any
/// live message or checkpoint still reads live off those terms regardless of
/// what the session row's timestamp says.
pub fn idle_secs(last_activity: Option<i64>, now: i64) -> Option<i64> {
    last_activity.map(|last| now.saturating_sub(last))
}

/// Whether a session has been silent long enough to be abandoned.
///
/// The signal is [`queue::session_last_activity`] — the newest of
/// `collab_sessions.updated_at`, `collab_checkpoints.updated_at`, and the
/// session's newest `messages.created_at`. See that function for why all three
/// are needed and why no migration was required.
///
/// **A missing signal counts as dead**, and fires a `tracing::warn` so the
/// degrade is observable rather than silent (repo convention:
/// `mcp/tools/collab_session.rs:1801`). `None` here does not mean the
/// activity query degraded — a degraded query returns `Err`, which the
/// caller propagates, never reaching this function at all.
/// [`queue::session_last_activity`] returns `Ok(None)` for exactly one
/// reason: the session row itself is gone. The only way to reach this arm on
/// the abandon path, where `ensure_active` establishes the row exists first,
/// is a delete racing in between that read and this one — the row vanishing
/// mid-flight, not a read that failed.
///
/// This direction is safe regardless: refusing to abandon on a row that no
/// longer exists would recreate the wedge this feature exists to clear, and
/// there is no session left to wrongly end. Once Task 2 wires this check to
/// run inside the same write transaction as the state change it authorizes
/// (D6), the race disappears entirely — `ensure_active` and
/// `session_last_activity` observe the same snapshot, so the row cannot
/// vanish between them, and this `None` arm becomes effectively unreachable
/// in practice. It stays here as the correct handling for the case, not as
/// dead code: the fail-safe direction (dead, loudly) is right even if the
/// case that reaches it becomes vanishingly rare.
pub fn session_is_dead(session_id: &str, last_activity: Option<i64>, now: i64) -> bool {
    match idle_secs(last_activity, now) {
        Some(idle) => idle >= COLLAB_DEAD_SESSION_SECS,
        None => {
            tracing::warn!(
                session_id = %session_id,
                "collab: no liveness signal for session; treating it as dead for abandon"
            );
            true
        }
    }
}

/// Prefix on `coding_failure` that marks a session an operator abandoned via
/// `collab_end { "abandon": true }`.
///
/// **Deliberately absent from [`RECOVERABLE_FAILURE_PREFIXES`].**
/// [`failure_class::classify`] therefore returns
/// [`failure_class::FailureClass::Terminal`] for it by the unrecognized-string
/// rule, which is exactly the intent: an abandoned session is sealed, and
/// nothing — `collab_resume` least of all — may resurrect it. Adding it to the
/// recoverable set would make abandon reversible and defeat the whole gate.
/// [`failure_class::tests::abandoned_prefix_classifies_terminal`] pins this.
///
/// It is also **not** in [`OFF_TURN_FAILURE_PREFIXES`]: it never arrives as a
/// `failure_report` at all. It is written directly by `handle_collab_end`'s
/// abandon arm, so the off-turn admissibility question does not apply to it.
///
/// That last sentence is an *enforced* property, not an intent.
/// [`crate::mcp::tools::collab_events::parse_failure_report_event`] refuses a
/// caller-supplied report carrying this prefix. Without that refusal an agent
/// could mint a `coding_failure` indistinguishable from a real abandon in every
/// later audit, in `collab_status`, and in the seal message
/// [`queue::ensure_active`] echoes — the prefix is the *only* thing that marks
/// the row as an operator's decision rather than an agent's.
pub const ABANDONED_PREFIX: &str = "abandoned:";

/// Prefix on `coding_failure` that marks a failure as "branch drift" — a
/// mismatch the non-owner may detect via its own git ops.
pub const BRANCH_DRIFT_PREFIX: &str = "branch_drift:";

/// Prefix on `coding_failure` that marks a failure as a Codex MCP
/// dispatch failure observed by Claude during `--implementer=codex`. It
/// shares the off-turn admit path with `branch_drift:` because the
/// non-owner (Claude in this case) is the only agent able to detect
/// that the owner's MCP session never advanced — Codex itself isn't
/// running to emit a regular failure report. Unlike branch drift, it is
/// admissible only from the phases whose Codex turn Claude actually
/// dispatches — see [`dispatch_failure_phase_admits`].
pub const CODEX_DISPATCH_FAILED_PREFIX: &str = "codex_dispatch_failed:";

/// A `coding_failure` reporting that the session's current checkpoint no
/// longer describes the repo: git HEAD has advanced past
/// `checkpoint.head_sha`, so the checkpoint's task-progress claim is stale.
///
/// Recoverable (`FailureClass::Tooling`), which is the deliberate asymmetry
/// with [`BRANCH_DRIFT_PREFIX`]: branch drift means the work is on the wrong
/// branch and cannot be fixed in place, while checkpoint drift means the
/// ledger is merely behind the work and is fixed by writing an accurate
/// checkpoint.
///
/// Off-turn admissible, but only from `CodeImplementPending` — see
/// [`checkpoint_drift_phase_admits`]. Being recoverable is exactly why the
/// scope is needed: admitting a Tooling report parks the session and hands
/// the turn to a new owner, so an unscoped carve-out would be a turn-seizure
/// primitive.
pub const CHECKPOINT_DRIFT_PREFIX: &str = "checkpoint_drift:";

/// The vocabulary of prefixes that have an off-turn carve-out at all: branch
/// drift, checkpoint drift, and Codex dispatch failure.
///
/// **This array is documentation only. Nothing reads it at runtime.** The gate
/// is [`off_turn_failure_is_admissible`], which spells out one clause per
/// prefix — branch drift unconditionally, checkpoint drift only from
/// `CodeImplementPending`, Codex dispatch failure only from Claude against a
/// Codex-owned turn in a phase Claude could have dispatched — and never
/// consults this constant. Adding a prefix here therefore changes nothing:
/// the new prefix stays off-turn-inadmissible until it also gets its own
/// clause in that function.
///
/// It is a list and not a match arm on purpose. The three scoping rules are
/// irreconcilable — one is unconditional, one is phase-scoped, one is scoped
/// on reporter *and* owner *and* phase *and* implementer — and a `&[&str]`
/// cannot carry any of that, so an iteration over this array could only ever
/// re-implement the loosest of the three. Keep the two in sync by hand, and
/// treat the function as the authority whenever they disagree.
pub const OFF_TURN_FAILURE_PREFIXES: &[&str] = &[
    BRANCH_DRIFT_PREFIX,
    CHECKPOINT_DRIFT_PREFIX,
    CODEX_DISPATCH_FAILED_PREFIX,
];

/// Phases from which a `codex_dispatch_failed:` report can possibly be
/// legitimate — the ones whose Codex-owned turn Claude spawns as an MCP
/// one-shot and can therefore watch fail to return:
///
/// * `CodeImplementPending`, when `implementer == Agent::Codex` — the batch
///   implementation turn Claude dispatched.
/// * `CodeReviewFixGlobalPending` — the copilot's post-implementation fix
///   turn. Combined with the `current_owner == Agent::Codex` requirement in
///   [`off_turn_failure_is_admissible`], this only ever admits a session
///   whose copilot is in fact Codex.
///
/// `CodeReviewLocalPending` and `CodeReviewFinalPending` are deliberately
/// excluded: those are the *pilot's* own audit and PR turns, which Claude
/// never dispatches, so a dispatch-failure report against them is either
/// meaningless or an attempt to seize the turn. Planning and terminal phases
/// are excluded for the same reason — nothing is dispatched from them.
///
/// The match is exhaustive on purpose: a new `Phase` must make an explicit
/// admit/deny decision here rather than inheriting a wildcard.
fn dispatch_failure_phase_admits(phase: Phase, implementer: Agent) -> bool {
    match phase {
        Phase::CodeImplementPending => implementer == Agent::Codex,
        Phase::CodeReviewFixGlobalPending => true,
        Phase::PlanParallelDrafts
        | Phase::PlanSynthesisPending
        | Phase::PlanCopilotReviewPending
        | Phase::PlanFinalizePending
        | Phase::PlanLocked
        | Phase::CodeReviewLocalPending
        | Phase::CodeReviewFinalPending
        | Phase::CodingComplete
        | Phase::CodingFailed => false,
    }
}

/// Phases from which a `checkpoint_drift:` report can be legitimate while the
/// reporter is off-turn: only `CodeImplementPending`.
///
/// `CodeImplementPending` is the one phase in which a checkpoint is *under
/// construction* — the implementer commits as it works and files checkpoints
/// as it goes, so its ledger is the thing that can fall behind HEAD (batch
/// commits 28 changes, checkpoint still frozen at task 1). That is also where
/// the off-turn carve-out earns its keep: the non-implementer is the one
/// positioned to run the HEAD-vs-checkpoint comparison, and handing recovery
/// to it so the ledger gets refiled is the intended behavior.
///
/// The three review phases are deliberately excluded. Past
/// `implementation_done` the checkpoint is frozen proof of what was built, not
/// a live ledger; drift observed there is the `implementation_done` gate's
/// business and the handoff/resume diagnostics' business, not a live off-turn
/// `failure_report`'s. Admitting it there buys nothing and costs the
/// pilot/copilot separation: `CodeReviewLocalPending` and
/// `CodeReviewFinalPending` are the *pilot's* audit and PR turns, and since
/// `checkpoint_drift:` classifies `Tooling`, admitting it there would park the
/// session and hand those turns to the copilot — who may well have authored
/// the commits under audit. Planning and terminal phases have no checkpoint
/// activity at all.
///
/// The match is exhaustive on purpose, mirroring
/// [`dispatch_failure_phase_admits`]: a new `Phase` must make an explicit
/// admit/deny decision here rather than inheriting a wildcard.
fn checkpoint_drift_phase_admits(phase: Phase) -> bool {
    match phase {
        Phase::CodeImplementPending => true,
        Phase::PlanParallelDrafts
        | Phase::PlanSynthesisPending
        | Phase::PlanCopilotReviewPending
        | Phase::PlanFinalizePending
        | Phase::PlanLocked
        | Phase::CodeReviewFixGlobalPending
        | Phase::CodeReviewLocalPending
        | Phase::CodeReviewFinalPending
        | Phase::CodingComplete
        | Phase::CodingFailed => false,
    }
}

/// Whether an agent may report this failure while it is not the current
/// owner.
///
/// **Branch drift** is the one unconditional clause, and detectability is not
/// why. Either participant can run a git comparison without owning the turn,
/// but that is a *necessary* condition for an off-turn carve-out, never a
/// sufficient one. What makes an unscoped clause safe is that admitting it
/// cannot hand anyone a live turn: `branch_drift:` classifies
/// [`failure_class::FailureClass::Terminal`], so `apply_event` sends the
/// session straight to `CodingFailed` and there is no turn left to seize. Any
/// reporter, any phase, same outcome — a dead session.
///
/// **Checkpoint drift** is equally detectable off-turn and is nonetheless
/// scoped, because it classifies `Tooling`. A recoverable report parks the
/// session in-phase and installs a *new* `current_owner` (the counterpart of
/// the interrupted owner, i.e. the off-turn reporter), who may then execute
/// the phase's advancing event via `require_actor_or_recovery`. Unscoped, that
/// is a turn-seizure primitive: the copilot could park the pilot's
/// `/ultrareview-local` audit turn, take it, audit its own commits, and repeat
/// for the PR turn. [`checkpoint_drift_phase_admits`] confines it to
/// `CodeImplementPending`, the only phase where a stale checkpoint is a live
/// problem and where handing recovery over is the intended remedy.
///
/// The discriminator to reuse when adding a prefix here is therefore
/// Terminal-vs-Tooling, not observability: an off-turn-admissible prefix may
/// be unscoped only if admitting it cannot leave anyone holding a live turn.
///
/// A Codex-dispatch failure is narrower on two independent axes.
///
/// **Who** may report it: only the *dispatcher* can observe that a
/// Codex-owned background dispatch never ran, so accepting it from Codex (or
/// while Claude owns the turn) would let a non-owner seize a live Claude
/// turn. The `reporter == Agent::Claude` term below names the **dispatcher**,
/// not the pilot. Claude is unconditionally the dispatcher in this codebase —
/// it is the side that spawns the Codex MCP one-shot — and that role is
/// orthogonal to the session's `pilot`/`copilot` assignment. Under
/// `pilot=codex` Claude is *still* the dispatcher, so this clause is
/// deliberately unaffected by pilot assignment. This function is handed
/// `phase` and `implementer` but never the session or its `pilot`, which is
/// what keeps that independence structural rather than merely intended. Do
/// not "generalize" this literal to `pilot(session)` or `copilot(session)`:
/// doing so would let Codex fabricate a report about its own process and
/// seize a live Claude turn.
///
/// **Where** it may be reported: only from a phase whose Codex turn Claude
/// could have dispatched, per [`dispatch_failure_phase_admits`]. An earlier
/// version of this function took no phase at all, and that was genuinely safe
/// while `CodeReviewLocalPending`/`CodeReviewFinalPending` hardcoded Claude
/// as the expected actor — the carve-out firing there handed the turn to
/// Claude, who was already the expected actor, so nothing was seized. Once
/// those two phases became pilot-owned they can be *Codex*-owned, and a
/// phase-blind carve-out would let Claude — the copilot under `pilot=codex` —
/// fabricate a dispatch failure, take the pilot's audit turn and then its PR
/// turn, and end up auditing its own commits. Since `pilot` and `implementer`
/// are deliberately uncorrelated, that is exactly the self-review the
/// pilot/copilot split exists to prevent. Phase awareness is what closes it.
///
/// A recognized prefix also needs at least one byte of detail. This keeps the
/// pre-dispatch turn gate aligned with the state-machine enforcement.
pub fn off_turn_failure_is_admissible(
    coding_failure: &str,
    reporter: Agent,
    current_owner: Agent,
    phase: Phase,
    implementer: Agent,
) -> bool {
    let has_detail = |prefix: &str| {
        coding_failure
            .strip_prefix(prefix)
            .is_some_and(|detail| !detail.is_empty())
    };

    has_detail(BRANCH_DRIFT_PREFIX)
        || (checkpoint_drift_phase_admits(phase) && has_detail(CHECKPOINT_DRIFT_PREFIX))
        || (reporter == Agent::Claude
            && current_owner == Agent::Codex
            && dispatch_failure_phase_admits(phase, implementer)
            && has_detail(CODEX_DISPATCH_FAILED_PREFIX))
}

/// Prefix on `coding_failure` that marks a failed `git commit` — a
/// recoverable tooling failure (see [`failure_class`]).
pub const GIT_COMMIT_FAILED_PREFIX: &str = "git_commit_failed:";

/// Prefix on `coding_failure` that marks a failed `git push` — a
/// recoverable tooling failure (see [`failure_class`]).
pub const GIT_PUSH_FAILED_PREFIX: &str = "git_push_failed:";

/// Prefix on `coding_failure` that marks a sandbox or permission denial
/// encountered by the implementer — a recoverable tooling failure (see
/// [`failure_class`]).
pub const SANDBOX_DENIED_PREFIX: &str = "sandbox_denied:";

/// Prefix on `coding_failure` that marks the implementer running out of
/// disk space — a recoverable tooling failure (see [`failure_class`]).
pub const DISK_FULL_PREFIX: &str = "disk_full:";

/// Prefix on `coding_failure` that marks a transient network failure
/// encountered by the implementer — a recoverable tooling failure (see
/// [`failure_class`]).
pub const NETWORK_FAILED_PREFIX: &str = "network_failed:";

/// Prefixes on `coding_failure` that, when followed by a non-empty detail
/// suffix, classify as [`failure_class::FailureClass::Tooling`] — recoverable
/// failures worth retrying rather than aborting the collab session. See
/// [`failure_class::classify`].
///
/// `CODEX_DISPATCH_FAILED_PREFIX` and `CHECKPOINT_DRIFT_PREFIX` are
/// deliberately in both this set and `OFF_TURN_FAILURE_PREFIXES` above: each
/// is both off-turn-admissible and recoverable. Every prefix in that
/// intersection is phase-scoped in [`off_turn_failure_is_admissible`], and
/// that is not a coincidence — a recoverable report installs a new
/// `current_owner`, so an unscoped off-turn clause on a `Tooling` prefix hands
/// the reporter a live turn.
///
/// The two prefix vocabularies overlap but are not identical —
/// `BRANCH_DRIFT_PREFIX` is off-turn-admissible but classifies as
/// `FailureClass::Terminal`, not `Tooling`, which is what lets its clause be
/// unscoped. That contrast is deliberate: branch drift means the work is on
/// the wrong branch, which cannot be fixed in place, while checkpoint drift
/// means the ledger is merely behind the work and is fixed by writing an
/// accurate checkpoint — recoverable, not session-ending.
pub const RECOVERABLE_FAILURE_PREFIXES: &[&str] = &[
    GIT_COMMIT_FAILED_PREFIX,
    GIT_PUSH_FAILED_PREFIX,
    SANDBOX_DENIED_PREFIX,
    DISK_FULL_PREFIX,
    NETWORK_FAILED_PREFIX,
    CODEX_DISPATCH_FAILED_PREFIX,
    CHECKPOINT_DRIFT_PREFIX,
];

#[cfg(test)]
mod dead_session_tests {
    use super::{idle_secs, session_is_dead, COLLAB_DEAD_SESSION_SECS};

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn a_missing_signal_is_dead() {
        assert!(session_is_dead("s", None, NOW));
        assert_eq!(idle_secs(None, NOW), None);
    }

    #[test]
    fn boundary_is_inclusive_at_the_threshold() {
        let at = NOW - COLLAB_DEAD_SESSION_SECS;
        assert!(
            session_is_dead("s", Some(at), NOW),
            "exactly at the threshold is dead"
        );
        assert!(
            !session_is_dead("s", Some(at + 1), NOW),
            "one second under the threshold is live"
        );
        assert!(
            session_is_dead("s", Some(at - 1), NOW),
            "one second over the threshold is dead"
        );
    }

    /// A clock that moved backwards must fail safe toward "live" — the false
    /// positive (ending a live session) is the destructive one (D2).
    #[test]
    fn a_future_timestamp_is_live() {
        assert!(!session_is_dead("s", Some(NOW + 60), NOW));
        assert_eq!(idle_secs(Some(NOW + 60), NOW), Some(-60));
    }

    /// A pathologically corrupt `last_activity` (the smallest representable
    /// `i64`, reachable only via direct DB repair of `collab_checkpoints
    /// .updated_at`, which carries no range CHECK) must not panic
    /// `now - last`'s overflow, and — unlike the two's-complement wraparound
    /// plain subtraction would produce in release builds — must not
    /// misclassify the corruption as live either.
    #[test]
    fn i64_min_last_activity_does_not_panic_and_reads_dead() {
        assert_eq!(idle_secs(Some(i64::MIN), NOW), Some(i64::MAX));
        assert!(session_is_dead("s", Some(i64::MIN), NOW));
    }
}

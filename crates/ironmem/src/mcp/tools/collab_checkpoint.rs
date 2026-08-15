//! The `collab_checkpoint` MCP tool — the durable, first-class write path for
//! v3 batch implementation progress (issue #273), plus the read-only
//! `inspect_divergence` mode that makes the operator-attested backfill an
//! *informed* decision rather than a rubber stamp.
//!
//! This replaces the `add_drawer(logical_key="collab-checkpoint:<id>")`
//! convention. The difference that matters is not the storage medium but the
//! enforcement: a checkpoint written here is what `implementation_done` will
//! demand as proof (Task 7), so a controller can no longer commit and hand off
//! while the recorded progress stays frozen at task 1.
//!
//! The tool **reports** head divergence, it never **refuses** on it. Writing a
//! checkpoint is how an operator *fixes* drift, so a write that failed on drift
//! would make the recovery path unreachable — and would defeat the
//! operator-attested escape hatch (`attested_by=operator` plus
//! `acknowledged_divergence`) that Task 10 builds on. Enforcement belongs at
//! the gate that decides what to do with a checkpoint, not at the write.
//!
//! What it *does* refuse is a caller who is not the current process for the
//! agent it claims to be — the generation lease every other session-scoped
//! collab write takes (see [`handle_collab_checkpoint`]) — and, as of Task 10,
//! an `attested_by=operator` write whose `acknowledged_divergence` the repo
//! contradicts (see [`verify_acknowledged_range`]). Note the second is not a
//! refusal *on drift*: an implementer checkpoint at any head, however stale,
//! still writes. It is a refusal on a false claim about what a human inspected.

use std::process::Command;

use serde_json::{json, Value};

use crate::collab::{AttestationCheck, AttestedBy, CollabCheckpoint};
use crate::error::MemoryError;
use crate::mcp::app::App;

use super::collab_session::{checkpoint_json, scrub_git_environment, HeadCheck};
use super::shared::{optional_bool, require_agent, require_str};

/// Ceiling on how many post-checkpoint commits an inspection will list.
///
/// The incident's batch was 28 commits, so this is not a limit anyone should
/// meet in practice — it is a bound on the response size for the pathological
/// case (a checkpoint frozen since the start of a long-lived branch). When it
/// bites, `commits_truncated` says so rather than the list silently ending: an
/// operator shown a *partial* range and told nothing would attest to more than
/// they saw, which is the rubber stamp this mode exists to prevent.
const MAX_INSPECTED_COMMITS: usize = 200;

/// `git log --format` for the inspection listing: full sha, ASCII unit
/// separator, subject. The separator is `%x1f` rather than a space or a tab
/// because a commit subject may contain either.
const INSPECT_LOG_FORMAT: &str = "--format=%H%x1f%s";

/// Write the session's current checkpoint, or — with `inspect_divergence` —
/// report what an operator attestation would be vouching for, writing nothing.
///
/// `agent` is **required** in both modes, and the generation lease below is why
/// for the write. A checkpoint is a durable, session-scoped write, so a
/// superseded process — one whose successor has already claimed the session via
/// `session_handoff` — must not be able to land its stale view of progress.
/// Nothing downstream can undo that: `updated_at` is server-stamped, so a stale
/// process's content arrives carrying a *fresh* timestamp, and a Task 7 gate
/// asking "is this checkpoint recent?" would be answered by the very
/// anti-backdating stamp that exists to stop exactly this. The check has to be
/// at the write, and it cannot be optional-when-supplied: an authorization
/// check a caller may omit is not a check, it is a suggestion.
///
/// This is not an obstacle to Task 10's operator-attested backfill. An
/// operator backfill *is* a takeover by a non-incumbent process, which is what
/// `session_handoff` plus `handoff_token` already exists to authorize and
/// audit. An `agent`-less operator path would be an unauthenticated bypass of
/// the head-consistency gate itself, since anyone reaching it could write
/// `attested_by=operator` with a fabricated range — and while
/// [`verify_acknowledged_range`] now resolves that range against the repo, it
/// can only do so when the repo is readable, so the authorization check is
/// still the thing standing between an anonymous caller and an attestation.
///
/// Deliberately does NOT call `ensure_no_conflicting_process_session`, which
/// seven sibling handlers do: that guard rebinds this process's metrics
/// attribution scope, and a checkpoint is a progress record rather than a turn
/// — it neither starts nor claims a session, so there is no attribution to
/// move. The lease above, not that guard, is what makes this call authorized.
pub(super) fn handle_collab_checkpoint(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let agent = require_agent(require_str(args, "agent")?)?;

    // Dispatched before the payload parser, because the inspection mode has no
    // checkpoint to parse: it reports on the *stored* one, so `status` and
    // `head_sha` — mandatory for a write — are precisely the values an
    // inspecting operator does not have yet and is calling in order to learn.
    if optional_bool(args, "inspect_divergence", false)? {
        return inspect_divergence(app, args, agent);
    }

    // Unlike its neighbours this handler does NOT pull `session_id` out with
    // `shared::require_str` first. The payload parser reads the same key and
    // refuses a strict superset of what that helper does — absent, non-string,
    // blank, *and* the `none` absent-sentinel — with a message that says which
    // of those went wrong, so a second reading could only ever subtract
    // information or disagree. And disagree it would: the two differ on
    // trimming, so a `" <id> "` transcribed out of a turn template would parse
    // into one session id and be looked up under another. Keeping a single
    // reading makes the session whose liveness is checked the same session the
    // row is keyed by, by construction rather than by a comparison.
    let mut checkpoint = CollabCheckpoint::from_json(args)
        .map_err(|err| MemoryError::Validation(err.to_string()))?;
    let session_id = checkpoint.session_id.clone();
    let session_id = session_id.as_str();

    // The git reads sit OUTSIDE the transaction on purpose. `with_transaction`
    // replays its closure on `SQLITE_BUSY_SNAPSHOT`, and its doc comment
    // enumerates every closure that reaches outside the database precisely
    // because a `Command::output()` there holds the write transaction open
    // across a process spawn — and, sitting between the reads that pin the
    // snapshot and the first write, widens the very window that triggers the
    // retry. Nothing about reading HEAD needs the transaction's snapshot: the
    // repo is not in the database, and `repo_path` is fixed at session start.
    //
    // The *previous* checkpoint is read in the same connection as the session
    // record so the range verification below judges the attestation against the
    // same snapshot the head check was taken on.
    let (record, previous) = app.db.with_connection(|conn| {
        crate::collab::queue::ensure_active(conn, session_id)?;
        let record = crate::collab::queue::load_session_record(conn, session_id)?;
        let previous = crate::collab::queue::load_current_checkpoint(conn, session_id)?;
        Ok((record, previous))
    })?;
    let head_check = HeadCheck::read(&record.repo_path, &checkpoint);
    let outcome = verify_acknowledged_range(
        &record.repo_path,
        &checkpoint,
        previous.as_ref().map(|cp| cp.head_sha.as_str()),
        &head_check,
    )?;
    // Stamped onto the row rather than only returned, so the finding survives
    // into `collab_status`, `collab_resume` and the `session_handoff` block.
    // `from_json` leaves this `None` and `validate` refuses it on an
    // implementer row, so a caller cannot label its own attestation.
    checkpoint.attestation_check = outcome.check;
    if let Some(canonical) = outcome.canonical_range {
        checkpoint.acknowledged_divergence = Some(canonical);
    }
    let attestation_check = outcome.check;
    let checkpoint = checkpoint;

    let (claim, updated_at) = app.db.with_transaction(|tx| {
        // Inside the transaction because a token claim is itself a DB write
        // that must be atomic with the upsert it authorizes — a claim that
        // committed while the checkpoint rolled back would burn a one-time
        // token for nothing. Replay-safe under `SQLITE_BUSY_SNAPSHOT` for the
        // reason `with_transaction`'s exception 1 already documents.
        let claim = super::handoff::ensure_actor_generation_current(
            app,
            tx,
            session_id,
            agent,
            super::handoff::opt_handoff_token(args).as_deref(),
        )?;
        // Re-checked under the write transaction: the read above resolved
        // `repo_path` for the git call, and the session could have ended
        // between the two. Deliberately left unpinned by any test — driving
        // the interleaving from one would take a second connection racing a
        // `collab_end` between two statements of this handler, which is more
        // machinery than the check is worth; it is defense in depth over the
        // read above, not the only thing standing between an ended session and
        // a write.
        crate::collab::queue::ensure_active(tx, session_id)?;
        // Unconditional last-writer-wins, with no `updated_at` guard. The
        // hazard that carries is NOT a stale in-memory checkpoint — this tool
        // takes a fully-formed payload and never does read-modify-write — it is
        // a stale *process*: a superseded agent overwriting its successor's
        // progress, and doing it with a fresh server stamp. That is precisely
        // what the generation lease above refuses, and it is the reason this
        // handler takes one. `upsert_checkpoint` calls `validate()` itself, so
        // nothing is repeated here.
        crate::collab::queue::upsert_checkpoint(tx, &checkpoint)?;
        // Read the stamp back rather than computing a second clock value here,
        // so the response reports what the row actually says. Doubles as a
        // read-back of the row just written: `load_current_checkpoint`
        // re-validates it, so a combination that would poison every later read
        // fails here, inside the transaction that wrote it.
        let updated_at = crate::collab::queue::load_current_checkpoint(tx, session_id)?
            .ok_or_else(|| {
                // Unreachable: the upsert above ran in this transaction. Db
                // rather than Validation because the caller did nothing wrong;
                // it renders as an opaque internal error and is logged in full.
                MemoryError::Db(rusqlite::Error::QueryReturnedNoRows)
            })?
            .updated_at;

        crate::db::schema::Database::wal_log_tx(
            tx,
            wal_operation(&checkpoint),
            &json!({
                "session_id": session_id,
                "agent": agent.as_str(),
                "status": checkpoint.status.as_str(),
                "head_sha": checkpoint.head_sha,
                "attested_by": checkpoint.attested_by.as_str(),
            }),
            // The audit trail records the head check in the same three states
            // the response does: an entry saying `diverged: false` where the
            // check never ran would be a false record of a verification. The
            // range verdict is recorded for the same reason — an attestation
            // written while the repo was unreadable must not be indexed beside
            // one the server actually resolved.
            Some(&json!({
                "completed_task_ids": checkpoint.completed_task_ids,
                "diverged": head_check.diverged(),
                "head_check": head_check.label(),
                "acknowledged_divergence": checkpoint.acknowledged_divergence,
                "attestation_check": attestation_check.map(AttestationCheck::as_str),
            })),
        )?;

        Ok((claim, updated_at))
    })?;
    // Only after the commit — publishing a claim whose transaction may still
    // roll back is the cache poisoning `GenerationClaim` exists to prevent.
    claim.publish(app);

    let mut response = json!({
        "session_id": session_id,
        "agent": agent.as_str(),
        "status": checkpoint.status.as_str(),
        "head_sha": checkpoint.head_sha,
        // Echoed so a caller can see the server's stamp rather than trusting a
        // bare success: it is the anti-backdating field, and the one a resumer
        // uses to tell a fresh checkpoint from a frozen one.
        "updated_at": updated_at,
        "diverged": head_check.diverged(),
        "head_check": head_check.label(),
        "repo_head_sha": head_check.repo_head_sha(),
    });
    if let Some(detail) = head_check.unreadable_detail() {
        // Only present when there is something to explain, and always
        // alongside `head_check: "unreadable"` — the caller is told why the
        // check could not run, not left to infer it from a missing verdict.
        response["head_check_error"] = json!(detail);
    }
    if let Some(check) = attestation_check {
        // Present only for an operator attestation, so an implementer write's
        // response bytes are unchanged and no reader can mistake a routine
        // checkpoint for one carrying a human's signature. The stored
        // `acknowledged_divergence` is echoed beside it because the write path
        // may have canonicalized it (see `AttestationOutcome`), and a caller
        // told only "ok" would not know which form became the audit record.
        response["attestation_check"] = json!(check.as_str());
        response["acknowledged_divergence"] = json!(checkpoint.acknowledged_divergence);
    }
    Ok(response)
}

/// The `wal_log` operation name for this write.
///
/// An operator attestation gets its **own** operation rather than a flag inside
/// the params blob, because the audit question is "show me every time a human
/// vouched for commits the protocol never witnessed" and the answer must be one
/// indexed `WHERE operation = ?` — not a scan that JSON-parses every checkpoint
/// row ever written. This is the one path that can knowingly cover unwitnessed
/// work, so it must be findable without knowing what to look for inside a
/// payload.
fn wal_operation(checkpoint: &CollabCheckpoint) -> &'static str {
    match checkpoint.attested_by {
        AttestedBy::Implementer => "collab_checkpoint",
        AttestedBy::Operator => "collab_checkpoint_operator_attested",
    }
}

/// What resolving an `acknowledged_divergence` against the repo established,
/// plus the form of the range that should be stored.
///
/// The verdict itself is [`AttestationCheck`], which lives in
/// `collab::checkpoint` beside the column it is persisted in — this task's
/// review found that a verdict reaching only the write response and the
/// `wal_log` never reaches `session_handoff`, `collab_status` or
/// `collab_resume`, which is precisely where a fabricated range gets rendered
/// to a human as `attested_by: operator`.
struct AttestationOutcome {
    /// `None` for an implementer checkpoint: there is no range to resolve, so
    /// there is no finding. Deliberately not a `NotApplicable` variant — see
    /// [`AttestationCheck`].
    check: Option<AttestationCheck>,
    /// The range in **resolved** form (`<full oid>..<full oid>`), present only
    /// when both endpoints resolved.
    ///
    /// `acknowledged_divergence` is the durable audit record of what a human
    /// vouched for, and git accepts revision *expressions* — `HEAD~1..HEAD`,
    /// `main..feature` — which resolve to different commits next week. An audit
    /// record that means something different later is barely an audit record,
    /// so the resolved form is what gets stored. When the endpoints could not
    /// be resolved at all the operator's own expression is kept verbatim, and
    /// the `unverified_repo_unreadable` verdict beside it says why it is not
    /// canonical.
    canonical_range: Option<String>,
}

/// Resolve an operator's `acknowledged_divergence` against the repository.
///
/// # Why this is at the WRITE and not at the gate
///
/// [`CollabCheckpoint::validate`] enforces *requiredness*, not shape: it checks
/// the range is non-blank, never that it is real, and it has no repo to resolve
/// one against. `require_checkpoint_proof` (Task 7) cannot take the job either
/// — it is deliberately pure with respect to the filesystem so it cannot fail a
/// turn on a transient repo problem, and adding a git read there would, by its
/// own binding note, force it to start consulting `attested_by`.
///
/// So it belongs here, at the moment the range is *asserted*. Two properties
/// follow from that placement and neither is available later:
///
/// 1. **A false attestation never becomes a stored row.** Verifying at
///    `implementation_done` would leave the fabricated range in
///    `collab_checkpoints` in the meantime, where `session_handoff`,
///    `collab_status` and `collab_resume` all read it and render
///    `attested_by: operator` to a human as though it meant something.
/// 2. **The refusal reaches the caller who can fix it.** The operator filing
///    the attestation is present at this call; the agent that later sends
///    `implementation_done` is usually a different process that did not choose
///    the range and cannot correct it.
///
/// There is nothing to verify at *inspection* time — inspection takes no range,
/// it emits the one a subsequent write should carry — so "both" is not an
/// option; the pairing is inspection emits, write verifies, and
/// `inspect_then_attest_ends_the_divergence_and_is_logged_distinctly` pins that
/// the two halves compose.
///
/// # What is checked
///
/// Syntax first, needing no repo, so the unreadable-repo escape valve below
/// cannot smuggle a value that is not a range at all. Then, only when live HEAD
/// was readable:
///
/// - both endpoints resolve to real commits;
/// - the range **ends at the checkpoint's own `head_sha`**. This is the
///   property `collab_resume` already relies on to conclude that live drift is
///   by construction past whatever the operator saw (`docs/COLLAB.md`: "an
///   attestation names a *closed* range ending at the checkpoint's own
///   `head_sha`"). Without it, one inspection could be pasted onto any later
///   checkpoint;
/// - the range covers at least one commit — `X..X` vouches for nothing, the
///   same claim `Some("")` makes, dressed to pass both the non-blank check and
///   the endpoints-exist check;
/// - `from` is an ancestor of `to`, so the range is a real span rather than two
///   unrelated commits joined by two dots;
/// - and it **spans the divergence**: `from` must be an ancestor-or-equal of
///   the *previous* checkpoint's head, so the operator cannot vouch for the
///   tail of the gap and leave the commits nearest the stale checkpoint
///   unaccounted for.
///
/// # The two deliberate escape valves
///
/// A legitimate attestation must stay writable in a repo state where the
/// endpoints are momentarily unresolvable, so neither of these refuses:
///
/// - **Live HEAD unreadable** (git missing, repo unmounted, not a repo): every
///   repo-backed check is skipped and the write is labelled
///   `unverified_repo_unreadable`. The alternative — refusing — would make the
///   recovery path unreachable in exactly the environment that most needs it.
/// - **The gap is not a gap.** The span check needs the checkpoint being
///   replaced to sit *behind* this one, and there are **two** ways for that to
///   fail — the second far more reachable than the first, and the one an
///   attacker uses:
///
///   1. the previous checkpoint's head no longer resolves (history rewritten
///      out from under it);
///   2. it resolves but is **not an ancestor** of this checkpoint's head — most
///      simply, the new checkpoint is on an orphan or unrelated branch, so
///      `<previous>..<new>` bounds no linear run of commits to demand coverage
///      of.
///
///   Either way the span check alone is skipped and the write is labelled
///   `verified_without_span`. Both are branch-drift shapes rather than
///   checkpoint gaps, and an operator repairing after a history rewrite is the
///   one person who can, so refusing would strand the repair. Case 2 means an
///   operator can file a well-formed attestation over a *narrow* range on an
///   unrelated branch and have the endpoint rules all pass — which is exactly
///   why the label has to reach every reader (see
///   [`CollabCheckpoint::attestation_verdict`]) rather than stopping at the
///   write response.
///
/// Both are labelled rather than silent, and both reach the `wal_log` row, so
/// an audit can tell an attestation the server resolved from one it merely
/// recorded.
fn verify_acknowledged_range(
    repo_path: &str,
    checkpoint: &CollabCheckpoint,
    previous_head_sha: Option<&str>,
    head_check: &HeadCheck,
) -> Result<AttestationOutcome, MemoryError> {
    let range = match (
        checkpoint.attested_by,
        checkpoint.acknowledged_divergence.as_deref(),
    ) {
        (AttestedBy::Operator, Some(range)) => range,
        // Every other combination has already been refused by
        // `CollabCheckpoint::validate` (an operator without a range, an
        // implementer with one), so this arm is only ever the ordinary
        // implementer write.
        _ => {
            return Ok(AttestationOutcome {
                check: None,
                canonical_range: None,
            })
        }
    };

    let (from, to) = parse_range(range)?;
    // `head_sha` is caller-supplied and `validate` only requires it non-blank,
    // so it gets the same option-injection guard the range endpoints get: it is
    // resolved below, and `git rev-parse <rev>^{commit}` has no `--` to hide a
    // leading dash behind.
    if checkpoint.head_sha.starts_with('-') {
        return Err(range_refusal(
            range,
            format!(
                "this checkpoint's head_sha {:?} would be read by git as an option rather than a \
                 revision, so the range cannot be resolved against it",
                checkpoint.head_sha
            ),
        ));
    }

    // The repo-backed half. `HeadCheck` already answered "can this repo be read
    // at all?" from the same snapshot, so this reuses that answer rather than
    // spawning git again to ask it a second time and possibly get a different
    // one.
    let HeadCheck::Checked { .. } = head_check else {
        return Ok(AttestationOutcome {
            check: Some(AttestationCheck::UnverifiedRepoUnreadable),
            // Nothing resolved, so the operator's expression is stored as
            // typed rather than silently normalized into something the server
            // never actually looked up.
            canonical_range: None,
        });
    };

    let from_oid = resolve_commit(repo_path, from).map_err(|detail| {
        range_refusal(
            range,
            format!("{from} does not name a commit in this repository ({detail})"),
        )
    })?;
    let to_oid = resolve_commit(repo_path, to).map_err(|detail| {
        range_refusal(
            range,
            format!("{to} does not name a commit in this repository ({detail})"),
        )
    })?;
    let head_oid = resolve_commit(repo_path, &checkpoint.head_sha).map_err(|detail| {
        range_refusal(
            range,
            format!(
                "this checkpoint's own head_sha {} does not name a commit in this repository \
                 ({detail}), so there is no work at it for an operator to vouch for",
                checkpoint.head_sha
            ),
        )
    })?;

    if to_oid != head_oid {
        return Err(range_refusal(
            range,
            format!(
                "it must end at this checkpoint's own head_sha {} (an attestation names a closed \
                 range ending where the checkpoint is filed, which is what makes any later drift \
                 provably past it), but it ends at {to}",
                checkpoint.head_sha
            ),
        ));
    }
    if from_oid == to_oid {
        return Err(range_refusal(
            range,
            "it covers no commits at all — an empty range vouches for nothing, which is the same \
             claim an absent range makes"
                .to_string(),
        ));
    }
    if !is_ancestor(repo_path, from, to)? {
        return Err(range_refusal(
            range,
            format!("{from} is not an ancestor of {to}, so the two do not bound a range of work"),
        ));
    }

    // The span check. Skipped when there is no previous checkpoint (nothing was
    // frozen, so there is no gap to under-cover) or when its head no longer
    // resolves (see the escape valves in this function's doc comment).
    let canonical_range = Some(format!("{from_oid}..{to_oid}"));
    let Some(previous_head) = previous_head_sha else {
        return Ok(AttestationOutcome {
            check: Some(AttestationCheck::Verified),
            canonical_range,
        });
    };
    if resolve_commit(repo_path, previous_head).is_err() {
        return Ok(AttestationOutcome {
            check: Some(AttestationCheck::VerifiedWithoutSpan),
            canonical_range,
        });
    }
    // Only meaningful when the previous head is actually behind this one. If it
    // is not, the divergence is branch drift rather than a run of commits after
    // the checkpoint, and there is no linear gap for the range to cover.
    if !is_ancestor(repo_path, previous_head, to)? {
        return Ok(AttestationOutcome {
            check: Some(AttestationCheck::VerifiedWithoutSpan),
            canonical_range,
        });
    }
    if !is_ancestor(repo_path, from, previous_head)? {
        return Err(range_refusal(
            range,
            format!(
                "it does not span the divergence: the checkpoint it replaces was filed at \
                 {previous_head}, and {from} is not an ancestor of it, so the commits between \
                 {previous_head} and {from} would be covered by no attestation at all"
            ),
        ));
    }
    Ok(AttestationOutcome {
        check: Some(AttestationCheck::Verified),
        canonical_range,
    })
}

/// One spelling of every `acknowledged_divergence` refusal, so each names the
/// field (the caller has to know which argument to fix) and the range it read.
fn range_refusal(range: &str, detail: String) -> MemoryError {
    MemoryError::Validation(format!(
        "acknowledged_divergence {range:?} is not a range this repository backs: {detail}"
    ))
}

/// Split `<from>..<to>`, rejecting everything that is not exactly that.
///
/// Repo-free on purpose: this runs even when git cannot be read, so the
/// unreadable-repo escape valve in [`verify_acknowledged_range`] admits an
/// unverified range but never an unparseable one.
///
/// `...` (symmetric difference) is refused rather than accepted as a sloppy
/// `..`: it names a genuinely different set of commits, and an operator who
/// typed it did not inspect what the server would compute. A leading `-` on
/// either endpoint is refused because git would read it as an option — the same
/// argument-injection guard `validate_global_review_head_advance` gets from its
/// `--` separator, needed here because `git rev-parse <rev>^{commit}` has no
/// place to put one.
fn parse_range(range: &str) -> Result<(&str, &str), MemoryError> {
    let trimmed = range.trim();
    if trimmed.contains("...") {
        return Err(range_refusal(
            range,
            "`...` is git's symmetric-difference operator and names a different set of commits \
             than the `<from>..<to>` an attestation vouches for"
                .to_string(),
        ));
    }
    let Some((from, to)) = trimmed.split_once("..") else {
        return Err(range_refusal(
            range,
            "it is not of the form <from_sha>..<to_sha>".to_string(),
        ));
    };
    let (from, to) = (from.trim(), to.trim());
    if from.is_empty() || to.is_empty() || to.contains("..") {
        return Err(range_refusal(
            range,
            "it is not of the form <from_sha>..<to_sha> with exactly one separator and a \
             non-empty commit on each side"
                .to_string(),
        ));
    }
    if from.starts_with('-') || to.starts_with('-') {
        return Err(range_refusal(
            range,
            "an endpoint may not begin with `-`, which git would read as an option rather than a \
             revision"
                .to_string(),
        ));
    }
    Ok((from, to))
}

/// Run git in `repo_path` with the inherited `GIT_*` environment scrubbed.
///
/// Every git shell-out in this module goes through here, so the scrub cannot be
/// forgotten at one call site: an inherited `GIT_DIR` would resolve an
/// attestation's endpoints against a *different* repository, and the resulting
/// `verified` label would be the unverified-claim-presented-as-verified failure
/// this issue exists to end, arriving through the environment.
fn git(repo_path: &str, args: &[&str]) -> Result<std::process::Output, MemoryError> {
    let mut command = Command::new("git");
    scrub_git_environment(&mut command);
    command
        .args(["-C", repo_path])
        .args(args)
        .output()
        .map_err(|err| {
            MemoryError::Validation(format!("unable to execute git in {repo_path}: {err}"))
        })
}

/// Resolve a revision to the object id of the commit it names.
///
/// `Err` carries git's own stderr and means "this revision does not name a
/// commit here" — the caller has already established (via [`HeadCheck`]) that
/// the repository itself is readable, so a failure at this point is about the
/// revision rather than the environment.
///
/// The `^{commit}` peel is load-bearing: without it a tag or a tree would
/// resolve, and a range whose endpoints are not commits bounds nothing.
fn resolve_commit(repo_path: &str, rev: &str) -> Result<String, String> {
    let peeled = format!("{rev}^{{commit}}");
    let output = git(repo_path, &["rev-parse", "--verify", "--quiet", &peeled])
        .map_err(|err| err.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "no such revision".to_string()
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Whether `ancestor` is reachable from `descendant` (reflexively true when
/// they are the same commit).
///
/// `Err` is reserved for git failing in a way that is neither yes nor no —
/// exit 1 is the honest "no", and anything else is an operational failure the
/// caller must not silently read as either answer.
fn is_ancestor(repo_path: &str, ancestor: &str, descendant: &str) -> Result<bool, MemoryError> {
    // `--` stops an endpoint that begins with `-` from being read as an option.
    // `parse_range` already refuses those, so this is the second of two
    // independent guards rather than the only one.
    let output = git(
        repo_path,
        &["merge-base", "--is-ancestor", "--", ancestor, descendant],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(MemoryError::Validation(format!(
            "git could not decide whether {ancestor} is an ancestor of {descendant} in \
             {repo_path}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

/// Report what an operator attestation would be vouching for. Writes nothing.
///
/// # Why this exists
///
/// D1: the server never synthesizes a checkpoint from post-checkpoint commits
/// on its own initiative. It cannot know which *tasks* those commits completed,
/// and inferring it from commit messages would manufacture exactly the false
/// provenance issue #273 exists to prevent — an auto-backfill would replace a
/// stale-but-honest report with a fresh-and-fabricated one. So the operator
/// inspects, then explicitly attests.
///
/// This half is what makes that confirmation *informed*. An override that does
/// not show the operator the commits it covers is a rubber stamp, which is
/// worse than no override at all because it launders a fabrication through a
/// human.
///
/// # Why it takes no generation lease
///
/// `agent` is required — every mode of this tool names the identity it acts as
/// — but this mode deliberately does not call
/// `handoff::ensure_actor_generation_current`. Two reasons, both about the
/// order of the real recovery flow:
///
/// - With a `handoff_token` the lease is a **DB write** (it burns a one-time
///   token), which a read-only mode must not perform. Rather than silently drop
///   a supplied token — leaving a caller believing it had claimed the session —
///   the token is refused outright below.
/// - Without one, the lease refuses a superseded process. But inspection is
///   what an operator does *before* taking the session over: demanding the
///   incumbent's lease first would force them to seize the session in order to
///   see what they would be attesting to, which is backwards. And it would
///   withhold, from the operator, a strictly *smaller* view than `collab_status`
///   already hands to any caller with no `agent` at all.
fn inspect_divergence(
    app: &App,
    args: &Value,
    agent: crate::collab::Agent,
) -> Result<Value, MemoryError> {
    if super::handoff::opt_handoff_token(args).is_some() {
        return Err(MemoryError::Validation(
            "inspect_divergence is read-only and cannot claim a handoff_token: claiming one is a \
             write that burns the token. Inspect without it, then present the token on the \
             collab_checkpoint call that files the attestation."
                .to_string(),
        ));
    }
    // The same reading the write path performs, through the checkpoint
    // parser's own helper rather than `shared::require_str`: that helper
    // neither trims nor rejects the `none` absent-sentinel, so a `" <id> "`
    // transcribed out of a turn template would inspect one session and be
    // looked up under another.
    let session_id = crate::collab::checkpoint::parse_session_id(args)
        .map_err(|err| MemoryError::Validation(err.to_string()))?;
    let session_id = session_id.as_str();

    let (record, checkpoint) = app.db.with_connection(|conn| {
        crate::collab::queue::ensure_active(conn, session_id)?;
        let record = crate::collab::queue::load_session_record(conn, session_id)?;
        let checkpoint = crate::collab::queue::load_current_checkpoint(conn, session_id)?;
        Ok((record, checkpoint))
    })?;

    let mut response = json!({
        "session_id": session_id,
        "agent": agent.as_str(),
        "inspect_divergence": true,
        // Stated in the response rather than only in the docs, because the one
        // thing a caller must not have to infer about an override's inspection
        // step is whether calling it changed anything.
        "read_only": true,
    });

    let Some(checkpoint) = checkpoint else {
        // A distinct answer from every other: a session that never checkpointed
        // has no progress claim to reconcile, so there is nothing an operator
        // could attest *over*. Rendering it as "no divergence" would tell them
        // a comparison came back clean when none was possible.
        response["checkpoint"] = checkpoint_json(None);
        response["commit_range_status"] = json!("no_checkpoint");
        response["attestable"] = json!(false);
        response["commit_range"] = Value::Null;
        response["commits"] = Value::Null;
        return Ok(response);
    };

    let head_check = HeadCheck::read(&record.repo_path, &checkpoint);
    // The same rendering `collab_status` and `collab_resume` emit, so the
    // operator about to attest and the successor who reads the row afterwards
    // are shown one story rather than two.
    response["checkpoint"] = checkpoint_json(Some((&checkpoint, &head_check)));

    let range = inspect_commit_range(&record.repo_path, &checkpoint, &head_check);
    response["commit_range_status"] = json!(range.status);
    response["attestable"] = json!(range.commits.is_some());
    response["commit_range"] = match &range.range {
        Some(spec) => json!(spec),
        None => Value::Null,
    };
    response["commits"] = match &range.commits {
        Some(commits) => json!(commits),
        None => Value::Null,
    };
    if let Some(detail) = range.error {
        response["commit_range_error"] = json!(detail);
    }
    if range.commits.is_some() {
        response["commits_truncated"] = json!(range.truncated);
        // The exact call that would attest to what was just shown. The gate's
        // refusals carry a machine-followable remedy for the same reason: an
        // operator who has to reconstruct the arguments by hand is an operator
        // who will get one of them wrong, and `head_sha`/`acknowledged_divergence`
        // are precisely the two the write path cross-checks.
        response["attestation"] = json!(format!(
            "collab_checkpoint(session_id={session_id}, agent=<you>, \
             status=<started|completed|blocked|batch_complete>, head_sha={}, \
             completed_task_ids=<cumulative ids>, attested_by=operator, \
             acknowledged_divergence={})",
            range.head_sha.as_deref().unwrap_or("<live HEAD>"),
            range.range.as_deref().unwrap_or("<from>..<to>"),
        ));
    }
    Ok(response)
}

/// The post-checkpoint commit listing, or the reason there isn't one.
struct CommitRange {
    /// Which of the six answers this is. A single machine-readable field so a
    /// caller never has to infer the state from which optional keys are
    /// present.
    status: &'static str,
    /// `<checkpoint head>..<live HEAD>`, present only when it is a real,
    /// attestable range.
    range: Option<String>,
    /// `Some` **only** when the range is attestable — this is the field
    /// `attestable` is derived from, so the two cannot disagree.
    commits: Option<Vec<Value>>,
    truncated: bool,
    head_sha: Option<String>,
    error: Option<String>,
}

impl CommitRange {
    fn refused(status: &'static str, error: String) -> Self {
        Self {
            status,
            range: None,
            commits: None,
            truncated: false,
            head_sha: None,
            error: Some(error),
        }
    }
}

/// List the commits that landed after the checkpoint, or say why that is not
/// the question.
///
/// Six outcomes, and the separation between them is the whole point:
///
/// - `not_checked` — live HEAD could not be read, so drift is **unknown**. Never
///   rendered as "no divergence": an unreadable repo is precisely where a
///   checkpoint is most likely to be stale.
/// - `no_divergence` — checked, and the checkpoint describes live HEAD. Nothing
///   to attest.
/// - `checkpoint_head_unreachable` — the checkpoint's `head_sha` is not
///   reachable from live HEAD (rewritten history, a different branch, or a sha
///   that no longer exists). This is **branch drift, not a checkpoint gap**, and
///   it must not be offered as an attestable range: `git log <cp>..<head>` would
///   cheerfully list every commit on the other branch, none of which is
///   post-checkpoint work on the branch the checkpoint was filed on.
///
///   Note the ancestry is asked *directly* (`merge-base --is-ancestor`) rather
///   than inferred from `git log` failing. `git log` only fails when a revision
///   does not resolve at all; for the orphan-branch and rewritten-history cases
///   — where both commits exist but are unrelated — it **succeeds**, and reading
///   its success as "here is the gap" is exactly the misreport this arm exists
///   to prevent.
/// - `empty_range` — ancestry held but nothing was listed. **Reachable**, and
///   the route is one this tool's own schema warns about: the divergence check
///   is string equality, so an *abbreviated* `head_sha` is unequal to live HEAD
///   (hence `diverged: true`) while resolving to the very same commit — an
///   ancestor of itself, bounding nothing. `attestable: false` makes the
///   behavior safe either way, but a client switching on
///   `commit_range_status` must not meet an undocumented value, so this is the
///   sixth answer rather than an internal fallback.
/// - `listed` — a real run of commits after the checkpoint, on the same
///   history. The only attestable answer.
fn inspect_commit_range(
    repo_path: &str,
    checkpoint: &CollabCheckpoint,
    head_check: &HeadCheck,
) -> CommitRange {
    let HeadCheck::Checked {
        repo_head_sha,
        divergence,
    } = head_check
    else {
        return CommitRange::refused(
            "not_checked",
            "live HEAD could not be read, so the commits after the checkpoint could not be \
             listed and whether the checkpoint has drifted at all is unknown"
                .to_string(),
        );
    };
    if divergence.is_none() {
        return CommitRange {
            status: "no_divergence",
            range: None,
            commits: None,
            truncated: false,
            head_sha: Some(repo_head_sha.clone()),
            error: None,
        };
    }

    let from = checkpoint.head_sha.as_str();
    if from.starts_with('-') {
        return CommitRange::refused(
            "checkpoint_head_unreachable",
            format!(
                "the checkpoint's head_sha {from:?} would be read by git as an option rather \
                 than a revision, so this is branch drift or a corrupt checkpoint rather than a \
                 gap of commits after it"
            ),
        );
    }
    match is_ancestor(repo_path, from, repo_head_sha) {
        Ok(true) => {}
        Ok(false) => {
            return CommitRange::refused(
                "checkpoint_head_unreachable",
                format!(
                    "the checkpoint's head_sha {from} is not an ancestor of live HEAD \
                     {repo_head_sha}: this is branch drift (history rewritten, or the worktree is \
                     on a different branch), not a gap of commits after the checkpoint, so there \
                     is no range here for an operator to attest to"
                ),
            );
        }
        Err(err) => {
            return CommitRange::refused(
                "checkpoint_head_unreachable",
                format!(
                    "the checkpoint's head_sha {from} could not be placed in this repository's \
                     history, so this is branch drift or a fabricated sha rather than a gap of \
                     commits after the checkpoint ({err})"
                ),
            );
        }
    }

    let spec = format!("{from}..{repo_head_sha}");
    let output = match git(
        repo_path,
        &[
            "log",
            "--no-color",
            INSPECT_LOG_FORMAT,
            &format!("--max-count={}", MAX_INSPECTED_COMMITS + 1),
            &spec,
            "--",
        ],
    ) {
        Ok(output) => output,
        Err(err) => {
            return CommitRange::refused(
                "not_checked",
                format!("the commits after the checkpoint could not be listed: {err}"),
            )
        }
    };
    if !output.status.success() {
        return CommitRange::refused(
            "checkpoint_head_unreachable",
            format!(
                "git could not list {spec}, so this is branch drift or a fabricated sha rather \
                 than a gap of commits after the checkpoint ({})",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut commits: Vec<Value> = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            // `splitn(2)` rather than `split`: a subject may itself contain the
            // separator, and losing its tail would misdescribe the commit an
            // operator is being asked to vouch for.
            let (sha, subject) = line.split_once('\u{1f}').unwrap_or((line, ""));
            json!({ "sha": sha, "subject": subject })
        })
        .collect();
    if commits.is_empty() {
        return CommitRange::refused(
            "empty_range",
            format!(
                "{spec} lists no commits even though the checkpoint's head is an ancestor of \
                 live HEAD — there is nothing here to attest to"
            ),
        );
    }
    let truncated = commits.len() > MAX_INSPECTED_COMMITS;
    commits.truncate(MAX_INSPECTED_COMMITS);

    CommitRange {
        status: "listed",
        range: Some(spec),
        commits: Some(commits),
        truncated,
        head_sha: Some(repo_head_sha.clone()),
        error: None,
    }
}

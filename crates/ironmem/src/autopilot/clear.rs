//! The operator's way out of a hold Autopilot cannot resolve by itself.
//!
//! Restart recovery holds a supervised CLI (Codex or Muse) invocation whose completion it cannot
//! prove — a controller killed by Ctrl-C, a root that exited before the first
//! `ps` sample, an observation error mid-run. That hold is correct: nothing a
//! controller can read distinguishes "finished" from "still running somewhere
//! it cannot see". A human can, and this is how they say so.
//!
//! It is not a force. It refuses while any fresh evidence could be the
//! issue's work, so it can only clear what no process visibly holds. It
//! deletes the dispatch-state drawer and nothing else: lineage, attempt
//! counts and the unpriced reservation stand, and the invocation's artifacts
//! stay on disk for inspection. The next dispatch starts a fresh session.

use std::path::Path;

use serde::Serialize;

use super::dispatch::InvocationArtifacts;
use super::dispatch_state::{self, DispatchState};
use super::implementer::Implementer;
use super::registry::{HarnessSnapshots, Liveness, LivenessSnapshot};
use super::{lineage, remediate, AttemptOutcome, IssueRef};
use crate::db::schema::Database;
use crate::error::MemoryError;

/// What was cleared, so the operator knows where the evidence still lives.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClearedDispatch {
    pub issue: String,
    pub implementer: Implementer,
    pub state: String,
    /// The session the clear discarded: the next dispatch opens a fresh
    /// one rather than resuming this.
    pub session_uuid: String,
    /// How many turns the discarded session had accumulated.
    pub turn_n: u32,
    /// Serialized as `codex_artifacts` so `clear-dispatch --json` output
    /// keeps its key for scripts written before Muse existed.
    #[serde(rename = "codex_artifacts")]
    pub invocation_artifacts: Option<InvocationArtifacts>,
}

/// A dispatch-state drawer this binary could not read, released: what it
/// held, so the evidence is not lost with it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClearedUnreadable {
    pub issue: String,
    /// Why it could not be read.
    pub reason: String,
    /// Its raw content, secrets scrubbed and bounded to
    /// [`RAW_CONTENT_CHARS`] characters.
    pub raw_content: String,
    /// Whether `raw_content` was cut.
    pub raw_content_truncated: bool,
}

/// How many characters of an unreadable drawer's content are reported.
pub const RAW_CONTENT_CHARS: usize = 4000;

/// What `clear-dispatch` released. Untagged, so a readable state's JSON is
/// what it always was.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Cleared {
    Dispatch(ClearedDispatch),
    Unreadable(ClearedUnreadable),
}

/// `observe` is called after the state is read, so the evidence is never
/// older than the state it judges, and the delete happens only if the state
/// is still exactly the one judged.
///
/// A drawer this binary cannot read (corrupt, refused by
/// [`DispatchState::validate`], or written by a newer binary for a harness it
/// does not know) is judged without knowing its harness: see
/// [`unreadable_refusal`].
pub fn clear_dispatch(
    db: &Database,
    issue: &IssueRef,
    observe: impl FnOnce() -> HarnessSnapshots,
) -> Result<Cleared, MemoryError> {
    clear_dispatch_with(db, issue, observe, &super::process_tree::user_pids)
}

/// [`clear_dispatch`], with the PIDs an unreadable state's worktree scan
/// reads injected ([`super::process_tree::ScanPids`]).
pub(crate) fn clear_dispatch_with(
    db: &Database,
    issue: &IssueRef,
    observe: impl FnOnce() -> HarnessSnapshots,
    pids: super::process_tree::ScanPids<'_>,
) -> Result<Cleared, MemoryError> {
    let refuse = |reason: String| {
        MemoryError::Validation(format!("refusing to clear {}: {reason}", issue.canonical()))
    };
    let Some(raw) = dispatch_state::raw_dispatch_state(db, issue)? else {
        return Err(MemoryError::NotFound(format!(
            "{} has no dispatch state",
            issue.canonical()
        )));
    };
    let parsed = dispatch_state::parse_dispatch_state(&raw);
    // A controller holding the issue's lock may have published this state
    // and not yet spawned (or registered) its implementer, when no evidence
    // shows it yet. Held through the delete, so none starts meanwhile.
    let _lock = match controller_lock(issue, &parsed, &raw) {
        Ok(lock) => lock,
        Err(reason) => return Err(refuse(reason)),
    };
    let state = match parsed {
        dispatch_state::Parsed::Readable(state) => *state,
        dispatch_state::Parsed::Unreadable(why) => {
            if let Some(reason) = unreadable_refusal(&observe(), issue, &raw, pids) {
                return Err(refuse(format!(
                    "its dispatch state cannot be read ({why}), and {reason}"
                )));
            }
            if !dispatch_state::clear_raw_dispatch_state_if_unchanged(db, issue, &raw)? {
                return Err(refuse(
                    "its dispatch state changed while it was being checked; a dispatch may \
                     have started — rerun once it settles"
                        .into(),
                ));
            }
            let reason = super::scrub::scrub_and_bound(&why, RAW_CONTENT_CHARS).text;
            let content = super::scrub::scrub_and_bound(&raw, RAW_CONTENT_CHARS);
            return Ok(Cleared::Unreadable(ClearedUnreadable {
                issue: issue.canonical(),
                reason,
                raw_content: content.text,
                raw_content_truncated: content.truncated,
            }));
        }
    };
    // A Claude claim still within its lease may be a run between its
    // claim and its launch, showing no process yet: not cleared,
    // lock or no lock. A supervised CLI's invocation is itself the
    // evidence below, so its lease does not refuse.
    if !state.implementer.recovers_invocations() {
        if let Some(claim) = dispatch_state::get_claim(db, issue)? {
            if claim.leased(chrono::Utc::now()) {
                return Err(refuse(format!(
                    "its claim is still within its {}s lease (claimed {}), so its run \
                     may not have launched yet; wait up to {}s, or until that run ends, and \
                     clear again",
                    dispatch_state::CLAIM_LEASE.as_secs(),
                    claim.age_phrase(chrono::Utc::now()).unwrap_or_default(),
                    dispatch_state::CLAIM_LEASE.as_secs()
                )));
            }
        }
    }
    if let Some(reason) = refusal(&observe(), &state, attempt_recorded(db, &state)?) {
        return Err(refuse(reason));
    }
    // Byte for byte, owner token and lease included: a run that adopted the
    // state since it was read rewrites it identically but for its owner,
    // and that state is the run's, not this clear's.
    if !dispatch_state::clear_raw_dispatch_state_if_unchanged(db, issue, &raw)? {
        return Err(refuse(
            "its dispatch state changed while it was being checked; a dispatch may have \
             started — rerun once it settles"
                .into(),
        ));
    }
    // Its run is gone with its state: any spend reservation it died
    // holding goes back to the day (best effort; a wrong date is a
    // no-op, and the next run releases its own day's).
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    if let Err(e) = super::budget::release_issue_spend_reservations(db, &today, &issue.canonical())
    {
        tracing::warn!(
            error = %e,
            issue = %issue.canonical(),
            "autopilot could not release a cleared run's spend reservation"
        );
    }
    Ok(Cleared::Dispatch(ClearedDispatch {
        issue: issue.canonical(),
        implementer: state.implementer,
        state: state.state,
        session_uuid: state.session_uuid.clone(),
        turn_n: state.turn_n,
        invocation_artifacts: state.invocation_artifacts,
    }))
}

/// The issue's lock ([`super::issue_lock`]), or why clearing (or releasing)
/// must not go ahead: a controller holds it, or it cannot be looked for.
///
/// Found through the git common directory the claim names, which outlives
/// the worktree; for an older record that names none, through the worktree
/// it names. A repository that no longer exists, or a record naming
/// neither, has no controller that could be launching into it, and takes no
/// lock.
fn controller_lock(
    issue: &IssueRef,
    parsed: &dispatch_state::Parsed,
    raw: &str,
) -> Result<Option<super::issue_lock::IssueLock>, String> {
    let loose: Option<serde_json::Value> = serde_json::from_str(raw).ok();
    let field = |key: &str| {
        loose
            .as_ref()
            .and_then(|v| v.get(key)?.as_str().map(str::to_owned))
    };
    let path = match field("common_dir") {
        Some(common) => {
            let common = std::path::PathBuf::from(common);
            match existing_worktree(&common)? {
                Some(common) => {
                    super::local_fs::require_local(&common, "the git common directory")
                        .map_err(|e| e.to_string())?;
                    super::issue_lock::lock_path_in(&common, issue)
                }
                None => return Ok(None),
            }
        }
        None => {
            let worktree = match parsed {
                dispatch_state::Parsed::Readable(state) => Some(state.worktree_path.clone()),
                dispatch_state::Parsed::Unreadable(_) => field("worktree_path"),
            };
            let Some(worktree) = worktree else {
                return Ok(None);
            };
            let Some(worktree) = existing_worktree(Path::new(&worktree))? else {
                return Ok(None);
            };
            let path =
                super::issue_lock::lock_path_if_repository(&worktree, issue).map_err(|e| {
                    format!("whether a controller is running this issue cannot be checked: {e}")
                })?;
            let Some(path) = path else {
                return Ok(None);
            };
            path
        }
    };
    match super::issue_lock::try_lock_path(&path).map_err(|e| e.to_string())? {
        super::issue_lock::IssueLockAttempt::Acquired(lock) => Ok(Some(lock)),
        super::issue_lock::IssueLockAttempt::Held { path } => Err(format!(
            "a controller is running this issue (it holds {}); rerun once it finishes",
            path.display()
        )),
    }
}

/// The worktree a state names, when it is a directory to check; `None` when
/// nothing can be in it (gone, or not a directory). One that cannot be read
/// is no proof it is empty, so it is `Err`, with why.
fn existing_worktree(path: &Path) -> Result<Option<std::path::PathBuf>, String> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(Some(path.to_path_buf())),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "its worktree {} cannot be read ({e}), so its processes cannot be checked",
            path.display()
        )),
    }
}

/// Why an issue whose dispatch state cannot be read may still be running,
/// judged without knowing its harness. Anything that could be its work
/// refuses: an Autopilot marker for the issue on any process (whatever runs
/// it), an ambiguous Codex or Muse command, a process table that cannot be
/// read, or its Claude IC, by the name every Claude dispatch for the issue
/// takes ([`super::dispatch::ic_name`]), either listed in Claude's registry
/// or named with `--name` on a live process. The process table settles a
/// Claude IC on its own, so an unreadable Claude registry (no `claude`
/// installed, say) does not keep the drawer for ever.
///
/// What the drawer still says is used too, read loosely: with its
/// invocation artifacts (`codex_artifacts`), the recorded tree (root,
/// group, descendants, identity file) as a readable state's check reads it;
/// with its `worktree_path`, every same-user process born since the
/// invocation started whose working directory is in that worktree (the
/// supervisor's own worktree check). Any marker-shaped path for the issue
/// is found by substring, so a space in it hides nothing.
fn unreadable_refusal(
    snapshots: &HarnessSnapshots,
    issue: &IssueRef,
    raw: &str,
    pids: super::process_tree::ScanPids<'_>,
) -> Option<String> {
    let loose: Option<serde_json::Value> = serde_json::from_str(raw).ok();
    let field = |key: &str| loose.as_ref().and_then(|v| v.get(key)).cloned();
    // A field that is present but cannot be read refuses rather than
    // reading as absent: absent skips the check it would have fed.
    let artifacts: Option<InvocationArtifacts> = match field("codex_artifacts") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => match serde_json::from_value(v) {
            Ok(artifacts) => Some(artifacts),
            Err(why) => {
                return Some(format!(
                    "its invocation artifacts cannot be read ({why}), so its recorded process \
                     tree cannot be checked"
                ))
            }
        },
    };
    if let Some(reason) = snapshots.processes.live_evidence_with(
        &issue.slug(),
        artifacts.as_ref(),
        &crate::autopilot::registry::configured_shared_daemon,
    ) {
        return Some(reason);
    }
    let super::process_tree::ProcessSnapshot::Available(rows) = &snapshots.processes else {
        return Some(snapshots.processes.reason());
    };
    let layout = format!("/autopilot/{}/", issue.slug());
    if let Some(r) = rows.iter().find(|r| r.active() && r.args.contains(&layout)) {
        return Some(format!(
            "process {} names a path in {}'s Autopilot invocation directory",
            r.pid,
            issue.slug()
        ));
    }
    let worktree = match field("worktree_path") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(path)) => match existing_worktree(Path::new(&path)) {
            Ok(worktree) => worktree,
            Err(reason) => return Some(reason),
        },
        Some(_) => {
            return Some(
                "its worktree path is not a string, so the processes in its worktree cannot \
                 be checked"
                    .into(),
            )
        }
    };
    if let Some(worktree) = worktree {
        let identity = artifacts
            .as_ref()
            .and_then(|a| super::process_tree::read_identity(a).ok());
        let started = field("started_at")
            .and_then(|v| v.as_str().map(str::to_owned))
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(&at).ok())
            .map(std::time::SystemTime::from)
            .and_then(super::process_tree::birth_at);
        let Some(spawned) = identity.as_ref().and_then(|i| i.root_birth).or(started) else {
            return Some(format!(
                "when its invocation started cannot be read, so the processes in its worktree \
                 {} cannot be checked",
                worktree.display()
            ));
        };
        let known: Vec<u32> = identity
            .iter()
            .flat_map(|i| std::iter::once(i.root_pid).chain(i.descendants.iter().map(|d| d.pid)))
            .collect();
        let daemon = crate::autopilot::registry::configured_shared_daemon();
        match super::process_tree::QuietCheck::once(
            &worktree,
            spawned,
            &known,
            daemon.as_ref(),
            rows,
            pids,
        ) {
            Ok(super::process_tree::Quiet::Yes) => {}
            Ok(super::process_tree::Quiet::Held(hold)) => return Some(hold.reason),
            Err(why) => return Some(why),
        }
    }
    // A session the drawer names may still be held by a process the tracker
    // never owned, as the readable clear checks a parked session.
    if let Some(session) = field("session_uuid").and_then(|v| v.as_str().map(str::to_owned)) {
        if let Some(reason) = session_holder(snapshots, &session) {
            return Some(reason);
        }
    }
    let own = field("ic_session_name").and_then(|v| v.as_str().map(str::to_owned));
    claude_ic_holder(snapshots, rows, issue, own.as_deref())
}

/// Why a Claude IC for the issue may be running: its registry lists it, or
/// an active process names it with `--name`. Both the drawer's own IC name
/// (`own`, the one its Claude IC ran under) and the name every Claude
/// dispatch for the issue takes ([`super::dispatch::ic_name`]) count.
pub(crate) fn claude_ic_holder(
    snapshots: &HarnessSnapshots,
    rows: &[super::process_tree::ProcessRow],
    issue: &IssueRef,
    own: Option<&str>,
) -> Option<String> {
    let mut names = vec![super::dispatch::ic_name(issue)];
    if let Some(own) = own.filter(|name| !name.is_empty() && !names.iter().any(|n| n == name)) {
        names.insert(0, own.to_string());
    }
    for ic in &names {
        if snapshots.claude.liveness(ic) == Liveness::Alive {
            return Some(format!("Claude's registry lists its IC session {ic}"));
        }
        if let Some(reason) = claude_ic_process_holder(rows, std::iter::once(ic.as_str())) {
            return Some(reason);
        }
    }
    None
}

/// Why a Claude IC for the issue may be running, on the process table
/// alone: an active process names one of `names` with `--name`. The
/// registry is not consulted (a fresh launch's check, `run`'s D4 rule).
pub(crate) fn claude_ic_process_holder<'a>(
    rows: &[super::process_tree::ProcessRow],
    names: impl Iterator<Item = &'a str>,
) -> Option<String> {
    for ic in names {
        let names_ic = |args: &str| {
            let words: Vec<&str> = args.split_whitespace().collect();
            words.windows(2).any(|w| w[0] == "--name" && w[1] == ic)
                || words.iter().any(|w| w.strip_prefix("--name=") == Some(ic))
        };
        if let Some(r) = rows.iter().find(|r| r.active() && names_ic(&r.args)) {
            return Some(format!(
                "process {} names its IC session {ic} with --name",
                r.pid
            ));
        }
    }
    None
}

/// Whether lineage already holds the attempt the state was dispatching: the
/// state is written with the count before that attempt, and recording it
/// raises the count past it.
fn attempt_recorded(db: &Database, state: &DispatchState) -> Result<bool, MemoryError> {
    Ok(lineage::get_issue_status(db, &state.issue)?
        .is_some_and(|status| status.cumulative_attempt_n > state.attempt_n))
}

fn refusal(
    snapshots: &HarnessSnapshots,
    state: &DispatchState,
    attempt_recorded: bool,
) -> Option<String> {
    if state.phase() == Some(dispatch_state::DispatchPhase::PausedSessionInUse) {
        if let Some(reason) = session_holder(snapshots, &state.session_uuid) {
            return Some(reason);
        }
    }
    match (state.implementer, snapshots.liveness(state)) {
        (_, Liveness::Alive) => Some(format!(
            "its {} implementation is alive",
            state.implementer.label()
        )),
        // Idle with pending artifacts is a finished invocation `run_issue` has
        // not recorded yet; clearing would drop its attempt from lineage.
        (implementer, Liveness::NotListed)
            if implementer.recovers_invocations()
                && state.invocation_artifacts.is_some()
                && !attempt_recorded =>
        {
            Some(format!(
                "a finished {} invocation is waiting to be recorded. A Lead records it \
                 when it next resumes the issue, or run `ironmem autopilot run {} {} \
                 --implementer {} --model <model> --title <issue title>` from the checkout \
                 it was dispatched in (add --path and --db if they are not the defaults)",
                implementer.label(),
                state.issue.repo,
                state.issue.number,
                implementer.cli_name()
            ))
        }
        // The registry lists an IC only once it registers, so a Claude IC
        // just launched (or one the registry dropped) is looked for in the
        // process table by name too, as an unreadable state's is.
        (Implementer::Claude, Liveness::NotListed) => {
            let super::process_tree::ProcessSnapshot::Available(rows) = &snapshots.processes else {
                return Some(snapshots.processes.reason());
            };
            claude_ic_holder(snapshots, rows, &state.issue, Some(&state.ic_session_name))
        }
        (_, Liveness::NotListed) => None,
        // Unknown is exactly the hold this exists for — but only once a fresh
        // process table shows nothing that could be the issue's work.
        (implementer, Liveness::Unknown) if implementer.recovers_invocations() => {
            snapshots.processes.live_evidence_with(
                &state.issue.slug(),
                state.invocation_artifacts.as_ref(),
                &crate::autopilot::registry::configured_shared_daemon,
            )
        }
        // Claude's registry could not be read. An operator clears only after
        // stopping the Leads, so the process table decides, as it does for
        // an unreadable state: no process naming its IC, and it clears.
        // Stricter than a run, which never adopts a Claude state on an
        // unreadable registry (`run::claude_recovery`).
        (_, Liveness::Unknown) => {
            let super::process_tree::ProcessSnapshot::Available(rows) = &snapshots.processes else {
                return Some(format!(
                    "{}, and {}",
                    snapshots.state_reason(state),
                    snapshots.processes.reason()
                ));
            };
            claude_ic_holder(snapshots, rows, &state.issue, Some(&state.ic_session_name))
        }
    }
}

/// A parked session's harness reported it still in use, and what holds it
/// may be a process the tracker never owned. Any active process that names
/// it with `--session-id` keeps it held; an unreadable process table does
/// too.
fn session_holder(snapshots: &HarnessSnapshots, session: &str) -> Option<String> {
    let super::process_tree::ProcessSnapshot::Available(rows) = &snapshots.processes else {
        return Some(snapshots.processes.reason());
    };
    let names_session = |args: &str| {
        let words: Vec<&str> = args.split_whitespace().collect();
        words
            .windows(2)
            .any(|w| w[0] == "--session-id" && w[1] == session)
            || words
                .iter()
                .any(|w| w.strip_prefix("--session-id=") == Some(session))
    };
    rows.iter()
        .find(|r| r.active() && names_session(&r.args))
        .map(|r| {
            format!(
                "process {} names its session {session} with --session-id, so the session may \
                 still be held; stop that process first",
                r.pid
            )
        })
}

/// Release the dispatch state of every issue the queue will never run again.
///
/// The queue counts every dispatch-state drawer as a held slot, but defers an
/// issue that already succeeded or reached its attempt cap before
/// `run_issue` — the one place that clears a finished state — could see it.
/// A controller that crashed between recording the outcome and clearing the
/// state would hold that slot for ever. A state is released only when its
/// harness shows nothing running (for Claude, its process table too), the
/// issue is finished, any invocation it names is already in lineage, it is
/// not parked as in use, and no controller holds the issue's lock; the
/// delete is conditional on the exact drawer judged, as [`clear_dispatch`]'s
/// is.
pub fn release_settled_dispatches(
    db: &Database,
    snapshots: &HarnessSnapshots,
    attempt_cap: u32,
) -> Result<Vec<IssueRef>, MemoryError> {
    let mut released = Vec::new();
    // Judged as scanned (raw bytes plus parsed claim), and cleared
    // against those same bytes: a claim rewritten since, identically
    // but for its owner, is a live run's, not this release's.
    let scan = dispatch_state::scan_dispatch_states(db, super::queue::IN_FLIGHT_SCAN_LIMIT)?;
    for (raw, claim) in &scan.claims {
        let state = &claim.state;
        if snapshots.liveness(state) != Liveness::NotListed {
            continue;
        }
        // A session parked as still in use waits for an operator, who knows
        // what holds it; a Claude IC is looked for by name too.
        if matches!(
            state.phase(),
            Some(
                dispatch_state::DispatchPhase::PausedSessionInUse
                    | dispatch_state::DispatchPhase::PausedCompletionUnproven
            )
        ) {
            continue;
        }
        if state.implementer == Implementer::Claude {
            let super::process_tree::ProcessSnapshot::Available(rows) = &snapshots.processes else {
                continue;
            };
            if claude_ic_holder(snapshots, rows, &state.issue, Some(&state.ic_session_name))
                .is_some()
            {
                continue;
            }
        }
        let status = lineage::get_issue_status(db, &state.issue)?;
        let cumulative_attempt_n = status.as_ref().map_or(0, |s| s.cumulative_attempt_n);
        let succeeded = status
            .as_ref()
            .is_some_and(|s| s.best_verdict == Some(AttemptOutcome::Success));
        // The queue's own two "never again" deferrals, read the same way.
        let finished = (succeeded && remediate::active_remediation(db, &state.issue)?.is_none())
            || remediate::attempts_charged(db, &state.issue, cumulative_attempt_n, succeeded)?
                >= attempt_cap;
        let unrecorded =
            state.invocation_artifacts.is_some() && cumulative_attempt_n <= state.attempt_n;
        if !finished || unrecorded {
            continue;
        }
        // Exactly the drawer judged as scanned, owner token included, and
        // only while no controller holds the issue: one that does may be
        // about to launch.
        let parsed = dispatch_state::Parsed::Readable(Box::new(state.clone()));
        let _lock = match controller_lock(&state.issue, &parsed, raw) {
            Ok(lock) => lock,
            Err(why) => {
                // Held is benign (retry next pass); a lock that cannot
                // even be checked would silently pin this slot forever,
                // so it is named.
                if !why.starts_with("a controller is running this issue") {
                    tracing::warn!(
                        issue = %state.issue.canonical(),
                        reason = %why,
                        "autopilot cannot check the issue's lock, so its finished dispatch state is not released"
                    );
                }
                continue;
            }
        };
        if dispatch_state::clear_raw_dispatch_state_if_unchanged(db, &state.issue, raw)? {
            released.push(state.issue.clone());
            // the released invocation's directory is evidence; older
            // ones past retention go. Best effort, as in run's clear.
            if let Some(common) = &claim.common_dir {
                let keep = state
                    .invocation_artifacts
                    .as_ref()
                    .map(|a| a.invocation_id.as_str());
                if let Err(e) = super::invocation::prune_invocations(
                    Path::new(common),
                    &state.issue.slug(),
                    keep,
                    super::invocation::INVOCATION_RETENTION,
                ) {
                    tracing::warn!(
                        error = %e,
                        issue = %state.issue.canonical(),
                        "autopilot could not prune old invocation directories"
                    );
                }
            }
        }
    }
    Ok(released)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::process_tree::{testing, ProcessRow, ProcessSnapshot};
    use crate::autopilot::registry::{AgentEntry, RegistrySnapshot};

    fn issue() -> IssueRef {
        IssueRef::new("owner/repo", 42)
    }

    fn held_state(implementer: Implementer, artifacts: InvocationArtifacts) -> DispatchState {
        DispatchState {
            implementer,
            native_session_id: None,
            invocation_artifacts: Some(artifacts),
            issue: issue(),
            worktree_path: "/tmp/wt".into(),
            ic_session_name: "ic-owner-repo-42".into(),
            dispatch_class: "logic".into(),
            attempt_n: 1,
            state: "dispatching".into(),
            started_at: "2026-10-26T00:00:00Z".into(),
            session_uuid: "controller-only".into(),
            turn_n: 1,
            session_claimed: true,
            session_model: None,
            resume_refusals: 0,
            signing_held: false,
        }
    }

    /// The readable state a clear released, failing on any other outcome.
    fn readable(cleared: Result<Cleared, MemoryError>) -> ClearedDispatch {
        match cleared.unwrap() {
            Cleared::Dispatch(cleared) => cleared,
            other => panic!("expected a readable state cleared: {other:?}"),
        }
    }

    /// Write `content` as the issue's dispatch-state drawer, as is.
    fn write_raw(db: &Database, content: &str) {
        crate::autopilot::write_current(db, &dispatch_state::dispatch_state_key(&issue()), content)
            .unwrap();
    }

    fn raw_drawer(db: &Database) -> Option<String> {
        crate::autopilot::read_current(db, &dispatch_state::dispatch_state_key(&issue()))
            .unwrap()
            .map(|drawer| drawer.content)
    }

    fn snapshots(rows: Vec<ProcessRow>) -> HarnessSnapshots {
        HarnessSnapshots {
            claude: RegistrySnapshot::Available(Vec::new()),
            processes: ProcessSnapshot::Available(rows),
        }
    }

    fn row(pid: u32, pgid: u32, args: &str) -> ProcessRow {
        ProcessRow {
            pid,
            ppid: 1,
            pgid,
            stat: "S".into(),
            args: args.into(),
        }
    }

    #[test]
    fn an_unprovable_codex_hold_clears_only_when_no_process_could_be_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        // Persisted identity, no supervisor exit record: Ctrl-C's shape.
        let (a, live) = testing::invocation(dir.path());
        dispatch_state::upsert_dispatch_state(&db, &held_state(Implementer::Codex, a.clone()))
            .unwrap();
        assert_eq!(
            snapshots(Vec::new()).liveness(&held_state(Implementer::Codex, a.clone())),
            Liveness::Unknown
        );

        let refused = |rows: Vec<ProcessRow>| {
            let err = clear_dispatch(&db, &issue(), || snapshots(rows))
                .unwrap_err()
                .to_string();
            assert!(err.contains("refusing to clear"), "{err}");
            assert!(dispatch_state::get_dispatch_state(&db, &issue())
                .unwrap()
                .is_some());
            err
        };
        // Its own marker, its recorded root, its process group, an ambiguous
        // Codex row, and another invocation for the same issue all refuse.
        assert!(refused(vec![live.clone()]).contains("alive"));
        assert!(refused(vec![row(1234, 9, "unrelated")]).contains("1234"));
        assert!(refused(vec![row(77, 1234, "make test")]).contains("process group"));
        assert!(refused(vec![row(
            78,
            78,
            "codex exec --future-flag v --output-schema /r/.git/ironmem/autopilot/owner-repo-7/11111111-2222-3333-4444-555555555555/schema.json p"
        )])
        .contains("ambiguous"));
        let sibling = "/r/.git/ironmem/autopilot/owner-repo-42/\
                       11111111-2222-3333-4444-555555555555/schema.json";
        assert!(refused(vec![row(
            79,
            79,
            &format!("codex exec --output-schema {sibling} p")
        )])
        .contains("owner-repo-42"));
        let unavailable = HarnessSnapshots {
            claude: RegistrySnapshot::Available(Vec::new()),
            processes: ProcessSnapshot::Unavailable {
                reason: "ps timed out".into(),
            },
        };
        assert!(clear_dispatch(&db, &issue(), || unavailable)
            .unwrap_err()
            .to_string()
            .contains("ps timed out"));

        // Nothing that could be it: cleared, artifacts reported, files kept.
        let other = "/r/.git/ironmem/autopilot/owner-repo-7/\
                     11111111-2222-3333-4444-555555555555/schema.json";
        let cleared = readable(clear_dispatch(&db, &issue(), || {
            snapshots(vec![
                row(80, 80, "sleep 5"),
                row(81, 81, &format!("codex exec --output-schema {other} p")),
            ])
        }));
        assert_eq!(cleared.invocation_artifacts.as_ref(), Some(&a));
        assert_eq!(cleared.state, "dispatching");
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_none());
        assert!(std::path::Path::new(&a.schema_path)
            .with_file_name("process.json")
            .is_file());
        assert!(clear_dispatch(&db, &issue(), || snapshots(Vec::new()))
            .unwrap_err()
            .to_string()
            .contains("no dispatch state"));
    }

    /// a claim still within its lease is not cleared, lock or no
    /// lock: its run may be between its claim and its launch.
    #[test]
    fn a_leased_claim_is_not_cleared() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = testing::invocation(dir.path());
        let mut state = held_state(Implementer::Claude, a);
        state.invocation_artifacts = None;
        let claim = dispatch_state::Claim {
            state,
            owner: Some("a-run".into()),
            claimed_at: Some(chrono::Utc::now().to_rfc3339()),
            common_dir: None,
        };
        assert!(dispatch_state::claim_if_unchanged(&db, None, &claim).unwrap());
        let err = clear_dispatch(&db, &issue(), || snapshots(Vec::new()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to clear"), "{err}");
        assert!(err.contains("120"), "{err}");
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_state_rewritten_between_read_and_delete_survives() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (a, _) = testing::invocation(dir.path());
        let judged = held_state(Implementer::Codex, a.clone());
        dispatch_state::upsert_dispatch_state(&db, &judged).unwrap();
        // A Lead resumes the issue after the state was read.
        let mut resumed = judged.clone();
        resumed.turn_n = 2;
        let err = clear_dispatch(&db, &issue(), || {
            dispatch_state::upsert_dispatch_state(&db, &resumed).unwrap();
            snapshots(Vec::new())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("changed while it was being checked"), "{err}");
        assert_eq!(
            dispatch_state::get_dispatch_state(&db, &issue()).unwrap(),
            Some(resumed)
        );
    }

    #[test]
    fn a_finished_unrecorded_codex_outcome_is_never_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (a, _) = testing::invocation(dir.path());
        testing::complete(&a);
        dispatch_state::upsert_dispatch_state(&db, &held_state(Implementer::Codex, a)).unwrap();
        let err = clear_dispatch(&db, &issue(), || snapshots(Vec::new()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("waiting to be recorded"), "{err}");
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_live_muse_root_refuses_clearing_and_names_muse() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (a, mut live) = testing::invocation(dir.path());
        live.args = format!(
            "/u/muse-bin-1.4.13-R6996.1 exec --json --output-schema {} --prompt-file /p.md",
            a.schema_path
        );
        dispatch_state::upsert_dispatch_state(&db, &held_state(Implementer::Muse, a)).unwrap();
        let err = clear_dispatch(&db, &issue(), || snapshots(vec![live]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("its Muse implementation is alive"), "{err}");
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_muse_hold_is_judged_by_process_evidence_and_names_its_own_harness() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (a, _) = testing::invocation(dir.path());
        let state = held_state(Implementer::Muse, a.clone());
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        // Unfinished: a live Muse invocation for the same issue holds it.
        let sibling = "/r/.git/ironmem/autopilot/owner-repo-42/\
                       11111111-2222-3333-4444-555555555555/schema.json";
        let err = clear_dispatch(&db, &issue(), || {
            snapshots(vec![row(
                79,
                79,
                &format!("/u/muse-bin-1.4.13-R6996.1 exec --output-schema {sibling}"),
            )])
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("owner-repo-42"), "{err}");
        testing::complete(&a);
        let err = clear_dispatch(&db, &issue(), || snapshots(Vec::new()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("a finished Muse invocation is waiting to be recorded"),
            "{err}"
        );
        assert!(err.contains("--implementer muse --model <model>"), "{err}");
    }

    fn record_status(db: &Database, best: Option<AttemptOutcome>, cumulative_attempt_n: u32) {
        lineage::upsert_issue_status(
            db,
            &lineage::IssueStatus {
                issue: issue(),
                best_verdict: best,
                best_commit_sha: None,
                cumulative_attempt_n,
            },
        )
        .unwrap();
    }

    #[test]
    fn the_unrecorded_refusal_names_a_runnable_command_and_a_recorded_attempt_clears() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (a, _) = testing::invocation(dir.path());
        testing::complete(&a);
        dispatch_state::upsert_dispatch_state(&db, &held_state(Implementer::Codex, a)).unwrap();
        let err = clear_dispatch(&db, &issue(), || snapshots(Vec::new()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--model <model> --title <issue title>"),
            "{err}"
        );
        // The attempt (attempt_n 1) is already in lineage: a crash came
        // between recording it and clearing the state.
        record_status(&db, Some(AttemptOutcome::Failed), 2);
        clear_dispatch(&db, &issue(), || snapshots(Vec::new())).unwrap();
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_none());
    }

    /// release judges the claim the scan read, owner token included,
    /// and gives back a settled owned claim's slot.
    #[test]
    fn release_gives_back_a_settled_owned_claim() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut state = held_state(Implementer::Claude, testing::invocation(dir.path()).0);
        state.invocation_artifacts = None;
        state.attempt_n = 3;
        let claim = dispatch_state::Claim {
            state,
            owner: Some("a-finished-run".into()),
            claimed_at: Some("2026-10-26T00:00:00Z".into()),
            common_dir: None,
        };
        assert!(dispatch_state::claim_if_unchanged(&db, None, &claim).unwrap());
        record_status(&db, Some(AttemptOutcome::Failed), 3);
        assert_eq!(
            release_settled_dispatches(&db, &snapshots(Vec::new()), 3).unwrap(),
            vec![issue()]
        );
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_none());
    }

    /// releasing a settled claim prunes the issue's invocation
    /// directories past retention under its common directory, keeping
    /// the released one.
    #[test]
    fn release_prunes_old_invocation_directories() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut state = held_state(Implementer::Claude, testing::invocation(dir.path()).0);
        state.invocation_artifacts = None;
        state.attempt_n = 3;
        let common = tempfile::tempdir().unwrap();
        let issue_dir = common.path().join("ironmem/autopilot/owner-repo-42");
        let old = issue_dir.join("0f0e0d0c-0b0a-4908-8706-050403020100");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::File::open(&old)
            .unwrap()
            .set_modified(
                std::time::SystemTime::now() - std::time::Duration::from_secs(20 * 24 * 60 * 60),
            )
            .unwrap();
        let claim = dispatch_state::Claim {
            state,
            owner: Some("a-finished-run".into()),
            claimed_at: Some("2026-10-26T00:00:00Z".into()),
            common_dir: Some(common.path().to_string_lossy().into_owned()),
        };
        assert!(dispatch_state::claim_if_unchanged(&db, None, &claim).unwrap());
        record_status(&db, Some(AttemptOutcome::Failed), 3);
        assert_eq!(
            release_settled_dispatches(&db, &snapshots(Vec::new()), 3).unwrap(),
            vec![issue()]
        );
        assert!(!old.exists(), "the old invocation directory was pruned");
    }

    #[test]
    fn only_a_finished_recorded_and_idle_dispatch_gives_its_slot_back() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (a, live) = testing::invocation(dir.path());
        testing::complete(&a);
        dispatch_state::upsert_dispatch_state(&db, &held_state(Implementer::Codex, a)).unwrap();
        let kept = |rows: Vec<ProcessRow>| {
            assert!(release_settled_dispatches(&db, &snapshots(rows), 3)
                .unwrap()
                .is_empty());
            assert!(dispatch_state::get_dispatch_state(&db, &issue())
                .unwrap()
                .is_some());
        };
        // Not finished: the queue will resume it.
        record_status(&db, Some(AttemptOutcome::Failed), 2);
        kept(Vec::new());
        // Succeeded, but the invocation the state names is not in lineage.
        record_status(&db, Some(AttemptOutcome::Success), 1);
        kept(Vec::new());
        // Succeeded and recorded, but its process is still visible.
        record_status(&db, Some(AttemptOutcome::Success), 2);
        kept(vec![live]);
        assert_eq!(
            release_settled_dispatches(&db, &snapshots(Vec::new()), 3).unwrap(),
            vec![issue()]
        );
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_none());

        // A Claude state at its attempt cap, gone from a readable registry.
        let mut claude = held_state(Implementer::Claude, testing::invocation(dir.path()).0);
        claude.invocation_artifacts = None;
        claude.attempt_n = 3;
        dispatch_state::upsert_dispatch_state(&db, &claude).unwrap();
        record_status(&db, Some(AttemptOutcome::Failed), 3);
        assert_eq!(
            release_settled_dispatches(&db, &snapshots(Vec::new()), 3).unwrap(),
            vec![issue()]
        );
    }

    /// A run parked because Muse reported its session still in use names no
    /// invocation; once nothing could be it, `clear-dispatch` releases it.
    #[test]
    fn a_muse_session_in_use_pause_is_released_by_clear_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut state = held_state(Implementer::Muse, testing::invocation(dir.path()).0);
        state.invocation_artifacts = None;
        state.session_uuid = "0d6c1e2a-5b8f-4c1d-9e3a-7f2b6a1c4d5e".into();
        state.native_session_id = Some(state.session_uuid.clone());
        state.state = "paused-session-in-use".into();
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        let cleared = readable(clear_dispatch(&db, &issue(), || snapshots(Vec::new())));
        assert_eq!(cleared.state, "paused-session-in-use");
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_none());
    }

    /// #342: the report names what the clear discards — the session and
    /// its turn count — so an operator releasing a paused dispatch can see
    /// exactly what is being thrown away (lineage is kept, and the CLI
    /// output says so).
    #[test]
    fn a_clear_reports_the_session_and_turns_it_discards() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let mut state = held_state(Implementer::Muse, testing::invocation(dir.path()).0);
        state.invocation_artifacts = None;
        state.session_uuid = "0d6c1e2a-5b8f-4c1d-9e3a-7f2b6a1c4d5e".into();
        state.native_session_id = Some(state.session_uuid.clone());
        state.state = "paused-infrastructure-failure".into();
        state.turn_n = 7;
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        let cleared = readable(clear_dispatch(&db, &issue(), || snapshots(Vec::new())));
        assert_eq!(cleared.state, "paused-infrastructure-failure");
        assert_eq!(cleared.session_uuid, "0d6c1e2a-5b8f-4c1d-9e3a-7f2b6a1c4d5e");
        assert_eq!(cleared.turn_n, 7);
    }

    /// The parked session may be held by a Muse process the tracker never
    /// owned. One that names it with `--session-id` holds the release.
    #[test]
    fn a_session_in_use_pause_is_not_released_while_a_process_names_its_session() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let session = "0d6c1e2a-5b8f-4c1d-9e3a-7f2b6a1c4d5e";
        let mut state = held_state(Implementer::Muse, testing::invocation(dir.path()).0);
        state.invocation_artifacts = None;
        state.session_uuid = session.into();
        state.native_session_id = Some(session.into());
        state.state = "paused-session-in-use".into();
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        let holder = |pid, sid: &str| {
            row(
                pid,
                pid,
                &format!(
                    "/Users/u/.local/bin/muse-bin-1.4.14-R7309.1 exec --json --session-id {sid} \
                     --prompt-file /tmp/p.md"
                ),
            )
        };
        for rows in [
            vec![holder(4242, session)],
            vec![row(
                4243,
                4243,
                &format!("muse exec --session-id={session}"),
            )],
        ] {
            let pid = rows[0].pid;
            let err = clear_dispatch(&db, &issue(), || snapshots(rows))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("refusing to clear") && err.contains(&format!("process {pid}")),
                "{err}"
            );
        }
        let mut exited = holder(4244, session);
        exited.stat = "Z".into();
        let other = holder(4245, "11111111-2222-4333-8444-555555555555");
        clear_dispatch(&db, &issue(), || snapshots(vec![exited, other])).unwrap();
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_claude_state_clears_only_against_a_readable_registry_that_omits_it() {
        let db = Database::open_in_memory().unwrap();
        let mut state = held_state(
            Implementer::Claude,
            testing::invocation(tempfile::tempdir().unwrap().path()).0,
        );
        state.invocation_artifacts = None;
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        let mut snaps = snapshots(Vec::new());
        snaps.claude = RegistrySnapshot::Available(vec![AgentEntry {
            name: "ic-owner-repo-42".into(),
            status: None,
        }]);
        assert!(clear_dispatch(&db, &issue(), || snaps.clone()).is_err());
        // An unreadable registry: the process table decides (C-M5, D3), and
        // one that cannot be read either refuses.
        snaps.claude = RegistrySnapshot::Unavailable {
            reason: "registry unreadable".into(),
        };
        let mut no_ps = snaps.clone();
        no_ps.processes = ProcessSnapshot::Unavailable {
            reason: "ps timed out".into(),
        };
        let err = clear_dispatch(&db, &issue(), || no_ps.clone())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("registry unreadable") && err.contains("ps timed out"),
            "{err}"
        );
        let mut named = snaps.clone();
        named.processes =
            ProcessSnapshot::Available(vec![row(98, 98, "claude -p x --name ic-owner-repo-42")]);
        assert!(clear_dispatch(&db, &issue(), || named.clone())
            .unwrap_err()
            .to_string()
            .contains("process 98"));
        assert_eq!(
            readable(clear_dispatch(&db, &issue(), || snaps.clone())).implementer,
            Implementer::Claude
        );
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        snaps.claude = RegistrySnapshot::Available(Vec::new());
        assert_eq!(
            readable(clear_dispatch(&db, &issue(), || snaps.clone())).implementer,
            Implementer::Claude
        );
    }

    /// A Claude IC the registry does not list yet (just launched, not yet
    /// registered) is still found by the name it was launched under.
    #[test]
    fn a_readable_claude_state_is_kept_while_a_process_names_its_ic() {
        let db = Database::open_in_memory().unwrap();
        let mut state = held_state(
            Implementer::Claude,
            testing::invocation(tempfile::tempdir().unwrap().path()).0,
        );
        state.invocation_artifacts = None;
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        for args in [
            "claude -p /goal x --name ic-owner-repo-42 --model m",
            "claude -p /goal x --name=ic-owner-repo-42 --model m",
        ] {
            let err = clear_dispatch(&db, &issue(), || snapshots(vec![row(98, 98, args)]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("process 98 names its IC session"), "{err}");
        }
        let unavailable = HarnessSnapshots {
            claude: RegistrySnapshot::Available(Vec::new()),
            processes: ProcessSnapshot::Unavailable {
                reason: "ps timed out".into(),
            },
        };
        assert!(clear_dispatch(&db, &issue(), || unavailable)
            .unwrap_err()
            .to_string()
            .contains("ps timed out"));
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_some());
        readable(clear_dispatch(&db, &issue(), || snapshots(Vec::new())));
    }

    /// A controller holding the issue's lock may have published its state
    /// and not yet spawned anything: no process evidence exists, and the
    /// lock is what says it is running.
    #[test]
    fn a_state_whose_controller_holds_the_issue_lock_is_kept() {
        let repo = tempfile::tempdir().unwrap();
        crate::autopilot::test_git::output(repo.path(), &["init", "-q"]);
        let db = Database::open_in_memory().unwrap();
        // Published with fresh artifacts, before any spawn: the directory
        // (and so its process.json) is gone with its dropped tempdir.
        let (a, _) = testing::invocation(tempfile::tempdir().unwrap().path());
        let mut state = held_state(Implementer::Muse, a);
        state.worktree_path = repo.path().to_string_lossy().into_owned();
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        let held =
            match crate::autopilot::issue_lock::try_lock_issue(repo.path(), &issue()).unwrap() {
                crate::autopilot::issue_lock::IssueLockAttempt::Acquired(lock) => lock,
                crate::autopilot::issue_lock::IssueLockAttempt::Held { .. } => unreachable!(),
            };
        let err = clear_dispatch(&db, &issue(), || snapshots(Vec::new()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("a controller is running this issue"), "{err}");
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_some());
        // An unreadable state naming the same worktree is kept the same way.
        write_raw(
            &db,
            &serde_json::json!({
                "implementer": "some-future-harness",
                "worktree_path": repo.path(),
            })
            .to_string(),
        );
        let err = clear_dispatch(&db, &issue(), || snapshots(Vec::new()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("a controller is running this issue"), "{err}");
        assert!(raw_drawer(&db).is_some());
        // Released, the readable state clears.
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        drop(held);
        readable(clear_dispatch(&db, &issue(), || snapshots(Vec::new())));
    }

    /// A marker path in Autopilot's layout for issue `owner-repo-<n>`.
    fn marker_for(n: u64) -> String {
        format!(
            "/r/.git/ironmem/autopilot/owner-repo-{n}/\
             11111111-2222-3333-4444-555555555555/schema.json"
        )
    }

    #[test]
    fn an_unreadable_dispatch_state_is_released_with_its_raw_content_reported() {
        let db = Database::open_in_memory().unwrap();
        // A credential-shaped value, built so no literal one sits in source.
        let secret = format!("{} abcdefgh{}", ["Bear", "er"].concat(), "SECRETSECRET");
        write_raw(
            &db,
            &format!(r#"{{"implementer":"some-future-harness","auth":"{secret}"}}"#),
        );
        // Another issue's invocation is no reason to hold this one.
        let other = marker_for(7);
        let cleared = clear_dispatch(&db, &issue(), || {
            snapshots(vec![row(
                81,
                81,
                &format!("muse exec --output-schema {other}"),
            )])
        })
        .unwrap();
        let Cleared::Unreadable(cleared) = cleared else {
            panic!("expected an unreadable state cleared: {cleared:?}");
        };
        assert_eq!(cleared.issue, "owner/repo#42");
        assert!(
            cleared.raw_content.contains("some-future-harness"),
            "{cleared:?}"
        );
        assert!(!cleared.raw_content.contains("SECRETSECRET"), "{cleared:?}");
        assert!(!cleared.reason.is_empty());
        assert_eq!(raw_drawer(&db), None);
        // The JSON names it for what it is.
        let json = serde_json::to_value(Cleared::Unreadable(cleared)).unwrap();
        assert!(json.get("raw_content").is_some(), "{json}");
    }

    #[test]
    fn an_unreadable_dispatch_state_is_kept_while_its_issue_may_be_running() {
        let db = Database::open_in_memory().unwrap();
        write_raw(&db, "not json");
        let ours = marker_for(42);
        for (evidence, why) in [
            (
                snapshots(vec![row(
                    90,
                    90,
                    &format!("muse exec --output-schema {ours}"),
                )]),
                "process 90",
            ),
            // A launcher shape the grammar cannot parse still names it.
            (
                snapshots(vec![row(91, 91, &format!("bash -e {ours}"))]),
                "process 91",
            ),
            // So does one whose path has a space, which no word split finds.
            (
                snapshots(vec![row(
                    94,
                    94,
                    &format!(
                        "bash {}/My Repo/.git/ironmem/autopilot/owner-repo-42/{}/schema.json",
                        "/Users/someone", "11111111-2222-3333-4444-555555555555"
                    ),
                )]),
                "process 94",
            ),
            (
                HarnessSnapshots {
                    claude: RegistrySnapshot::Available(Vec::new()),
                    processes: ProcessSnapshot::Unavailable {
                        reason: "ps timed out".into(),
                    },
                },
                "ps timed out",
            ),
            // Its Claude IC, by the name every Claude dispatch takes.
            (
                HarnessSnapshots {
                    claude: RegistrySnapshot::Available(vec![AgentEntry {
                        name: "ic-owner-repo-42".into(),
                        status: None,
                    }]),
                    processes: ProcessSnapshot::Available(Vec::new()),
                },
                "ic-owner-repo-42",
            ),
            // Or a live `claude` process named for it, with no registry.
            (
                HarnessSnapshots {
                    claude: RegistrySnapshot::Unavailable {
                        reason: "claude not found".into(),
                    },
                    processes: ProcessSnapshot::Available(vec![row(
                        92,
                        92,
                        "claude -p /goal x --name ic-owner-repo-42 --model m",
                    )]),
                },
                "process 92",
            ),
        ] {
            let err = clear_dispatch(&db, &issue(), || evidence)
                .unwrap_err()
                .to_string();
            assert!(err.contains("refusing to clear"), "{err}");
            assert!(err.contains(why), "{err}");
            assert_eq!(raw_drawer(&db).as_deref(), Some("not json"));
        }
    }

    #[test]
    fn an_unreadable_dispatch_state_clears_without_a_readable_claude_registry() {
        // An operator with no `claude` installed is not left holding it: the
        // process table alone settles whether a Claude IC runs.
        let db = Database::open_in_memory().unwrap();
        write_raw(&db, "not json");
        let cleared = clear_dispatch(&db, &issue(), || HarnessSnapshots {
            claude: RegistrySnapshot::Unavailable {
                reason: "claude not found".into(),
            },
            processes: ProcessSnapshot::Available(vec![row(
                93,
                93,
                "claude -p /goal x --name ic-owner-repo-7 --model m",
            )]),
        })
        .unwrap();
        assert!(matches!(cleared, Cleared::Unreadable(_)), "{cleared:?}");
        assert_eq!(raw_drawer(&db), None);
    }

    #[test]
    fn an_unreadable_dispatch_state_rewritten_while_checked_is_kept() {
        let db = Database::open_in_memory().unwrap();
        write_raw(&db, "not json");
        let err = clear_dispatch(&db, &issue(), || {
            // Another binary rewrites it after it was read.
            write_raw(&db, "still not json, but different");
            snapshots(Vec::new())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("changed while it was being checked"), "{err}");
        assert_eq!(
            raw_drawer(&db).as_deref(),
            Some("still not json, but different")
        );
    }

    /// An unreadable drawer that still names its invocation's artifacts and
    /// worktree is checked against them as a readable one would be: a child
    /// its recorded root left running in the worktree holds it, and once
    /// that child is gone the drawer is released.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_dispatch_state_is_kept_while_its_worktree_is_in_use() {
        let db = Database::open_in_memory().unwrap();
        let artifacts_dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        // The recorded root (PID 1234 in the fixture) has exited.
        let (a, _) = testing::invocation(artifacts_dir.path());
        // Just before the `sleep` below, which is all the window needs. Not a
        // minute: on Linux a start is counted from boot, and a CI VM (or a
        // Docker VM just restarted) may have booted under a minute ago, when
        // a start before boot reads as unknown and the clear refuses for that
        // reason instead.
        let started = (chrono::Utc::now() - chrono::Duration::seconds(2)).to_rfc3339();
        write_raw(
            &db,
            &serde_json::json!({
                "implementer": "some-future-harness",
                "codex_artifacts": a,
                "worktree_path": worktree.path(),
                "started_at": started,
            })
            .to_string(),
        );
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(worktree.path())
            .spawn()
            .unwrap();
        let pid = child.id();
        let only_it = move || -> Result<Vec<u32>, String> { Ok(vec![pid]) };
        let err = clear_dispatch_with(
            &db,
            &issue(),
            || snapshots(vec![row(pid, pid, "sleep 30")]),
            &only_it,
        );
        let _ = child.kill();
        let _ = child.wait();
        let err = err.unwrap_err().to_string();
        assert!(err.contains("refusing to clear"), "{err}");
        assert!(err.contains(&pid.to_string()), "{err}");
        assert!(raw_drawer(&db).is_some());
        // Gone: released.
        let cleared = clear_dispatch_with(&db, &issue(), || snapshots(Vec::new()), &only_it);
        assert!(matches!(cleared, Ok(Cleared::Unreadable(_))), "{cleared:?}");
        assert_eq!(raw_drawer(&db), None);
    }

    /// Its recorded tree counts too: a recorded descendant still running.
    #[test]
    fn an_unreadable_dispatch_state_is_kept_while_its_recorded_tree_runs() {
        let db = Database::open_in_memory().unwrap();
        let artifacts_dir = tempfile::tempdir().unwrap();
        let (a, _) = testing::invocation(artifacts_dir.path());
        write_raw(
            &db,
            &serde_json::json!({"implementer": "some-future-harness", "codex_artifacts": a})
                .to_string(),
        );
        // The fixture's recorded root, 1234, still in the table, unmarked.
        let err = clear_dispatch(&db, &issue(), || snapshots(vec![row(1234, 1234, "node x")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("1234"), "{err}");
        assert!(raw_drawer(&db).is_some());
    }

    /// What an unreadable drawer says about its session is used too: a
    /// process still holding its `session_uuid` (as the readable clear checks
    /// a `paused-session-in-use` state), and its own `ic_session_name` before
    /// the name a Claude dispatch for the issue would take.
    #[test]
    fn an_unreadable_dispatch_state_is_kept_while_its_named_session_is_held() {
        let db = Database::open_in_memory().unwrap();
        let session = "0d6c1e2a-5b8f-4c1d-9e3a-7f2b6a1c4d5e";
        write_raw(
            &db,
            &serde_json::json!({
                "implementer": "some-future-harness",
                "session_uuid": session,
                "ic_session_name": "ic-renamed-42",
            })
            .to_string(),
        );
        let held = |snaps: HarnessSnapshots, why: &str| {
            let err = clear_dispatch(&db, &issue(), || snaps)
                .unwrap_err()
                .to_string();
            assert!(err.contains(why), "{err}");
            assert!(raw_drawer(&db).is_some());
        };
        held(
            snapshots(vec![row(
                95,
                95,
                &format!("muse exec --session-id {session} --json"),
            )]),
            "process 95",
        );
        held(
            HarnessSnapshots {
                claude: RegistrySnapshot::Available(vec![AgentEntry {
                    name: "ic-renamed-42".into(),
                    status: None,
                }]),
                processes: ProcessSnapshot::Available(Vec::new()),
            },
            "ic-renamed-42",
        );
        held(
            snapshots(vec![row(
                96,
                96,
                "claude -p x --name ic-renamed-42 --model m",
            )]),
            "process 96",
        );
        // The name every Claude dispatch for the issue takes still counts.
        held(
            HarnessSnapshots {
                claude: RegistrySnapshot::Available(vec![AgentEntry {
                    name: "ic-owner-repo-42".into(),
                    status: None,
                }]),
                processes: ProcessSnapshot::Available(Vec::new()),
            },
            "ic-owner-repo-42",
        );
        held(
            snapshots(vec![row(
                97,
                97,
                "claude -p x --name ic-owner-repo-42 --model m",
            )]),
            "process 97",
        );
        let cleared = clear_dispatch(&db, &issue(), || snapshots(Vec::new()));
        assert!(matches!(cleared, Ok(Cleared::Unreadable(_))), "{cleared:?}");
    }

    /// An unreadable drawer naming a worktree, started just now.
    fn unreadable_naming(worktree: serde_json::Value, extra: serde_json::Value) -> String {
        let mut body = serde_json::json!({
            "implementer": "some-future-harness",
            "worktree_path": worktree,
            "started_at": (chrono::Utc::now() - chrono::Duration::seconds(2)).to_rfc3339(),
        });
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                body.insert(k.clone(), v.clone());
            }
        }
        body.to_string()
    }

    /// the unreadable path's other refusals and releases, each pinned.
    #[test]
    fn an_unreadable_dispatch_state_is_judged_on_every_field_it_still_names() {
        let nothing = || -> Result<Vec<u32>, String> { Ok(Vec::new()) };
        let clear = |db: &Database, pids: crate::autopilot::process_tree::ScanPids<'_>| {
            clear_dispatch_with(db, &issue(), || snapshots(Vec::new()), pids)
        };
        let refused =
            |raw: String, pids: crate::autopilot::process_tree::ScanPids<'_>, why: &str| {
                let db = Database::open_in_memory().unwrap();
                write_raw(&db, &raw);
                let err = clear(&db, pids).unwrap_err().to_string();
                assert!(err.contains(why), "{why}: {err}");
                assert_eq!(raw_drawer(&db).as_deref(), Some(raw.as_str()));
            };
        let worktree = tempfile::tempdir().unwrap();
        // A worktree path that is a file holds no process: released.
        let file = worktree.path().join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        let db = Database::open_in_memory().unwrap();
        write_raw(
            &db,
            &unreadable_naming(serde_json::json!(file), serde_json::json!({})),
        );
        assert!(matches!(clear(&db, &nothing), Ok(Cleared::Unreadable(_))));
        // A start that cannot be read, and no identity to fall back on.
        refused(
            unreadable_naming(
                serde_json::json!(worktree.path()),
                serde_json::json!({"started_at": "yesterday-ish"}),
            ),
            &nothing,
            "when its invocation started cannot be read",
        );
        // The worktree scan itself failing.
        let broken = || -> Result<Vec<u32>, String> { Err("the pid list failed".into()) };
        refused(
            unreadable_naming(serde_json::json!(worktree.path()), serde_json::json!({})),
            &broken,
            "the pid list failed",
        );
        // The auto-fix's two refusals: artifacts that do not parse, and a
        // worktree path that is not a string.
        refused(
            unreadable_naming(
                serde_json::json!(worktree.path()),
                serde_json::json!({"codex_artifacts": {"schema_path": 7}}),
            ),
            &nothing,
            "its invocation artifacts cannot be read",
        );
        refused(
            unreadable_naming(serde_json::json!(42), serde_json::json!({})),
            &nothing,
            "its worktree path is not a string",
        );
        // `--name=<ic>` names its IC as `--name <ic>` does.
        let db = Database::open_in_memory().unwrap();
        write_raw(&db, "not json");
        let err = clear_dispatch(&db, &issue(), || {
            snapshots(vec![row(
                99,
                99,
                "claude -p x --name=ic-owner-repo-42 --model m",
            )])
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("process 99 names its IC session"), "{err}");
    }

    /// a worktree that cannot be read is no proof it is empty.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_dispatch_state_whose_worktree_cannot_be_read_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        // Root reads through any mode, so the case cannot be made there.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let parent = tempfile::tempdir().unwrap();
        let worktree = parent.path().join("wt");
        std::fs::create_dir(&worktree).unwrap();
        let db = Database::open_in_memory().unwrap();
        write_raw(
            &db,
            &unreadable_naming(serde_json::json!(worktree), serde_json::json!({})),
        );
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
        let nothing = || -> Result<Vec<u32>, String> { Ok(Vec::new()) };
        let result = clear_dispatch_with(&db, &issue(), || snapshots(Vec::new()), &nothing);
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cannot be read"), "{err}");
        assert!(err.contains(&worktree.display().to_string()), "{err}");
        assert!(raw_drawer(&db).is_some());
    }

    /// A Claude state at its cap, finished and idle, in a real repository
    /// whose common directory its claim names (its worktree long gone).
    fn finished_claude_claim(db: &Database, repo: &std::path::Path, owner: &str) -> String {
        let mut claude = held_state(
            Implementer::Claude,
            testing::invocation(tempfile::tempdir().unwrap().path()).0,
        );
        claude.invocation_artifacts = None;
        claude.attempt_n = 3;
        claude.worktree_path = repo.join("gone-worktree").to_string_lossy().into_owned();
        let common = crate::autopilot::issue_lock::git_common_dir(repo)
            .unwrap()
            .unwrap();
        let claim = dispatch_state::Claim {
            state: claude,
            owner: Some(owner.into()),
            claimed_at: None,
            common_dir: Some(common.to_string_lossy().into_owned()),
        };
        let current = dispatch_state::get_claim(db, &issue()).unwrap();
        assert!(dispatch_state::claim_if_unchanged(db, current.as_ref(), &claim).unwrap());
        record_status(db, Some(AttemptOutcome::Failed), 3);
        raw_drawer(db).unwrap()
    }

    fn git_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        crate::autopilot::test_git::output(repo.path(), &["init", "-q"]);
        repo
    }

    fn hold(repo: &std::path::Path) -> crate::autopilot::issue_lock::IssueLock {
        match crate::autopilot::issue_lock::try_lock_issue(repo, &issue()).unwrap() {
            crate::autopilot::issue_lock::IssueLockAttempt::Acquired(lock) => lock,
            crate::autopilot::issue_lock::IssueLockAttempt::Held { .. } => unreachable!(),
        }
    }

    /// an adopting run's claim, identical but for its owner, written
    /// after clear-dispatch read the state, is never deleted.
    #[test]
    fn clear_dispatch_never_deletes_a_claim_a_run_made_since_it_read() {
        let repo = git_repo();
        let db = Database::open_in_memory().unwrap();
        finished_claude_claim(&db, repo.path(), "crashed");
        let read = dispatch_state::get_claim(&db, &issue()).unwrap().unwrap();
        let err = clear_dispatch(&db, &issue(), || {
            let mut adopted = read.clone();
            adopted.owner = Some("lead".into());
            assert!(dispatch_state::claim_if_unchanged(&db, Some(&read), &adopted).unwrap());
            snapshots(Vec::new())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("changed while it was being checked"), "{err}");
        assert_eq!(
            dispatch_state::get_claim(&db, &issue())
                .unwrap()
                .and_then(|c| c.owner),
            Some("lead".into())
        );
    }

    /// the claim names the common directory, so the lock is taken even
    /// once the worktree is gone.
    #[test]
    fn clear_dispatch_takes_the_lock_through_the_claims_common_dir() {
        let repo = git_repo();
        let db = Database::open_in_memory().unwrap();
        finished_claude_claim(&db, repo.path(), "crashed");
        let held = hold(repo.path());
        let err = clear_dispatch(&db, &issue(), || snapshots(Vec::new()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("a controller is running this issue"), "{err}");
        drop(held);
        readable(clear_dispatch(&db, &issue(), || snapshots(Vec::new())));
    }

    /// C-I1, release gives a slot back only for the exact claim judged,
    /// never while a controller holds the issue, never while a process names
    /// its Claude IC, and never for a session parked as in use.
    #[test]
    fn release_skips_a_held_issue_a_named_ic_and_a_parked_in_use_session() {
        let repo = git_repo();
        let db = Database::open_in_memory().unwrap();
        let raw = finished_claude_claim(&db, repo.path(), "crashed");
        let released =
            |rows: Vec<ProcessRow>| release_settled_dispatches(&db, &snapshots(rows), 3).unwrap();
        let held = hold(repo.path());
        assert!(released(Vec::new()).is_empty());
        drop(held);
        assert!(released(vec![row(98, 98, "claude -p x --name ic-owner-repo-42")]).is_empty());
        assert_eq!(raw_drawer(&db).as_deref(), Some(raw.as_str()));
        // Parked as in use: an operator's to release.
        let mut parked = dispatch_state::get_claim(&db, &issue()).unwrap().unwrap();
        let before = parked.clone();
        parked.state.state = dispatch_state::DispatchPhase::PausedSessionInUse
            .as_str()
            .into();
        assert!(dispatch_state::claim_if_unchanged(&db, Some(&before), &parked).unwrap());
        assert!(released(Vec::new()).is_empty());
        assert!(dispatch_state::claim_if_unchanged(&db, Some(&parked), &before).unwrap());
        assert_eq!(released(Vec::new()), vec![issue()]);
        assert!(raw_drawer(&db).is_none());
    }
}

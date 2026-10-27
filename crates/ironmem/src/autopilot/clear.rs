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
    /// Serialized as `codex_artifacts` so `clear-dispatch --json` output
    /// keeps its key for scripts written before Muse existed.
    #[serde(rename = "codex_artifacts")]
    pub invocation_artifacts: Option<InvocationArtifacts>,
}

/// `observe` is called after the state is read, so the evidence is never
/// older than the state it judges, and the delete happens only if the state
/// is still exactly the one judged.
pub fn clear_dispatch(
    db: &Database,
    issue: &IssueRef,
    observe: impl FnOnce() -> HarnessSnapshots,
) -> Result<ClearedDispatch, MemoryError> {
    let state = dispatch_state::get_dispatch_state(db, issue)?.ok_or_else(|| {
        MemoryError::NotFound(format!("{} has no dispatch state", issue.canonical()))
    })?;
    let refuse = |reason: String| {
        MemoryError::Validation(format!("refusing to clear {}: {reason}", issue.canonical()))
    };
    if let Some(reason) = refusal(&observe(), &state, attempt_recorded(db, &state)?) {
        return Err(refuse(reason));
    }
    if !dispatch_state::clear_dispatch_state_if_unchanged(db, &state)? {
        return Err(refuse(
            "its dispatch state changed while it was being checked; a dispatch may have \
             started — rerun once it settles"
                .into(),
        ));
    }
    Ok(ClearedDispatch {
        issue: issue.canonical(),
        implementer: state.implementer,
        state: state.state,
        invocation_artifacts: state.invocation_artifacts,
    })
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
    if state.state == "paused-session-in-use" {
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
        // A Claude hold means its registry could not be read; there is no
        // evidence to clear against.
        (_, Liveness::Unknown) => Some(snapshots.state_reason(state)),
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
/// harness shows nothing running, the issue is finished, and any invocation
/// it names is already in lineage; the delete is conditional, as
/// [`clear_dispatch`]'s is.
pub fn release_settled_dispatches(
    db: &Database,
    snapshots: &HarnessSnapshots,
    attempt_cap: u32,
) -> Result<Vec<IssueRef>, MemoryError> {
    let mut released = Vec::new();
    for state in dispatch_state::all_dispatch_states(db, super::queue::IN_FLIGHT_SCAN_LIMIT)? {
        if snapshots.liveness(&state) != Liveness::NotListed {
            continue;
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
        if finished && !unrecorded && dispatch_state::clear_dispatch_state_if_unchanged(db, &state)?
        {
            released.push(state.issue);
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
        let cleared = clear_dispatch(&db, &issue(), || {
            snapshots(vec![
                row(80, 80, "sleep 5"),
                row(81, 81, &format!("codex exec --output-schema {other} p")),
            ])
        })
        .unwrap();
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
        let cleared = clear_dispatch(&db, &issue(), || snapshots(Vec::new())).unwrap();
        assert_eq!(cleared.state, "paused-session-in-use");
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_none());
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
        snaps.claude = RegistrySnapshot::Unavailable {
            reason: "registry unreadable".into(),
        };
        assert!(clear_dispatch(&db, &issue(), || snaps.clone())
            .unwrap_err()
            .to_string()
            .contains("registry unreadable"));
        snaps.claude = RegistrySnapshot::Available(Vec::new());
        assert_eq!(
            clear_dispatch(&db, &issue(), || snaps.clone())
                .unwrap()
                .implementer,
            Implementer::Claude
        );
    }
}

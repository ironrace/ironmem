//! The operator's way out of a hold Autopilot cannot resolve by itself.
//!
//! Restart recovery holds a Codex invocation whose completion it cannot
//! prove — a controller killed by Ctrl-C, a root that failed before the first
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

use super::dispatch::CodexArtifacts;
use super::dispatch_state::{self, DispatchState};
use super::implementer::Implementer;
use super::registry::{HarnessSnapshots, Liveness, LivenessSnapshot};
use super::IssueRef;
use crate::db::schema::Database;
use crate::error::MemoryError;

/// What was cleared, so the operator knows where the evidence still lives.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClearedDispatch {
    pub issue: String,
    pub implementer: Implementer,
    pub state: String,
    pub codex_artifacts: Option<CodexArtifacts>,
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
    if let Some(reason) = refusal(&observe(), &state) {
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
        codex_artifacts: state.codex_artifacts,
    })
}

fn refusal(snapshots: &HarnessSnapshots, state: &DispatchState) -> Option<String> {
    match (state.implementer, snapshots.liveness(state)) {
        (_, Liveness::Alive) => Some(format!(
            "its {:?} implementation is alive",
            state.implementer
        )),
        // Idle with pending artifacts is a finished invocation `run_issue` has
        // not recorded yet; clearing would drop its attempt from lineage.
        (Implementer::Codex, Liveness::NotListed) if state.codex_artifacts.is_some() => {
            Some(format!(
                "a finished Codex invocation is waiting to be recorded; `ironmem autopilot \
                 run {} {} --implementer codex` records it",
                state.issue.repo, state.issue.number
            ))
        }
        (_, Liveness::NotListed) => None,
        // Unknown is exactly the hold this exists for — but only once a fresh
        // process table shows nothing that could be the issue's work.
        (Implementer::Codex, Liveness::Unknown) => snapshots
            .codex
            .live_evidence(&state.issue.slug(), state.codex_artifacts.as_ref()),
        // A Claude hold means its registry could not be read; there is no
        // evidence to clear against.
        (Implementer::Claude, Liveness::Unknown) => Some(snapshots.state_reason(state)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::codex_process::{testing, CodexProcessSnapshot, ProcessRow};
    use crate::autopilot::registry::{AgentEntry, RegistrySnapshot};

    fn issue() -> IssueRef {
        IssueRef::new("owner/repo", 42)
    }

    fn held_codex_state(artifacts: CodexArtifacts) -> DispatchState {
        DispatchState {
            implementer: Implementer::Codex,
            native_session_id: None,
            codex_artifacts: Some(artifacts),
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
        }
    }

    fn snapshots(rows: Vec<ProcessRow>) -> HarnessSnapshots {
        HarnessSnapshots {
            claude: RegistrySnapshot::Available(Vec::new()),
            codex: CodexProcessSnapshot::Available(rows),
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
        dispatch_state::upsert_dispatch_state(&db, &held_codex_state(a.clone())).unwrap();
        assert_eq!(
            snapshots(Vec::new()).liveness(&held_codex_state(a.clone())),
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
            "codex exec --output-schema relative/schema.json p"
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
            codex: CodexProcessSnapshot::Unavailable {
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
        assert_eq!(cleared.codex_artifacts.as_ref(), Some(&a));
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
        let judged = held_codex_state(a.clone());
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
        dispatch_state::upsert_dispatch_state(&db, &held_codex_state(a)).unwrap();
        let err = clear_dispatch(&db, &issue(), || snapshots(Vec::new()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("waiting to be recorded"), "{err}");
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_claude_state_clears_only_against_a_readable_registry_that_omits_it() {
        let db = Database::open_in_memory().unwrap();
        let mut state =
            held_codex_state(testing::invocation(tempfile::tempdir().unwrap().path()).0);
        state.implementer = Implementer::Claude;
        state.codex_artifacts = None;
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

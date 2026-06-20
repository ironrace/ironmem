use abeval::client::Usage;
use abeval::collab_db::SessionState;
use abeval::collab_driver::{
    run_collab_task, CodexAttributor, CodexResult, CollabStateReader, CollabTaskCtx, WorkerResult,
    WorkerSpawner,
};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

/// State reader that returns a scripted sequence of phases, one per poll.
struct ScriptedReader {
    states: Vec<SessionState>,
    idx: RefCell<usize>,
    /// `(drawer_id, rowid)` the next `newest_draft_drawer` call should surface,
    /// or `None` for "no DB drawer" (forces the `parse_ref_line` fallback).
    draft: Option<(String, i64)>,
}
impl CollabStateReader for ScriptedReader {
    fn read(&self, _session_id: &str) -> anyhow::Result<SessionState> {
        let mut i = self.idx.borrow_mut();
        let s = self.states[(*i).min(self.states.len() - 1)].clone();
        *i += 1;
        Ok(s)
    }
    fn newest_draft_drawer(&self, after_rowid: i64) -> anyhow::Result<Option<(String, i64)>> {
        Ok(self.draft.clone().filter(|(_, rowid)| *rowid > after_rowid))
    }
}

fn st(phase: &str, owner: &str, grr: u32) -> SessionState {
    SessionState {
        phase: phase.into(),
        current_owner: owner.into(),
        implementer: "claude".into(),
        pr_url: None,
        review_round: 0,
        global_review_round: grr,
        task_review_round: 0,
        last_head_sha: Some("h".into()),
    }
}

/// Records every spawn; claude turns return fixed usage + a verdict carrying a ref.
struct FakeSpawner {
    claude_prompts: RefCell<Vec<String>>,
    codex_calls: RefCell<u32>,
}
impl WorkerSpawner for FakeSpawner {
    fn spawn_claude(&self, prompt: &str, _wt: &Path) -> anyhow::Result<WorkerResult> {
        self.claude_prompts.borrow_mut().push(prompt.to_string());
        let stdout = if prompt.contains("ABEVAL_BOOTSTRAP") {
            "ABEVAL_SESSION_ID=sess-xyz\n".to_string()
        } else if prompt.contains("submit task_list from approved final plan") {
            "result: task_list sent (2 tasks)\n\
             ref: docs/superpowers/plans/x.md\nblocker: none\n"
                .to_string()
        } else {
            "result: ok\nref: drawer-1\nblocker: none\n".to_string()
        };
        Ok(WorkerResult {
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
            stdout,
        })
    }
    fn spawn_codex(&self, _session_id: &str, _wt: &Path) -> anyhow::Result<CodexResult> {
        *self.codex_calls.borrow_mut() += 1;
        Ok(CodexResult { commits_added: 2 })
    }
}

struct FixedAttributor(Usage);
impl CodexAttributor for FixedAttributor {
    fn attribute(&self) -> anyhow::Result<Usage> {
        Ok(self.0.clone())
    }
}

fn ctx(prompts_dir: &Path) -> CollabTaskCtx {
    CollabTaskCtx {
        task_id: "task1".into(),
        worktree: PathBuf::from("/tmp/wt"),
        branch: "abeval/task1".into(),
        prompts_dir: prompts_dir.to_path_buf(),
        bootstrap_prompt: "ABEVAL_BOOTSTRAP call collab_start".into(),
    }
}

/// Point prompts_dir at the real repo templates so render reads actual files.
fn repo_prompts_dir() -> PathBuf {
    // tests run with CWD = crate dir (benchmarks/abeval); repo root is two up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".claude-plugin/prompts")
}

#[test]
fn full_happy_path_sums_usage_and_counts_rework() {
    let prompts = repo_prompts_dir();
    // A faithful phase walk ending in CodingComplete.
    let reader = ScriptedReader {
        states: vec![
            st("PlanParallelDrafts", "claude", 0),
            st("PlanParallelDrafts", "codex", 0),
            st("PlanSynthesisPending", "claude", 0),
            st("PlanCodexReviewPending", "codex", 0),
            st("PlanClaudeFinalizePending", "claude", 0),
            st("PlanLocked", "claude", 0),
            st("CodeImplementPending", "claude", 0),
            st("CodeReviewFixGlobalPending", "codex", 1),
            st("CodeReviewLocalPending", "claude", 1),
            st("CodeReviewFinalPending", "claude", 1),
            st("CodingComplete", "claude", 1),
        ],
        idx: RefCell::new(0),
        draft: None,
    };
    let spawner = FakeSpawner {
        claude_prompts: RefCell::new(vec![]),
        codex_calls: RefCell::new(0),
    };
    let attributor = FixedAttributor(Usage {
        input_tokens: 1000,
        output_tokens: 200,
        ..Default::default()
    });

    let res = run_collab_task(&ctx(&prompts), &reader, &spawner, &attributor).unwrap();

    assert_eq!(res.reached_phase, "CodingComplete");
    assert_eq!(res.review_rounds, 1); // from global_review_round
    assert_eq!(res.fix_commits, 2); // one codex fix turn × 2 commits
    assert_eq!(*spawner.codex_calls.borrow(), 3); // draft + plan-review + fix
    assert_eq!(res.codex_usage.total(), 1200);
    // Claude usage = 1 bootstrap + 9 loop spawns (ClaudeSend×4,
    // ClaudeCompose×1×2, TaskListBridge×1, FinalReviewSynthetic×2)
    // = 10 spawns × 15 tokens each = 150.
    assert_eq!(res.claude_usage.total(), 150);
    assert_eq!(res.pr_url_synthetic, "https://abeval.invalid/task1");
    // The final-review path produced a synthetic submit, never a gh pr create.
    let prompts_seen = spawner.claude_prompts.borrow();
    assert!(prompts_seen
        .iter()
        .any(|p| p.contains("https://abeval.invalid/task1")));
}

#[test]
fn zero_codex_completed_run_is_invalid() {
    let prompts = repo_prompts_dir();
    let reader = ScriptedReader {
        states: vec![
            st("CodeImplementPending", "claude", 0),
            st("CodingComplete", "claude", 0),
        ],
        idx: RefCell::new(0),
        draft: None,
    };
    let spawner = FakeSpawner {
        claude_prompts: RefCell::new(vec![]),
        codex_calls: RefCell::new(0),
    };
    let attributor = FixedAttributor(Usage::default()); // zero Codex tokens

    let err = run_collab_task(&ctx(&prompts), &reader, &spawner, &attributor).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("codex"),
        "zero-codex completed run must fail loud: {err}"
    );
}

#[test]
fn anomaly_phase_owner_combo_errors() {
    let prompts = repo_prompts_dir();
    let reader = ScriptedReader {
        states: vec![st("CodeReviewFixGlobalPending", "claude", 0)], // claude can't own this
        idx: RefCell::new(0),
        draft: None,
    };
    let spawner = FakeSpawner {
        claude_prompts: RefCell::new(vec![]),
        codex_calls: RefCell::new(0),
    };
    let attributor = FixedAttributor(Usage {
        input_tokens: 1,
        ..Default::default()
    });
    let err = run_collab_task(&ctx(&prompts), &reader, &spawner, &attributor).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("anomaly"),
        "expected anomaly error: {err}"
    );
}

/// Reader that cycles its scripted states modulo their length (never clamps), so
/// the `(phase, owner, round)` key changes every poll. This drives a session that
/// progresses *between* states yet never reaches a terminal phase — the only
/// scenario that still exhausts `MAX_TURNS` now that an unchanging state bails
/// early via the stall guard.
struct CyclingReader {
    states: Vec<SessionState>,
    idx: RefCell<usize>,
}
impl CollabStateReader for CyclingReader {
    fn read(&self, _session_id: &str) -> anyhow::Result<SessionState> {
        let mut i = self.idx.borrow_mut();
        let s = self.states[*i % self.states.len()].clone();
        *i += 1;
        Ok(s)
    }
    fn newest_draft_drawer(&self, _after_rowid: i64) -> anyhow::Result<Option<(String, i64)>> {
        Ok(None)
    }
}

#[test]
fn max_turns_exhaustion_without_terminal_is_invalid() {
    // CONTRACT CHANGE: a *fixed* non-advancing state now trips the earlier stall
    // guard (see `stalled_phase_bails_invalid_before_exhausting_max_turns`), so
    // MAX_TURNS is reached only by a session that keeps changing key without ever
    // terminating. Alternating Claude/Codex ownership at PlanParallelDrafts cycles
    // forever: each poll flips the owner (key changes → no stall) but the phase
    // never advances to a terminal one.
    let prompts = repo_prompts_dir();
    let reader = CyclingReader {
        states: vec![
            st("PlanParallelDrafts", "claude", 0),
            st("PlanParallelDrafts", "codex", 0),
        ],
        idx: RefCell::new(0),
    };
    let spawner = FakeSpawner {
        claude_prompts: RefCell::new(vec![]),
        codex_calls: RefCell::new(0),
    };
    let attributor = FixedAttributor(Usage {
        input_tokens: 1,
        ..Default::default()
    });
    let err = run_collab_task(&ctx(&prompts), &reader, &spawner, &attributor).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("MAX_TURNS"),
        "expected MAX_TURNS exhaustion error: {err}"
    );
}

/// TEST 4 — CodingFailed + zero Codex is Ok, not Err.
/// The zero-Codex INVALID guard fires only on CodingComplete. A failed run that
/// produced no Codex sessions is still a valid (non-INVALID) data point.
#[test]
fn coding_failed_with_zero_codex_is_ok_not_err() {
    let prompts = repo_prompts_dir();
    let reader = ScriptedReader {
        states: vec![
            st("CodeImplementPending", "claude", 0),
            st("CodingFailed", "claude", 0),
        ],
        idx: RefCell::new(0),
        draft: None,
    };
    let spawner = FakeSpawner {
        claude_prompts: RefCell::new(vec![]),
        codex_calls: RefCell::new(0),
    };
    let attributor = FixedAttributor(Usage::default()); // zero Codex tokens

    let res = run_collab_task(&ctx(&prompts), &reader, &spawner, &attributor)
        .expect("CodingFailed + zero Codex must be Ok, not Err");
    assert_eq!(res.reached_phase, "CodingFailed");
    assert_eq!(res.codex_usage.total(), 0);
}

/// TEST 5 — zero-Claude on CodingComplete is INVALID.
/// A run that reaches CodingComplete but accumulated ZERO Claude tokens across
/// all worker turns must return Err with a message naming "claude".
struct ZeroUsageSpawner;

impl WorkerSpawner for ZeroUsageSpawner {
    fn spawn_claude(&self, prompt: &str, _wt: &std::path::Path) -> anyhow::Result<WorkerResult> {
        let stdout = if prompt.contains("ABEVAL_BOOTSTRAP") {
            "ABEVAL_SESSION_ID=sess-xyz\n".to_string()
        } else {
            "result: ok\nref: drawer-1\nblocker: none\n".to_string()
        };
        Ok(WorkerResult {
            usage: Usage::default(), // zero every turn
            stdout,
        })
    }
    fn spawn_codex(&self, _session_id: &str, _wt: &std::path::Path) -> anyhow::Result<CodexResult> {
        Ok(CodexResult { commits_added: 0 })
    }
}

#[test]
fn zero_claude_completed_run_is_invalid() {
    let prompts = repo_prompts_dir();
    let reader = ScriptedReader {
        states: vec![
            st("CodeImplementPending", "claude", 0),
            st("CodingComplete", "claude", 0),
        ],
        idx: RefCell::new(0),
        draft: None,
    };
    let spawner = ZeroUsageSpawner;
    // Non-zero Codex so only the Claude guard can fire.
    let attributor = FixedAttributor(Usage {
        input_tokens: 500,
        output_tokens: 100,
        ..Default::default()
    });

    let err = run_collab_task(&ctx(&prompts), &reader, &spawner, &attributor).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("claude"),
        "zero-claude completed run must fail loud naming claude: {err}"
    );
}

/// TEST 6 — ClaudeCompose returning no ref: line errors through run_collab_task.
/// A compose-phase spawner that returns a verdict without a `ref:` line must
/// produce an Err with a message containing "ref".
struct NoRefSpawner;

impl WorkerSpawner for NoRefSpawner {
    fn spawn_claude(&self, prompt: &str, _wt: &std::path::Path) -> anyhow::Result<WorkerResult> {
        let stdout = if prompt.contains("ABEVAL_BOOTSTRAP") {
            "ABEVAL_SESSION_ID=sess-xyz\n".to_string()
        } else {
            // Deliberate: no `ref:` line.
            "result: ok\nblocker: none\n".to_string()
        };
        Ok(WorkerResult {
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
            stdout,
        })
    }
    fn spawn_codex(&self, _session_id: &str, _wt: &std::path::Path) -> anyhow::Result<CodexResult> {
        Ok(CodexResult { commits_added: 0 })
    }
}

#[test]
fn compose_worker_returning_no_ref_errors() {
    let prompts = repo_prompts_dir();
    // PlanClaudeFinalizePending + claude → ClaudeCompose, which requires a ref: line.
    let reader = ScriptedReader {
        states: vec![st("PlanClaudeFinalizePending", "claude", 0)],
        idx: RefCell::new(0),
        draft: None,
    };
    let spawner = NoRefSpawner;
    // Non-zero Codex (won't be reached, but set for completeness).
    let attributor = FixedAttributor(Usage {
        input_tokens: 1,
        ..Default::default()
    });

    let err = run_collab_task(&ctx(&prompts), &reader, &spawner, &attributor).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("ref"),
        "missing ref: line must produce an error naming 'ref': {err}"
    );
}

/// TEST 7 (layer 8) — a compose worker that OMITS its `ref:` line must NOT fail
/// the run when the drawer it staged actually persisted: the driver recovers the
/// artifact ref from the newest `collab-drafts` drawer (rowid advanced past the
/// pre-compose snapshot). This is the drawer-staging-flakiness fix.
struct RecoverReader {
    states: Vec<SessionState>,
    idx: RefCell<usize>,
    /// Per-compose, `newest_draft_drawer` is called twice: first the pre-compose
    /// snapshot (no new drawer yet → None), then the post-compose resolve (the
    /// drawer the worker persisted → Some). Toggles on each call.
    calls: RefCell<u32>,
}
impl CollabStateReader for RecoverReader {
    fn read(&self, _session_id: &str) -> anyhow::Result<SessionState> {
        let mut i = self.idx.borrow_mut();
        let s = self.states[(*i).min(self.states.len() - 1)].clone();
        *i += 1;
        Ok(s)
    }
    fn newest_draft_drawer(&self, after_rowid: i64) -> anyhow::Result<Option<(String, i64)>> {
        let mut c = self.calls.borrow_mut();
        *c += 1;
        // Odd call = pre-compose snapshot (empty); even = post-compose (persisted).
        if *c % 2 == 1 {
            Ok(None)
        } else {
            Ok(Some(("recovered-drawer".to_string(), 1)).filter(|(_, r)| *r > after_rowid))
        }
    }
}

#[test]
fn compose_worker_missing_ref_recovers_persisted_drawer() {
    let prompts = repo_prompts_dir();
    // One compose phase (final), then terminal. The worker omits `ref:`.
    let reader = RecoverReader {
        states: vec![
            st("PlanClaudeFinalizePending", "claude", 0),
            st("CodingComplete", "claude", 1),
        ],
        idx: RefCell::new(0),
        calls: RefCell::new(0),
    };
    let spawner = NoRefSpawner; // never prints a ref: line
    let attributor = FixedAttributor(Usage {
        input_tokens: 100,
        output_tokens: 20,
        ..Default::default()
    });

    let res = run_collab_task(&ctx(&prompts), &reader, &spawner, &attributor)
        .expect("missing ref: line must be recovered from the persisted drawer, not fail");
    // Reaching the terminal phase is the proof: without recovery, the compose at
    // PlanClaudeFinalizePending would have errored with "no ref: line".
    assert_eq!(res.reached_phase, "CodingComplete");
}

/// TEST 8 (layer 9) — the PlanLocked TaskListBridge must fail clean when the
/// submit worker reports a blocker, rather than waiting for the stall guard.
struct TaskListBlockerSpawner;
impl WorkerSpawner for TaskListBlockerSpawner {
    fn spawn_claude(&self, prompt: &str, _wt: &std::path::Path) -> anyhow::Result<WorkerResult> {
        let stdout = if prompt.contains("ABEVAL_BOOTSTRAP") {
            "ABEVAL_SESSION_ID=sess-xyz\n".to_string()
        } else {
            "result: task_list not sent\n\
             ref: none\nblocker: task 2 exceeds 20 minutes\n"
                .to_string()
        };
        Ok(WorkerResult {
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
            stdout,
        })
    }
    fn spawn_codex(&self, _session_id: &str, _wt: &std::path::Path) -> anyhow::Result<CodexResult> {
        Ok(CodexResult { commits_added: 0 })
    }
}

#[test]
fn task_list_blocker_aborts_clean() {
    let prompts = repo_prompts_dir();
    let reader = ScriptedReader {
        states: vec![st("PlanLocked", "claude", 0)],
        idx: RefCell::new(0),
        draft: None,
    };
    let spawner = TaskListBlockerSpawner;
    let attributor = FixedAttributor(Usage {
        input_tokens: 1,
        ..Default::default()
    });
    let err = run_collab_task(&ctx(&prompts), &reader, &spawner, &attributor).unwrap_err();
    assert!(
        err.to_string().contains("20 minutes"),
        "task-list blocker must abort the run with the blocker detail: {err}"
    );
}

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use abeval::client::Usage;
use abeval::collab_db::SessionState;
use abeval::collab_driver::{
    run_collab_task, CodexAttributor, CodexResult, CollabStateReader, CollabTaskCtx,
    WorkerResult, WorkerSpawner,
};

/// State reader that returns a scripted sequence of phases, one per poll.
struct ScriptedReader {
    states: Vec<SessionState>,
    idx: RefCell<usize>,
}
impl CollabStateReader for ScriptedReader {
    fn read(&self, _session_id: &str) -> anyhow::Result<SessionState> {
        let mut i = self.idx.borrow_mut();
        let s = self.states[(*i).min(self.states.len() - 1)].clone();
        *i += 1;
        Ok(s)
    }
}

fn st(phase: &str, owner: &str, grr: u32) -> SessionState {
    SessionState {
        phase: phase.into(),
        current_owner: owner.into(),
        implementer: "claude".into(),
        pr_url: None,
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
        } else {
            "result: ok\nref: drawer-1\nblocker: none\n".to_string()
        };
        Ok(WorkerResult {
            usage: Usage { input_tokens: 10, output_tokens: 5, ..Default::default() },
            stdout,
        })
    }
    fn spawn_codex(&self, _session_id: &str, _wt: &Path) -> anyhow::Result<CodexResult> {
        *self.codex_calls.borrow_mut() += 1;
        Ok(CodexResult { usage_hint: Usage::default(), commits_added: 2 })
    }
}

struct FixedAttributor(Usage);
impl CodexAttributor for FixedAttributor {
    fn attribute(&self) -> anyhow::Result<Usage> { Ok(self.0.clone()) }
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
        .parent().unwrap().parent().unwrap()
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
    };
    let spawner = FakeSpawner { claude_prompts: RefCell::new(vec![]), codex_calls: RefCell::new(0) };
    let attributor = FixedAttributor(Usage { input_tokens: 1000, output_tokens: 200, ..Default::default() });

    let res = run_collab_task(&ctx(&prompts), &reader, &spawner, &attributor).unwrap();

    assert_eq!(res.reached_phase, "CodingComplete");
    assert_eq!(res.review_rounds, 1);          // from global_review_round
    assert_eq!(res.fix_commits, 2);            // one codex fix turn × 2 commits
    assert_eq!(*spawner.codex_calls.borrow(), 3); // draft + plan-review + fix
    assert_eq!(res.codex_usage.total(), 1200);
    // Claude usage = 1 bootstrap + 11 loop spawns (ClaudeSend×3, ClaudeCompose×3×2,
    // FinalReviewSynthetic×2) = 12 spawns × 15 tokens each = 180.
    assert_eq!(res.claude_usage.total(), 180);
    assert_eq!(res.pr_url_synthetic, "local://abeval/task1");
    // The final-review path produced a synthetic submit, never a gh pr create.
    let prompts_seen = spawner.claude_prompts.borrow();
    assert!(prompts_seen.iter().any(|p| p.contains("local://abeval/task1")));
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
    };
    let spawner = FakeSpawner { claude_prompts: RefCell::new(vec![]), codex_calls: RefCell::new(0) };
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
    };
    let spawner = FakeSpawner { claude_prompts: RefCell::new(vec![]), codex_calls: RefCell::new(0) };
    let attributor = FixedAttributor(Usage { input_tokens: 1, ..Default::default() });
    let err = run_collab_task(&ctx(&prompts), &reader, &spawner, &attributor).unwrap_err();
    assert!(err.to_string().to_lowercase().contains("anomaly"), "expected anomaly error: {err}");
}

#[test]
fn max_turns_exhaustion_without_terminal_is_invalid() {
    let prompts = repo_prompts_dir();
    // Always a Claude-owned send phase; never terminal.
    let reader = ScriptedReader {
        states: vec![st("CodeImplementPending", "claude", 0)],
        idx: RefCell::new(0),
    };
    let spawner = FakeSpawner { claude_prompts: RefCell::new(vec![]), codex_calls: RefCell::new(0) };
    let attributor = FixedAttributor(Usage { input_tokens: 1, ..Default::default() });
    let err = run_collab_task(&ctx(&prompts), &reader, &spawner, &attributor).unwrap_err();
    let msg = err.to_string().to_lowercase();
    assert!(msg.contains("max_turns") || msg.contains("terminal"), "expected exhaustion error: {err}");
}

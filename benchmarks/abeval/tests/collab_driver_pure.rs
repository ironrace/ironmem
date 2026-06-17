use std::fs;
use abeval::collab_driver::{
    parse_ref_line, parse_session_id, render_worker_prompt, worker_action, WorkerAction,
};

#[test]
fn dispatch_matrix_maps_each_phase() {
    // Claude-owned planning/coding phases.
    assert_eq!(
        worker_action("PlanParallelDrafts", "claude", 0),
        WorkerAction::ClaudeSend { template: "collab-turn-plan-draft.md", mode: "send" }
    );
    assert_eq!(
        worker_action("PlanSynthesisPending", "claude", 0),
        WorkerAction::ClaudeCompose { template: "collab-turn-plan-synthesis.md", topic: "canonical" }
    );
    // Revision round: send, not compose.
    assert_eq!(
        worker_action("PlanSynthesisPending", "claude", 1),
        WorkerAction::ClaudeSend { template: "collab-turn-plan-synthesis.md", mode: "send" }
    );
    assert_eq!(
        worker_action("PlanClaudeFinalizePending", "claude", 0),
        WorkerAction::ClaudeCompose { template: "collab-turn-plan-finalize.md", topic: "final" }
    );
    assert_eq!(
        worker_action("PlanLocked", "claude", 0),
        WorkerAction::ClaudeCompose { template: "collab-turn-task-list.md", topic: "task_list" }
    );
    assert_eq!(
        worker_action("CodeImplementPending", "claude", 0),
        WorkerAction::ClaudeSend { template: "collab-turn-code-implement.md", mode: "send" }
    );
    assert_eq!(
        worker_action("CodeReviewLocalPending", "claude", 0),
        WorkerAction::ClaudeSend { template: "collab-turn-review-local.md", mode: "send" }
    );
    assert_eq!(
        worker_action("CodeReviewFinalPending", "claude", 0),
        WorkerAction::FinalReviewSynthetic
    );
    // Codex-owned phases.
    assert_eq!(worker_action("PlanParallelDrafts", "codex", 0), WorkerAction::Codex);
    assert_eq!(worker_action("PlanCodexReviewPending", "codex", 0), WorkerAction::Codex);
    assert_eq!(worker_action("CodeReviewFixGlobalPending", "codex", 0), WorkerAction::Codex);
    // Terminal.
    assert_eq!(worker_action("CodingComplete", "claude", 0), WorkerAction::Terminal);
    assert_eq!(worker_action("CodingFailed", "codex", 0), WorkerAction::Terminal);
    // Owner/phase mismatch is an anomaly (e.g. claude owning a codex-only phase).
    assert_eq!(worker_action("CodeReviewFixGlobalPending", "claude", 0), WorkerAction::Anomaly);
    assert_eq!(worker_action("CodeImplementPending", "codex", 0), WorkerAction::Anomaly);
}

#[test]
fn ref_line_parsed_from_verdict() {
    let verdict = "result: canonical composed\nref: drawer-9f2a\nblocker: none\n";
    assert_eq!(parse_ref_line(verdict), Some("drawer-9f2a".to_string()));
    let none = "result: x\nref: none\nblocker: none\n";
    assert_eq!(parse_ref_line(none), None);
    assert_eq!(parse_ref_line("no ref at all"), None);
}

#[test]
fn session_id_parsed_from_bootstrap() {
    let out = "some chatter\nABEVAL_SESSION_ID=774344f8\nmore\n";
    assert_eq!(parse_session_id(out).unwrap(), "774344f8");
    assert!(parse_session_id("no marker here").is_err());
}

#[test]
fn template_render_substitutes_vars_without_prefix_clobber() {
    let dir = tempfile::tempdir().unwrap();
    let tpl = "session=$SESSION_ID ref=$ARTIFACT_REF hash=$ARTIFACT_HASH branch=$BRANCH mode=$MODE topic=$TOPIC";
    fs::write(dir.path().join("t.md"), tpl).unwrap();
    let out = render_worker_prompt(
        dir.path(),
        "t.md",
        &[
            ("$SESSION_ID", "s1"),
            ("$ARTIFACT_REF", "drawer-1"),
            ("$ARTIFACT_HASH", "deadbeef"),
            ("$BRANCH", "abeval/task1"),
            ("$MODE", "compose"),
            ("$TOPIC", "canonical"),
        ],
    )
    .unwrap();
    assert_eq!(
        out,
        "session=s1 ref=drawer-1 hash=deadbeef branch=abeval/task1 mode=compose topic=canonical"
    );
}

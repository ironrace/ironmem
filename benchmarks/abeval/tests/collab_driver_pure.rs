use abeval::collab_driver::{
    is_session_limit_error, parse_ref_line, parse_session_id, render_worker_prompt, worker_action,
    ModelTier, WorkerAction,
};
use std::fs;

#[test]
fn session_limit_signature_detected_but_not_overmatched() {
    // External account-wide limit conditions (surfaced in the worker error's
    // stdout tail by Gap 1) → retryable/excludable, never a task FAILED.
    assert!(is_session_limit_error(
        "claude worker exited Some(1) in /wt — stderr:  — \
         stdout tail: …Claude usage limit reached. Resets at 9pm."
    ));
    assert!(is_session_limit_error(
        "…you've hit your session limit, try again later…"
    ));
    assert!(is_session_limit_error(
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"…"}}"#
    ));

    // Genuine red gates / task output that merely mention "limit" or "rate" must
    // NOT be misclassified as retryable — that would corrupt the FAILED data
    // point by silently dropping it from the corpus.
    assert!(!is_session_limit_error(
        "claude worker exited Some(1) in /wt — stderr: thread 'main' panicked — \
         stdout tail: …error[E0599]: no method named `rate` found"
    ));
    assert!(!is_session_limit_error(
        "test failed: assert_eq!(left == right) where left=`limit` right=`5`"
    ));
    assert!(!is_session_limit_error(
        "compilation error: unused variable `rate_limiter`"
    ));
    assert!(!is_session_limit_error(""));
}

#[test]
fn dispatch_matrix_maps_each_phase() {
    // Claude-owned planning/coding phases.
    assert_eq!(
        worker_action("PlanParallelDrafts", "claude", 0),
        WorkerAction::ClaudeSend {
            template: "collab-turn-plan-draft.md",
            mode: "send",
            model: ModelTier::Opus,
        }
    );
    assert_eq!(
        worker_action("PlanSynthesisPending", "claude", 0),
        WorkerAction::ClaudeSend {
            template: "collab-turn-plan-synthesis.md",
            mode: "send",
            model: ModelTier::Opus,
        }
    );
    // Synthesis is always autonomous now; final is the only human planning gate.
    assert_eq!(
        worker_action("PlanSynthesisPending", "claude", 1),
        WorkerAction::ClaudeSend {
            template: "collab-turn-plan-synthesis.md",
            mode: "send",
            model: ModelTier::Opus,
        }
    );
    assert_eq!(
        worker_action("PlanClaudeFinalizePending", "claude", 0),
        WorkerAction::ClaudeCompose {
            template: "collab-turn-plan-finalize.md",
            topic: "final",
            model: ModelTier::Opus,
        }
    );
    assert_eq!(
        // PlanLocked is the v3 bridge: one mechanical task-list submit worker,
        // not another Superpowers planning pass.
        worker_action("PlanLocked", "claude", 0),
        WorkerAction::TaskListBridge
    );
    assert_eq!(
        worker_action("CodeImplementPending", "claude", 0),
        WorkerAction::ClaudeSend {
            template: "collab-turn-code-implement.md",
            mode: "send",
            model: ModelTier::Sonnet,
        }
    );
    assert_eq!(
        worker_action("CodeReviewLocalPending", "claude", 0),
        WorkerAction::ClaudeSend {
            template: "collab-turn-review-local.md",
            mode: "send",
            model: ModelTier::Opus,
        }
    );
    assert_eq!(
        worker_action("CodeReviewFinalPending", "claude", 0),
        WorkerAction::FinalReviewSynthetic
    );
    // Codex-owned phases.
    assert_eq!(
        worker_action("PlanParallelDrafts", "codex", 0),
        WorkerAction::Codex
    );
    assert_eq!(
        worker_action("PlanCodexReviewPending", "codex", 0),
        WorkerAction::Codex
    );
    assert_eq!(
        worker_action("CodeReviewFixGlobalPending", "codex", 0),
        WorkerAction::Codex
    );
    // Terminal.
    assert_eq!(
        worker_action("CodingComplete", "claude", 0),
        WorkerAction::Terminal
    );
    assert_eq!(
        worker_action("CodingFailed", "codex", 0),
        WorkerAction::Terminal
    );
    // Owner/phase mismatch is an anomaly (e.g. claude owning a codex-only phase).
    assert_eq!(
        worker_action("CodeReviewFixGlobalPending", "claude", 0),
        WorkerAction::Anomaly
    );
    assert_eq!(
        worker_action("CodeImplementPending", "codex", 0),
        WorkerAction::Anomaly
    );
    // TEST 7: unknown owner string is always an anomaly, regardless of phase.
    assert_eq!(
        worker_action("PlanLocked", "human", 0),
        WorkerAction::Anomaly
    );
    // Terminal wins regardless of owner — even an unknown owner string.
    assert_eq!(
        worker_action("CodingComplete", "nobody", 0),
        WorkerAction::Terminal
    );
}

/// The per-turn model tier is the contribution of this campaign patch (memory
/// project_abeval_campaign_model_tiering): planning + review turns run on opus
/// (deepest reasoning for design/review), implementation runs on sonnet (the
/// designated best coding model — opus already did the design in planning). The
/// mechanical TaskListBridge/submit turns are pinned to sonnet at their call sites
/// (they carry no action-level model). This test is the frozen contract for the
/// tiers that ARE a dispatch decision.
#[test]
fn dispatch_matrix_pins_model_tier_per_phase() {
    fn model_of(action: WorkerAction) -> ModelTier {
        match action {
            WorkerAction::ClaudeSend { model, .. } | WorkerAction::ClaudeCompose { model, .. } => {
                model
            }
            other => panic!("expected a Claude action carrying a model tier, got {other:?}"),
        }
    }
    // Planning turns → opus.
    assert_eq!(
        model_of(worker_action("PlanParallelDrafts", "claude", 0)),
        ModelTier::Opus
    );
    assert_eq!(
        model_of(worker_action("PlanSynthesisPending", "claude", 0)),
        ModelTier::Opus
    );
    assert_eq!(
        model_of(worker_action("PlanClaudeFinalizePending", "claude", 0)),
        ModelTier::Opus
    );
    // Review turn → opus.
    assert_eq!(
        model_of(worker_action("CodeReviewLocalPending", "claude", 0)),
        ModelTier::Opus
    );
    // Implementation → sonnet (the one Claude work turn that drops a tier).
    assert_eq!(
        model_of(worker_action("CodeImplementPending", "claude", 0)),
        ModelTier::Sonnet
    );
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

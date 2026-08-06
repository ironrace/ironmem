use abeval::collab_driver::{
    is_session_limit_error, parse_ref_line, parse_session_id, render_worker_prompt, worker_action,
    ModelTier, RecoveryState, WorkerAction,
};
use std::fs;

/// A recovery in flight for `(phase, owner)`: the state machine kept the
/// session in `phase` and flipped `current_owner` to `owner`.
fn recovering<'a>(phase: &'a str, owner: &'a str) -> RecoveryState<'a> {
    RecoveryState {
        pending_failure: Some("git_push_failed: remote hung up"),
        recovery_phase: Some(phase),
        recovery_owner: Some(owner),
    }
}

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
    assert!(!is_session_limit_error(
        "task output: 429 Too Many Requests from the app under test"
    ));
    assert!(!is_session_limit_error(
        "integration test got HTTP 429: rate limit exceeded"
    ));
    assert!(!is_session_limit_error(""));
}

#[test]
fn dispatch_matrix_maps_each_phase() {
    // Claude-owned planning/coding phases.
    assert_eq!(
        worker_action("PlanParallelDrafts", "claude", 0, RecoveryState::NONE),
        WorkerAction::ClaudeSend {
            template: "collab-turn-plan-draft.md",
            mode: "send",
            model: ModelTier::Opus,
        }
    );
    assert_eq!(
        worker_action("PlanSynthesisPending", "claude", 0, RecoveryState::NONE),
        WorkerAction::ClaudeSend {
            template: "collab-turn-plan-synthesis.md",
            mode: "send",
            model: ModelTier::Opus,
        }
    );
    // Synthesis is always autonomous now; final is the only human planning gate.
    assert_eq!(
        worker_action("PlanSynthesisPending", "claude", 1, RecoveryState::NONE),
        WorkerAction::ClaudeSend {
            template: "collab-turn-plan-synthesis.md",
            mode: "send",
            model: ModelTier::Opus,
        }
    );
    assert_eq!(
        worker_action(
            "PlanClaudeFinalizePending",
            "claude",
            0,
            RecoveryState::NONE
        ),
        WorkerAction::ClaudeCompose {
            template: "collab-turn-plan-finalize.md",
            topic: "final",
            model: ModelTier::Opus,
        }
    );
    assert_eq!(
        // PlanLocked is the v3 bridge: one mechanical task-list submit worker,
        // not another Superpowers planning pass.
        worker_action("PlanLocked", "claude", 0, RecoveryState::NONE),
        WorkerAction::TaskListBridge
    );
    assert_eq!(
        worker_action("CodeImplementPending", "claude", 0, RecoveryState::NONE),
        WorkerAction::ClaudeSend {
            template: "collab-turn-code-implement.md",
            mode: "send",
            model: ModelTier::Sonnet,
        }
    );
    assert_eq!(
        worker_action("CodeReviewLocalPending", "claude", 0, RecoveryState::NONE),
        WorkerAction::ClaudeSend {
            template: "collab-turn-review-local.md",
            mode: "send",
            model: ModelTier::Opus,
        }
    );
    assert_eq!(
        worker_action("CodeReviewFinalPending", "claude", 0, RecoveryState::NONE),
        WorkerAction::FinalReviewSynthetic
    );
    // Codex-owned phases.
    assert_eq!(
        worker_action("PlanParallelDrafts", "codex", 0, RecoveryState::NONE),
        WorkerAction::Codex
    );
    assert_eq!(
        worker_action("PlanCodexReviewPending", "codex", 0, RecoveryState::NONE),
        WorkerAction::Codex
    );
    assert_eq!(
        worker_action(
            "CodeReviewFixGlobalPending",
            "codex",
            0,
            RecoveryState::NONE
        ),
        WorkerAction::Codex
    );
    // Terminal.
    assert_eq!(
        worker_action("CodingComplete", "claude", 0, RecoveryState::NONE),
        WorkerAction::Terminal
    );
    assert_eq!(
        worker_action("CodingFailed", "codex", 0, RecoveryState::NONE),
        WorkerAction::Terminal
    );
    // Owner/phase mismatch is an anomaly (e.g. claude owning a codex-only phase).
    assert_eq!(
        worker_action(
            "CodeReviewFixGlobalPending",
            "claude",
            0,
            RecoveryState::NONE
        ),
        WorkerAction::Anomaly
    );
    assert_eq!(
        worker_action("CodeImplementPending", "codex", 0, RecoveryState::NONE),
        WorkerAction::Anomaly
    );
    // TEST 7: unknown owner string is always an anomaly, regardless of phase.
    assert_eq!(
        worker_action("PlanLocked", "human", 0, RecoveryState::NONE),
        WorkerAction::Anomaly
    );
    // Terminal wins regardless of owner — even an unknown owner string.
    assert_eq!(
        worker_action("CodingComplete", "nobody", 0, RecoveryState::NONE),
        WorkerAction::Terminal
    );
}

/// A recoverable (`tooling`) `failure_report` keeps the session in its CURRENT
/// phase and flips `current_owner` to the counterpart agent, who completes the
/// interrupted turn via the delegated-completion override. Those flipped pairs
/// are legitimate protocol states, not anomalies: classifying them `Anomaly`
/// drops the whole run from the corpus, so one transient `git_push_failed:`
/// would bias an A/B campaign toward whichever arm hit fewer tooling failures.
#[test]
fn recovery_flipped_pairs_dispatch_the_completing_worker() {
    // Codex reported a tooling failure at its own global-review-fix turn →
    // Claude is the recovery owner and completes it (sends `review_fix_global`).
    assert_eq!(
        worker_action(
            "CodeReviewFixGlobalPending",
            "claude",
            0,
            recovering("CodeReviewFixGlobalPending", "claude")
        ),
        WorkerAction::ClaudeRecoveryFixGlobal
    );
    // Claude reported a tooling failure at its local-review audit → Codex
    // recovery owner. The Codex shim's recovery row routes exactly this pair
    // to `collab-recovery.md`, whose `CodeReviewLocalPending` section sends
    // `review_local`, so a plain Codex turn completes the phase.
    assert_eq!(
        worker_action(
            "CodeReviewLocalPending",
            "codex",
            0,
            recovering("CodeReviewLocalPending", "codex")
        ),
        WorkerAction::Codex
    );
    // Claude recovery at CodeImplementPending (session ran
    // `--implementer=codex`) is already the normal matrix entry — the recovery
    // signal must not change it.
    assert_eq!(
        worker_action(
            "CodeImplementPending",
            "claude",
            0,
            recovering("CodeImplementPending", "claude")
        ),
        WorkerAction::ClaudeSend {
            template: "collab-turn-code-implement.md",
            mode: "send",
            model: ModelTier::Sonnet,
        }
    );
}

/// The two recovery-flipped pairs with no honest Codex turn behind them.
///
/// `CodeReviewFinalPending`: a Codex recovery owner must create a REAL PR
/// (`.codex-plugin/prompts/collab-recovery.md` § `CodeReviewFinalPending`:
/// "create the ready PR" … "Never fabricate a URL"), which the
/// synthetic-`pr_url` driver never does — a THIS-harness limitation.
///
/// `CodeImplementPending`: a Codex recovery owner here means the session's
/// implementer is `claude`, and no Codex prompt covers that turn at all.
/// `.codex-plugin/commands/collab.md` routes `CodeImplementPending` only when
/// `implementer == "codex"`, and its recovery row covers only the two review
/// phases; `collab-recovery.md` is explicitly "only for a recoverable
/// `CodeReviewLocalPending` or `CodeReviewFinalPending` turn" and exits
/// otherwise. Dispatching Codex would burn a turn that reports status and
/// exits — no completion event, no failure report — and the run would die
/// later as an opaque stall. This is a protocol-wide gap, not a harness one.
///
/// Both stay `Anomaly` deliberately: stop and name the pair.
#[test]
fn codex_recovery_without_a_routable_prompt_stays_anomaly() {
    assert_eq!(
        worker_action(
            "CodeReviewFinalPending",
            "codex",
            0,
            recovering("CodeReviewFinalPending", "codex")
        ),
        WorkerAction::Anomaly
    );
    assert_eq!(
        worker_action(
            "CodeImplementPending",
            "codex",
            0,
            recovering("CodeImplementPending", "codex")
        ),
        WorkerAction::Anomaly
    );
}

/// `Anomaly` keeps meaning "should not occur": the recovery signal only
/// licenses the exact flipped pair the state machine created.
#[test]
fn recovery_signal_does_not_become_a_catch_all() {
    // Recovery in flight for a DIFFERENT pair → normal matrix applies.
    assert_eq!(
        worker_action(
            "CodeReviewFixGlobalPending",
            "claude",
            0,
            recovering("CodeReviewLocalPending", "codex")
        ),
        WorkerAction::Anomaly
    );
    // Phase matches but the owner is not the recovery owner → normal matrix.
    assert_eq!(
        worker_action(
            "CodeImplementPending",
            "codex",
            0,
            recovering("CodeImplementPending", "claude")
        ),
        WorkerAction::Anomaly
    );
    // Recovery pointers set but no failure pending (a cleared/stale row) → the
    // delegated-completion override is NOT active on the server either.
    assert_eq!(
        worker_action(
            "CodeReviewFixGlobalPending",
            "claude",
            0,
            RecoveryState {
                pending_failure: None,
                recovery_phase: Some("CodeReviewFixGlobalPending"),
                recovery_owner: Some("claude"),
            }
        ),
        WorkerAction::Anomaly
    );
    // An unknown owner string is impossible, recovery or not.
    assert_eq!(
        worker_action(
            "CodeImplementPending",
            "human",
            0,
            recovering("CodeImplementPending", "human")
        ),
        WorkerAction::Anomaly
    );
    // A recovery cannot exist outside the coding-active phases; if one is
    // somehow claimed at a planning phase, the normal matrix still rules.
    assert_eq!(
        worker_action(
            "PlanCodexReviewPending",
            "claude",
            0,
            recovering("PlanCodexReviewPending", "claude")
        ),
        WorkerAction::Anomaly
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
        model_of(worker_action(
            "PlanParallelDrafts",
            "claude",
            0,
            RecoveryState::NONE
        )),
        ModelTier::Opus
    );
    assert_eq!(
        model_of(worker_action(
            "PlanSynthesisPending",
            "claude",
            0,
            RecoveryState::NONE
        )),
        ModelTier::Opus
    );
    assert_eq!(
        model_of(worker_action(
            "PlanClaudeFinalizePending",
            "claude",
            0,
            RecoveryState::NONE
        )),
        ModelTier::Opus
    );
    // Review turn → opus.
    assert_eq!(
        model_of(worker_action(
            "CodeReviewLocalPending",
            "claude",
            0,
            RecoveryState::NONE
        )),
        ModelTier::Opus
    );
    // Implementation → sonnet (the one Claude work turn that drops a tier).
    assert_eq!(
        model_of(worker_action(
            "CodeImplementPending",
            "claude",
            0,
            RecoveryState::NONE
        )),
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

#[test]
fn template_render_rejects_leftover_placeholder() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("t.md"),
        "collab_send(sender=\"$SENDER\") for session_id=$SESSION_ID",
    )
    .unwrap();
    // The exact shape of the bug this guards: the template gained `$SENDER`,
    // the call site was never updated. Must be a hard error, not a prompt with
    // a literal `$SENDER` shipped to a live `claude -p` worker.
    let err = render_worker_prompt(dir.path(), "t.md", &[("$SESSION_ID", "s1")])
        .expect_err("leftover $SENDER must fail the render");
    let msg = err.to_string();
    assert!(msg.contains("t.md"), "error must name the template: {msg}");
    assert!(
        msg.contains("$SENDER"),
        "error must name the leftover: {msg}"
    );

    // Supplying it renders clean.
    let ok = render_worker_prompt(
        dir.path(),
        "t.md",
        &[("$SESSION_ID", "s1"), ("$SENDER", "codex")],
    )
    .unwrap();
    assert_eq!(ok, "collab_send(sender=\"codex\") for session_id=s1");
}

#[test]
fn template_render_allows_non_placeholder_dollar_text() {
    let dir = tempfile::tempdir().unwrap();
    // `$NAME=` is the orchestrator's contract notation naming ANOTHER template's
    // input (`collab-turn-final-review.md` documents `$TOPIC=final_review` and
    // `$ARTIFACT_REF=<drawer_id>`); `${...}`, `$1` and a bare `$` are ordinary
    // shell/regex text. None of these are unsubstituted placeholders.
    let body = "call submit with `$TOPIC=final` and `$ARTIFACT_REF=<drawer_id>`; \
                run `echo ${session_id}` and `$1`; costs $5; anchor /x$/ for $SESSION_ID";
    fs::write(dir.path().join("t.md"), body).unwrap();
    let out = render_worker_prompt(dir.path(), "t.md", &[("$SESSION_ID", "s1")]).unwrap();
    assert!(out.ends_with("for s1"));
}

/// Point at the real repo templates so the render sees what a live run sees.
fn repo_prompts_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".claude-plugin/prompts")
}

#[test]
fn real_submit_template_renders_with_a_concrete_sender() {
    // Mirrors the driver's `ClaudeCompose` submit call site exactly. Renders the
    // REAL `.claude-plugin/prompts/collab-turn-submit.md`, which the live driver
    // feeds to a `claude -p` subprocess: a surviving `$SENDER` trips the
    // template's own "if `$SENDER` … does not equal `current_owner`, ABORT"
    // guard, and the aborted submit still reports success — the run then wedges
    // on STUCK_LIMIT instead of failing loudly.
    for owner in ["claude", "codex"] {
        let out = render_worker_prompt(
            &repo_prompts_dir(),
            "collab-turn-submit.md",
            &[
                ("$SESSION_ID", "sess-xyz"),
                ("$BRANCH", "abeval/task1"),
                ("$MODE", "submit"),
                ("$TOPIC", "final"),
                ("$ARTIFACT_REF", "drawer-1"),
                ("$SENDER", owner),
            ],
        )
        .unwrap();
        assert!(
            !out.contains("$SENDER"),
            "rendered submit prompt still contains a literal $SENDER"
        );
        assert!(
            out.contains(&format!("sender=\"{owner}\"")),
            "rendered submit prompt must send as the session's current owner"
        );
    }
}

#[test]
fn every_driver_rendered_template_has_no_leftover_placeholders() {
    // The driver's full render surface, each with the substitution list its call
    // site supplies. Any template that gains a placeholder the driver does not
    // pass fails here instead of in a live run.
    let dir = repo_prompts_dir();
    let send = |t: &str| {
        render_worker_prompt(
            &dir,
            t,
            &[
                ("$SESSION_ID", "sess-xyz"),
                ("$BRANCH", "abeval/task1"),
                ("$MODE", "send"),
                ("$SENDER", "claude"),
            ],
        )
    };
    for t in [
        "collab-turn-plan-draft.md",
        "collab-turn-plan-synthesis.md",
        "collab-turn-code-implement.md",
        "collab-turn-review-local.md",
    ] {
        send(t).unwrap_or_else(|e| panic!("{t}: {e}"));
    }
    render_worker_prompt(
        &dir,
        "collab-turn-plan-finalize.md",
        &[
            ("$SESSION_ID", "sess-xyz"),
            ("$BRANCH", "abeval/task1"),
            ("$MODE", "compose"),
            ("$TOPIC", "final"),
            ("$SENDER", "claude"),
        ],
    )
    .unwrap();
    render_worker_prompt(
        &dir,
        "collab-turn-task-list.md",
        &[
            ("$SESSION_ID", "sess-xyz"),
            ("$BRANCH", "abeval/task1"),
            ("$SENDER", "claude"),
        ],
    )
    .unwrap();
    render_worker_prompt(
        &dir,
        "collab-turn-final-review.md",
        &[
            ("$SESSION_ID", "sess-xyz"),
            ("$BRANCH", "abeval/task1"),
            ("$MODE", "compose"),
            ("$SENDER", "claude"),
        ],
    )
    .unwrap();
}

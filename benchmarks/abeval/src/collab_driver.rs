//! Headless collab driver: reproduces the interactive `/collab` dispatcher loop
//! (`.claude-plugin/commands/collab.md`) against a live per-task collab session,
//! with two injected seams so the loop is unit-tested with fakes:
//! [`CollabStateReader`] (DB poll) and [`WorkerSpawner`] (claude/codex spawn).

use std::cmp::Reverse;
use std::path::Path;

use anyhow::{anyhow, Result};

/// One dispatch decision for a `(phase, owner)` poll. Frozen mirror of the
/// design-spec §4.3 table / `collab.md` owner-first dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerAction {
    /// A single Claude turn that sends its phase event directly (no compose/submit
    /// split). `mode` is substituted into `$MODE` (e.g. plan-synthesis revision).
    ClaudeSend { template: &'static str, mode: &'static str },
    /// A Claude compose turn (writes an artifact, returns a `ref:`), then an
    /// auto-approved `collab-turn-submit.md` send of `topic` by that ref.
    ClaudeCompose { template: &'static str, topic: &'static str },
    /// A Codex turn (`codex exec ... join <session>`); usage attributed later.
    Codex,
    /// The final-review compose + a driver-owned synthetic-`pr_url` submit (no
    /// `gh pr create`, nothing pushed).
    FinalReviewSynthetic,
    /// Terminal phase: stop the loop.
    Terminal,
    /// Owner/phase combination that should not occur — stop and surface.
    Anomaly,
}

/// Map a `(phase, owner, global_review_round)` poll to a [`WorkerAction`].
pub fn worker_action(phase: &str, owner: &str, global_review_round: u32) -> WorkerAction {
    if matches!(phase, "CodingComplete" | "CodingFailed") {
        return WorkerAction::Terminal;
    }
    match owner {
        "codex" => match phase {
            "PlanParallelDrafts" | "PlanCodexReviewPending" | "CodeReviewFixGlobalPending" => {
                WorkerAction::Codex
            }
            _ => WorkerAction::Anomaly,
        },
        "claude" => match phase {
            "PlanParallelDrafts" => WorkerAction::ClaudeSend {
                template: "collab-turn-plan-draft.md",
                mode: "send",
            },
            "PlanSynthesisPending" => {
                if global_review_round == 0 {
                    WorkerAction::ClaudeCompose {
                        template: "collab-turn-plan-synthesis.md",
                        topic: "canonical",
                    }
                } else {
                    WorkerAction::ClaudeSend {
                        template: "collab-turn-plan-synthesis.md",
                        mode: "send",
                    }
                }
            }
            "PlanClaudeFinalizePending" => WorkerAction::ClaudeCompose {
                template: "collab-turn-plan-finalize.md",
                topic: "final",
            },
            "PlanLocked" => WorkerAction::ClaudeCompose {
                template: "collab-turn-task-list.md",
                topic: "task_list",
            },
            "CodeImplementPending" => WorkerAction::ClaudeSend {
                template: "collab-turn-code-implement.md",
                mode: "send",
            },
            "CodeReviewLocalPending" => WorkerAction::ClaudeSend {
                template: "collab-turn-review-local.md",
                mode: "send",
            },
            "CodeReviewFinalPending" => WorkerAction::FinalReviewSynthetic,
            _ => WorkerAction::Anomaly,
        },
        _ => WorkerAction::Anomaly,
    }
}

/// Extract the `ref:` value from a worker's ≤3-line verdict. `none` / absent → None.
pub fn parse_ref_line(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix("ref:") {
            let v = rest.trim();
            return if v.is_empty() || v.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(v.to_string())
            };
        }
    }
    None
}

/// Read the `ABEVAL_SESSION_ID=<id>` line the bootstrap worker prints.
pub fn parse_session_id(stdout: &str) -> Result<String> {
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix("ABEVAL_SESSION_ID=") {
            let id = rest.trim();
            if !id.is_empty() {
                return Ok(id.to_string());
            }
        }
    }
    Err(anyhow!(
        "bootstrap output did not contain an ABEVAL_SESSION_ID=<id> line"
    ))
}

/// Render a worker template by reading `<prompts_dir>/<template>` and replacing
/// each `$VAR` in `subst`. Keys are applied longest-first so that a prefix key
/// (`$ARTIFACT_HASH`) cannot clobber a longer one (`$ARTIFACT_REF`).
pub fn render_worker_prompt(
    prompts_dir: &Path,
    template: &str,
    subst: &[(&str, &str)],
) -> Result<String> {
    let path = prompts_dir.join(template);
    let mut body = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("reading worker template {}: {e}", path.display()))?;
    let mut keys: Vec<&(&str, &str)> = subst.iter().collect();
    keys.sort_by_key(|(k, _)| Reverse(k.len()));
    for (k, v) in keys {
        body = body.replace(k, v);
    }
    Ok(body)
}

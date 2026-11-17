//! Read-only status of one repo's unattended-route prerequisites (#420).
//!
//! `ironmem autopilot status <owner/repo>` answers two questions the
//! `/evaluate-issue` router asks before it recommends GOAL or AUTOPILOT: does
//! the repo have an approved gate config, and does it have the labels
//! Autopilot needs. Before this module the router answered the first by
//! reading Autopilot's drawer directly, which tied prose instructions to a
//! storage layout.
//!
//! # Read-only by construction
//!
//! The gate config is read through [`ReadOnlyDb`], a type with no write or
//! migrate method, over a connection opened `SQLITE_OPEN_READ_ONLY`. The
//! labels are read with one `gh label list`. Nothing here can create a
//! drawer, a label or a database file.
//!
//! # A failed read is never a missing label
//!
//! [`LabelStatus`] is an enum, not a struct of optional fields: when `gh`
//! could not be started, exited non-zero, or returned a list that may be
//! truncated, the status is [`LabelStatus::Failed`] and carries no label
//! answers at all. Its JSON has no `agent` or `risk` key, so a consumer
//! cannot read "we do not know" as "the label is not there".

use std::collections::BTreeMap;

use serde::Serialize;

use crate::db::read_only::ReadOnlyDb;
use crate::error::MemoryError;

use super::gate_config::{get_gate_config_read_only, GateConfigState};
use super::gh::{repo_labels, GhRunner};
use super::labels::AgentLabel;
use super::lead::RISK_LABEL_PREFIX;
use super::validate_repo;

/// Everything `ironmem autopilot status` reports for one repo. Its
/// serialized form is the command's `--json` output, a contract the
/// `/evaluate-issue` router reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoStatus {
    pub repo: String,
    /// Whether a gate config exists at all, in any state. Always equal to
    /// `gate_config.is_some()`; stated so a reader need not infer it.
    pub onboarded: bool,
    /// `None` (JSON `null`) for a repo that was never onboarded.
    pub gate_config: Option<GateConfigStatus>,
    pub labels: LabelStatus,
}

impl RepoStatus {
    /// The one place `onboarded` is derived, so it cannot disagree with
    /// `gate_config`.
    pub fn new(repo: &str, gate_config: Option<GateConfigStatus>, labels: LabelStatus) -> Self {
        Self {
            repo: repo.to_string(),
            onboarded: gate_config.is_some(),
            gate_config,
            labels,
        }
    }
}

/// The reported part of a stored gate config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GateConfigStatus {
    pub state: GateConfigState,
    /// Never empty and never holding a blank entry: a stored config that
    /// breaks that rule fails to read instead of being reported.
    pub gate_commands: Vec<String>,
    pub proposed_at: String,
    /// `None` (JSON `null`) while the config is still pending.
    pub approved_at: Option<String>,
}

/// What the repo's labels are, or that they could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "read", rename_all = "snake_case")]
pub enum LabelStatus {
    /// The label list was read in full.
    Ok {
        /// One entry per [`AgentLabel::ALL`] label, keyed by its GitHub name,
        /// `true` when the repo has it.
        agent: BTreeMap<&'static str, bool>,
        /// The repo's `risk:<class>` labels as spelled on GitHub, sorted.
        risk: Vec<String>,
    },
    /// The label list could not be read. Says nothing about any label.
    Failed { error: String },
}

impl LabelStatus {
    /// Read `repo`'s labels. Never an `Err`: every failure is
    /// [`LabelStatus::Failed`], so one unreadable half of the report does
    /// not hide the other.
    pub fn read(gh: &mut dyn GhRunner, repo: &str) -> Self {
        match repo_labels(gh, repo) {
            Ok(names) => Self::from_names(&names),
            Err(err) => Self::failed(err.to_string()),
        }
    }

    /// A read that failed before `gh` could be asked — for instance when no
    /// `gh` binary is on `PATH`.
    pub fn failed(error: impl Into<String>) -> Self {
        Self::Failed {
            error: error.into(),
        }
    }

    /// Match the repo's label names exactly, never by search.
    fn from_names(names: &[String]) -> Self {
        // Driven by `AgentLabel::ALL`, so a new variant cannot be left out
        // of the report.
        let agent = AgentLabel::ALL
            .into_iter()
            .map(|label| {
                let present = names
                    .iter()
                    .any(|name| AgentLabel::from_label_str(name) == Some(label));
                (label.as_str(), present)
            })
            .collect();
        let mut risk: Vec<String> = names
            .iter()
            .filter(|name| is_risk_label(name))
            .cloned()
            .collect();
        risk.sort();
        Self::Ok { agent, risk }
    }
}

/// Whether `name` is a `risk:<class>` label: the prefix, compared the way
/// [`super::queue`] compares an issue's labels (trimmed, case-insensitive),
/// followed by a non-empty class.
fn is_risk_label(name: &str) -> bool {
    name.trim()
        .to_ascii_lowercase()
        .strip_prefix(RISK_LABEL_PREFIX)
        .is_some_and(|class| !class.is_empty())
}

/// Read `repo`'s gate config, whatever state it is in. `Ok(None)` is a repo
/// that was never onboarded; an `Err` is a status that could not be read (an
/// invalid repo string, or a stored config that does not parse).
pub fn gate_config_status(
    db: &ReadOnlyDb,
    repo: &str,
) -> Result<Option<GateConfigStatus>, MemoryError> {
    validate_repo(repo)?;
    Ok(
        get_gate_config_read_only(db, repo)?.map(|config| GateConfigStatus {
            state: config.state,
            gate_commands: config.gate_commands().to_vec(),
            proposed_at: config.proposed_at,
            approved_at: config.approved_at,
        }),
    )
}

/// The human-readable report.
pub fn render_text(status: &RepoStatus) -> String {
    let repo = &status.repo;
    let mut out = String::new();
    match &status.gate_config {
        None => out.push_str(&format!(
            "{repo}: not onboarded — no gate config. Run `ironmem autopilot onboard {repo} \
             --path <checkout>` first.\n"
        )),
        Some(config) => {
            let state = match config.state {
                GateConfigState::Pending => "pending",
                GateConfigState::Approved => "approved",
            };
            out.push_str(&format!("{repo}: gate config {state}\n"));
            for command in &config.gate_commands {
                out.push_str(&format!("  - {command}\n"));
            }
            out.push_str(&format!("  proposed: {}\n", config.proposed_at));
            match &config.approved_at {
                Some(at) => out.push_str(&format!("  approved: {at}\n")),
                None => out.push_str(&format!(
                    "  approved: not yet — run `ironmem autopilot approve {repo}`\n"
                )),
            }
        }
    }
    match &status.labels {
        LabelStatus::Failed { error } => {
            out.push_str(&format!("Labels: could not be read — {error}\n"));
        }
        LabelStatus::Ok { agent, risk } => {
            out.push_str("Labels:\n");
            for label in AgentLabel::ALL {
                let present = agent.get(label.as_str()).copied().unwrap_or(false);
                let word = if present { "present" } else { "MISSING" };
                out.push_str(&format!("  {} — {word}\n", label.as_str()));
            }
            if risk.is_empty() {
                out.push_str("  risk:* — none\n");
            } else {
                out.push_str(&format!("  risk:* — {}\n", risk.join(", ")));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::gate_config::{approve_gate_config, propose_gate_config};
    use crate::autopilot::gh::testing::ScriptedGh;
    use crate::autopilot::gh::{label_list_argv, GhOutput};
    use crate::autopilot::testing::on_disk_db;
    use crate::db::schema::Database;

    fn label_json(names: &[&str]) -> String {
        let rows: Vec<serde_json::Value> = names
            .iter()
            .map(|name| serde_json::json!({ "name": name }))
            .collect();
        serde_json::to_string(&rows).unwrap()
    }

    fn read_labels(names: &[&str]) -> LabelStatus {
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::ok(&label_json(names)))]);
        let status = LabelStatus::read(&mut gh, "owner/repo");
        assert_eq!(gh.seen, vec![label_list_argv("owner/repo")]);
        status
    }

    fn agent_map(ready: bool, blocked: bool, exhausted: bool) -> BTreeMap<&'static str, bool> {
        BTreeMap::from([
            ("agent:ready", ready),
            ("agent:blocked", blocked),
            ("agent:exhausted", exhausted),
        ])
    }

    // ── labels ──────────────────────────────────────────────────────────

    #[test]
    fn a_repo_with_every_label_reports_each_present_and_its_risk_labels_sorted() {
        let status = read_labels(&[
            "bug",
            "agent:exhausted",
            "risk:logic",
            "agent:ready",
            "risk:documentation",
            "agent:blocked",
        ]);
        assert_eq!(
            status,
            LabelStatus::Ok {
                agent: agent_map(true, true, true),
                risk: vec!["risk:documentation".into(), "risk:logic".into()],
            }
        );
    }

    #[test]
    fn a_missing_agent_label_is_reported_false_not_left_out() {
        let status = read_labels(&["agent:ready", "agent:blocked"]);
        assert_eq!(
            status,
            LabelStatus::Ok {
                agent: agent_map(true, true, false),
                risk: vec![],
            }
        );
    }

    #[test]
    fn labels_match_by_exact_name_not_by_resemblance() {
        // Everything a fuzzy `--search "agent:"` or `--search "risk:"` could
        // return without being the label asked about.
        let status = read_labels(&[
            "agent:ready-ish",
            "not-agent:ready",
            "agent:something-else",
            "sev:critical",
            "xrisk:logic",
            "risk:",
            "risky",
        ]);
        assert_eq!(
            status,
            LabelStatus::Ok {
                agent: agent_map(false, false, false),
                risk: vec![],
            }
        );
    }

    #[test]
    fn a_differently_cased_label_is_the_same_label() {
        // GitHub label names are case-insensitive for uniqueness, and
        // `AgentLabel::from_label_str` already reads them that way.
        let status = read_labels(&["Agent:Ready", "Risk:Logic"]);
        assert_eq!(
            status,
            LabelStatus::Ok {
                agent: agent_map(true, false, false),
                risk: vec!["Risk:Logic".into()],
            }
        );
    }

    #[test]
    fn a_refused_gh_is_a_failed_read_that_carries_the_error() {
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::failed(
            "",
            "HTTP 401: Bad credentials (https://api.github.com/graphql)",
        ))]);

        let status = LabelStatus::read(&mut gh, "owner/repo");

        match status {
            LabelStatus::Failed { error } => {
                assert!(error.contains("gh label list on owner/repo"), "{error}");
                assert!(error.contains("HTTP 401: Bad credentials"), "{error}");
            }
            other => panic!("a refused gh must be a failed read, got {other:?}"),
        }
    }

    #[test]
    fn a_gh_that_cannot_start_is_a_failed_read() {
        let mut gh = ScriptedGh::new(vec![Err(MemoryError::NotFound(
            "failed to start gh label list".into(),
        ))]);
        let status = LabelStatus::read(&mut gh, "owner/repo");
        assert_eq!(
            status,
            LabelStatus::failed("Not found: failed to start gh label list")
        );
    }

    #[test]
    fn an_ok_read_serializes_the_pinned_shape() {
        let status = read_labels(&["agent:ready", "risk:logic"]);
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            serde_json::json!({
                "read": "ok",
                "agent": {
                    "agent:ready": true,
                    "agent:blocked": false,
                    "agent:exhausted": false,
                },
                "risk": ["risk:logic"],
            })
        );
    }

    #[test]
    fn a_failed_read_serializes_with_no_label_answers_at_all() {
        // No `agent` and no `risk` key: "could not read" must not be
        // readable as "missing".
        assert_eq!(
            serde_json::to_value(LabelStatus::failed("boom")).unwrap(),
            serde_json::json!({ "read": "failed", "error": "boom" })
        );
    }

    // ── gate config ─────────────────────────────────────────────────────

    #[test]
    fn an_approved_config_reports_its_state_commands_and_both_timestamps() {
        let (_dir, path) = on_disk_db();
        let stored = {
            let db = Database::open(&path).unwrap();
            propose_gate_config(
                &db,
                "owner/repo",
                vec!["cargo fmt --all -- --check".into(), "cargo test".into()],
                vec![],
            )
            .unwrap();
            approve_gate_config(&db, "owner/repo").unwrap()
        };

        let ro = Database::open_read_only(&path).unwrap();
        let status = gate_config_status(&ro, "owner/repo").unwrap().unwrap();

        assert_eq!(
            status,
            GateConfigStatus {
                state: GateConfigState::Approved,
                gate_commands: vec!["cargo fmt --all -- --check".into(), "cargo test".into()],
                proposed_at: stored.proposed_at.clone(),
                approved_at: stored.approved_at.clone(),
            }
        );
        assert!(status.approved_at.is_some());
    }

    #[test]
    fn a_pending_config_reports_pending_with_no_approval_time() {
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            propose_gate_config(&db, "owner/repo", vec!["cargo test".into()], vec![]).unwrap();
        }

        let ro = Database::open_read_only(&path).unwrap();
        let status = gate_config_status(&ro, "owner/repo").unwrap().unwrap();

        assert_eq!(status.state, GateConfigState::Pending);
        assert_eq!(status.approved_at, None);
    }

    #[test]
    fn a_repo_never_onboarded_has_no_gate_config_status() {
        let (_dir, path) = on_disk_db();
        let ro = Database::open_read_only(&path).unwrap();
        assert_eq!(gate_config_status(&ro, "owner/repo").unwrap(), None);
    }

    #[test]
    fn an_empty_repo_string_is_refused() {
        let (_dir, path) = on_disk_db();
        let ro = Database::open_read_only(&path).unwrap();
        let err = gate_config_status(&ro, "  ").unwrap_err();
        assert!(matches!(err, MemoryError::Validation(_)), "{err}");
    }

    // ── the report ──────────────────────────────────────────────────────

    #[test]
    fn onboarded_follows_the_gate_config() {
        let labels = LabelStatus::failed("boom");
        assert!(!RepoStatus::new("owner/repo", None, labels.clone()).onboarded);
        let config = GateConfigStatus {
            state: GateConfigState::Pending,
            gate_commands: vec!["cargo test".into()],
            proposed_at: "2026-01-01T00:00:00+00:00".into(),
            approved_at: None,
        };
        assert!(RepoStatus::new("owner/repo", Some(config), labels).onboarded);
    }

    #[test]
    fn a_repo_that_is_not_onboarded_serializes_a_null_gate_config() {
        let status = RepoStatus::new("owner/repo", None, LabelStatus::failed("boom"));
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            serde_json::json!({
                "repo": "owner/repo",
                "onboarded": false,
                "gate_config": null,
                "labels": { "read": "failed", "error": "boom" },
            })
        );
    }

    #[test]
    fn the_text_report_says_not_onboarded_and_that_labels_were_not_read() {
        let text = render_text(&RepoStatus::new(
            "owner/repo",
            None,
            LabelStatus::failed("gh label list on owner/repo failed (exit Some(1)): HTTP 401"),
        ));
        assert!(text.contains("owner/repo: not onboarded"), "{text}");
        assert!(
            text.contains("ironmem autopilot onboard owner/repo"),
            "{text}"
        );
        assert!(
            text.contains("Labels: could not be read — gh label list"),
            "{text}"
        );
        assert!(
            !text.contains("MISSING"),
            "a failed read names no label: {text}"
        );
    }

    #[test]
    fn the_text_report_lists_the_gate_and_marks_a_missing_label() {
        let text = render_text(&RepoStatus::new(
            "owner/repo",
            Some(GateConfigStatus {
                state: GateConfigState::Pending,
                gate_commands: vec!["cargo test".into()],
                proposed_at: "2026-01-01T00:00:00+00:00".into(),
                approved_at: None,
            }),
            read_labels(&["agent:ready", "agent:blocked", "risk:logic"]),
        ));
        assert!(text.contains("owner/repo: gate config pending"), "{text}");
        assert!(text.contains("  - cargo test"), "{text}");
        assert!(
            text.contains("ironmem autopilot approve owner/repo"),
            "{text}"
        );
        assert!(text.contains("  agent:ready — present"), "{text}");
        assert!(text.contains("  agent:exhausted — MISSING"), "{text}");
        assert!(text.contains("  risk:* — risk:logic"), "{text}");
    }
}

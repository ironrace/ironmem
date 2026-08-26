//! Approved gate config — `logical_key` per repo, `pending` → `approved`
//! (spec's *Repo onboarding* section).
//!
//! This module implements only the storage/state-transition half of that
//! section: writing a proposed config and flipping it to approved. Inferring
//! the gate commands from a repo's build manifests is [`super::onboard`]
//! (rung 3's Onboarder); nothing here inspects a repo at all.

use serde::{Deserialize, Serialize};

use crate::db::schema::Database;
use crate::error::MemoryError;

use super::{read_current, repo_slug, validate_repo, write_current};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateConfigState {
    Pending,
    Approved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateConfig {
    pub repo: String,
    pub state: GateConfigState,
    pub gate_commands: Vec<String>,
    pub proposed_at: String,
    pub approved_at: Option<String>,
}

fn gate_config_key(repo: &str) -> String {
    format!("gate-config:{}", repo_slug(repo))
}

/// Write a fresh `pending` proposal for `repo`, as the Onboarder would after
/// inspecting its build manifests. Always writes `pending`, even if an
/// `approved` config already exists — a re-run onboard is a new proposal a
/// human must re-approve, not a silent no-op against an already-trusted
/// config.
///
/// Rejects an empty `gate_commands`: this is the storage boundary every
/// caller goes through — including a human bypassing [`super::onboard`]'s
/// inference entirely to supply commands read out of CI config by hand (see
/// that module's docs) — so it must not rely on [`super::onboard`] being the
/// only caller that happens to already refuse an empty list. Without this
/// check here, an empty-gate proposal would sail through `pending` →
/// `approved` and only fail much later, as a panic inside
/// `turn_prompt::render` at actual dispatch time.
pub fn propose_gate_config(
    db: &Database,
    repo: &str,
    gate_commands: Vec<String>,
) -> Result<GateConfig, MemoryError> {
    validate_repo(repo)?;
    if gate_commands.is_empty() {
        return Err(MemoryError::Validation(
            "gate_commands must not be empty — a dispatch needs a real gate to satisfy".into(),
        ));
    }
    let config = GateConfig {
        repo: repo.to_string(),
        state: GateConfigState::Pending,
        gate_commands,
        proposed_at: chrono::Utc::now().to_rfc3339(),
        approved_at: None,
    };
    let content = serde_json::to_string(&config)?;
    write_current(db, &gate_config_key(repo), &content)?;
    Ok(config)
}

/// Flip `repo`'s config from `pending` to `approved`. Idempotent if it's
/// already approved. Errors if no config has ever been proposed — there is
/// nothing for a human to be approving.
pub fn approve_gate_config(db: &Database, repo: &str) -> Result<GateConfig, MemoryError> {
    let key = gate_config_key(repo);
    let mut config = match read_current(db, &key)? {
        Some(drawer) => serde_json::from_str::<GateConfig>(&drawer.content)?,
        None => {
            return Err(MemoryError::NotFound(format!(
                "no gate config has been proposed for repo '{repo}'"
            )))
        }
    };

    if config.state != GateConfigState::Approved {
        config.state = GateConfigState::Approved;
        config.approved_at = Some(chrono::Utc::now().to_rfc3339());
        let content = serde_json::to_string(&config)?;
        write_current(db, &key, &content)?;
    }
    Ok(config)
}

/// Read `repo`'s current gate config, whatever state it's in.
pub fn get_gate_config(db: &Database, repo: &str) -> Result<Option<GateConfig>, MemoryError> {
    match read_current(db, &gate_config_key(repo))? {
        None => Ok(None),
        Some(drawer) => Ok(Some(serde_json::from_str(&drawer.content)?)),
    }
}

/// Whether `repo` currently has an `approved` gate config — the check the
/// spec says the Lead must pass before dispatching into a repo at all ("The
/// Lead refuses to dispatch into any repo without an approved config"). A
/// repo with no config at all, or one still `pending`, is not eligible.
pub fn is_gate_config_approved(db: &Database, repo: &str) -> Result<bool, MemoryError> {
    Ok(matches!(
        get_gate_config(db, repo)?,
        Some(config) if config.state == GateConfigState::Approved
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propose_gate_config_rejects_an_empty_gate_commands_list() {
        // The storage boundary must refuse this itself — not rely on
        // callers (e.g. `onboard::infer_gate_commands`) to have already
        // filtered it out — since the module's own docs invite a human to
        // call this directly, bypassing that inference path entirely.
        let db = Database::open_in_memory().unwrap();
        assert!(propose_gate_config(&db, "ironmem", vec![]).is_err());
        assert!(get_gate_config(&db, "ironmem").unwrap().is_none());
    }

    #[test]
    fn work_is_refused_on_a_pending_unapproved_repo_config() {
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(&db, "ironmem", vec!["cargo test --workspace".into()]).unwrap();

        assert!(!is_gate_config_approved(&db, "ironmem").unwrap());
        assert_eq!(
            get_gate_config(&db, "ironmem").unwrap().unwrap().state,
            GateConfigState::Pending
        );
    }

    #[test]
    fn approve_flips_pending_to_approved() {
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(&db, "ironmem", vec!["cargo test --workspace".into()]).unwrap();

        let approved = approve_gate_config(&db, "ironmem").unwrap();
        assert_eq!(approved.state, GateConfigState::Approved);
        assert!(approved.approved_at.is_some());
        assert!(is_gate_config_approved(&db, "ironmem").unwrap());
    }

    #[test]
    fn approve_is_idempotent() {
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(&db, "ironmem", vec!["cargo test".into()]).unwrap();
        let first = approve_gate_config(&db, "ironmem").unwrap();
        let second = approve_gate_config(&db, "ironmem").unwrap();
        assert_eq!(first.approved_at, second.approved_at);
    }

    #[test]
    fn approve_without_a_prior_proposal_errors() {
        let db = Database::open_in_memory().unwrap();
        assert!(approve_gate_config(&db, "never-onboarded").is_err());
    }

    #[test]
    fn no_config_at_all_is_not_approved() {
        let db = Database::open_in_memory().unwrap();
        assert!(!is_gate_config_approved(&db, "unknown-repo").unwrap());
    }

    #[test]
    fn repo_with_a_slash_is_slugged_safely_in_the_logical_key() {
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(&db, "ironrace/ironmem", vec!["cargo test".into()]).unwrap();
        let config = get_gate_config(&db, "ironrace/ironmem").unwrap().unwrap();
        assert_eq!(config.repo, "ironrace/ironmem");
    }
}

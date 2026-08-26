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

use super::{read_current, repo_slug, validate_repo, write_current, EMPTY_GATE_COMMANDS_MSG};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateConfigState {
    Pending,
    Approved,
}

/// Whether `gate_commands` is a real, satisfiable gate: non-empty, with no
/// empty/whitespace-only entry. Shared by [`propose_gate_config`]'s explicit
/// check (the only write path) and [`GateConfig`]'s deserialize guard (every
/// read path) — one definition of "not a real gate", enforced everywhere a
/// `GateConfig` can come into existence, not just the one function that
/// happens to be the intended caller today.
fn validate_gate_commands(gate_commands: &[String]) -> Result<(), String> {
    if gate_commands.is_empty() || gate_commands.iter().any(|cmd| cmd.trim().is_empty()) {
        return Err(EMPTY_GATE_COMMANDS_MSG.to_string());
    }
    Ok(())
}

/// A proposed or approved gate config for one repo.
///
/// `gate_commands` is private with a [`GateConfig::gate_commands`] accessor
/// rather than a public field: the invariant that it's non-empty with no
/// blank entry must hold for *every* `GateConfig` that exists, not just ones
/// built via [`propose_gate_config`]'s explicit check. A public field would
/// let some future write path (a bulk import, a migration/restore tool, a
/// hand-edited row) construct or deserialize a `GateConfig` that skips that
/// check entirely — the vacuous-gate condition would then surface only much
/// later, as `turn_prompt::render`'s panic at actual dispatch time. Making
/// the field private and routing deserialization through
/// `TryFrom<GateConfigShadow>` below closes that gap structurally: there is
/// no way to obtain a `GateConfig` — construction *or* deserialization —
/// without passing the same check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "GateConfigShadow")]
pub struct GateConfig {
    pub repo: String,
    pub state: GateConfigState,
    gate_commands: Vec<String>,
    /// Non-fatal problems the Onboarder hit while inferring `gate_commands`
    /// — e.g. a build manifest that exists but couldn't be read or parsed,
    /// encountered while a *different* stack was still recognized (see
    /// `onboard`'s module docs: one broken manifest must not veto a
    /// different stack's real command — but it must not vanish without a
    /// trace either, which is what this field closes). Always empty for a
    /// config proposed by hand via [`propose_gate_config`] directly. A human
    /// approving a `pending` config should read these first: an inferred
    /// gate that looks complete may be silently missing a stack whose
    /// manifest was broken.
    pub manifest_warnings: Vec<String>,
    pub proposed_at: String,
    pub approved_at: Option<String>,
}

impl GateConfig {
    /// The repo's gate commands. Guaranteed non-empty with no blank entry —
    /// see the struct doc's rationale for why this is a method over a public
    /// field.
    pub fn gate_commands(&self) -> &[String] {
        &self.gate_commands
    }
}

/// Deserialization staging struct for [`GateConfig`] — see its
/// `#[serde(try_from = "GateConfigShadow")]` attribute. Field-for-field
/// identical to `GateConfig`, but with `gate_commands` still public so serde
/// can populate it directly; [`GateConfig`]'s `TryFrom` impl below is the
/// only place a `GateConfigShadow` ever gets turned into a real
/// `GateConfig`, and it re-runs [`validate_gate_commands`] before doing so.
#[derive(Deserialize)]
struct GateConfigShadow {
    repo: String,
    state: GateConfigState,
    gate_commands: Vec<String>,
    #[serde(default)]
    manifest_warnings: Vec<String>,
    proposed_at: String,
    approved_at: Option<String>,
}

impl TryFrom<GateConfigShadow> for GateConfig {
    type Error = String;

    fn try_from(raw: GateConfigShadow) -> Result<Self, Self::Error> {
        validate_gate_commands(&raw.gate_commands)?;
        Ok(Self {
            repo: raw.repo,
            state: raw.state,
            gate_commands: raw.gate_commands,
            manifest_warnings: raw.manifest_warnings,
            proposed_at: raw.proposed_at,
            approved_at: raw.approved_at,
        })
    }
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
/// Rejects an empty `gate_commands`, or one containing an empty/
/// whitespace-only entry: this is the storage boundary every caller goes
/// through — including a human bypassing [`super::onboard`]'s inference
/// entirely to supply commands read out of CI config by hand (see that
/// module's docs) — so it must not rely on [`super::onboard`] being the only
/// caller that happens to already refuse these. `manifest_warnings` is
/// carried through verbatim (a hand-authored proposal has none to report;
/// pass an empty vec).
pub fn propose_gate_config(
    db: &Database,
    repo: &str,
    gate_commands: Vec<String>,
    manifest_warnings: Vec<String>,
) -> Result<GateConfig, MemoryError> {
    validate_repo(repo)?;
    validate_gate_commands(&gate_commands).map_err(MemoryError::Validation)?;
    let config = GateConfig {
        repo: repo.to_string(),
        state: GateConfigState::Pending,
        gate_commands,
        manifest_warnings,
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
        assert!(propose_gate_config(&db, "ironmem", vec![], vec![]).is_err());
        assert!(get_gate_config(&db, "ironmem").unwrap().is_none());
    }

    #[test]
    fn propose_gate_config_rejects_an_empty_or_whitespace_only_entry() {
        // A non-empty vec containing an empty/blank entry is not caught by
        // the `gate_commands.is_empty()` check above, nor by
        // `turn_prompt::render`'s `!gate_commands.is_empty()` assert — it
        // would instead silently join into a malformed `" && "` condition
        // (e.g. a leading `&&`) that only fails much later, at actual
        // dispatch time.
        let db = Database::open_in_memory().unwrap();
        assert!(
            propose_gate_config(&db, "ironmem", vec!["".into(), "cargo test".into()], vec![])
                .is_err()
        );
        assert!(propose_gate_config(&db, "ironmem", vec!["   ".into()], vec![]).is_err());
        assert!(get_gate_config(&db, "ironmem").unwrap().is_none());
    }

    #[test]
    fn deserializing_a_stored_config_with_a_blank_gate_command_is_rejected() {
        // Regression guard for the structural fix: even a `GateConfig` that
        // reached storage some other way (bulk import, hand-edited row, a
        // pre-fix build's leftover row) must fail to deserialize rather than
        // silently reaching `turn_prompt::render`'s panic at dispatch time.
        let json = r#"{
            "repo": "ironmem",
            "state": "pending",
            "gate_commands": ["", "cargo test"],
            "manifest_warnings": [],
            "proposed_at": "2026-01-01T00:00:00Z",
            "approved_at": null
        }"#;
        assert!(serde_json::from_str::<GateConfig>(json).is_err());
    }

    #[test]
    fn manifest_warnings_round_trip_through_storage() {
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(
            &db,
            "ironmem",
            vec!["cargo test".into()],
            vec!["package.json could not be parsed and was skipped".into()],
        )
        .unwrap();

        let config = get_gate_config(&db, "ironmem").unwrap().unwrap();
        assert_eq!(
            config.manifest_warnings,
            vec!["package.json could not be parsed and was skipped".to_string()]
        );

        // Approving must not drop the warnings a human still needs to see.
        let approved = approve_gate_config(&db, "ironmem").unwrap();
        assert_eq!(approved.manifest_warnings.len(), 1);
    }

    #[test]
    fn work_is_refused_on_a_pending_unapproved_repo_config() {
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(
            &db,
            "ironmem",
            vec!["cargo test --workspace".into()],
            vec![],
        )
        .unwrap();

        assert!(!is_gate_config_approved(&db, "ironmem").unwrap());
        assert_eq!(
            get_gate_config(&db, "ironmem").unwrap().unwrap().state,
            GateConfigState::Pending
        );
    }

    #[test]
    fn approve_flips_pending_to_approved() {
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(
            &db,
            "ironmem",
            vec!["cargo test --workspace".into()],
            vec![],
        )
        .unwrap();

        let approved = approve_gate_config(&db, "ironmem").unwrap();
        assert_eq!(approved.state, GateConfigState::Approved);
        assert!(approved.approved_at.is_some());
        assert!(is_gate_config_approved(&db, "ironmem").unwrap());
    }

    #[test]
    fn approve_is_idempotent() {
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(&db, "ironmem", vec!["cargo test".into()], vec![]).unwrap();
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
        propose_gate_config(&db, "ironrace/ironmem", vec!["cargo test".into()], vec![]).unwrap();
        let config = get_gate_config(&db, "ironrace/ironmem").unwrap().unwrap();
        assert_eq!(config.repo, "ironrace/ironmem");
    }
}

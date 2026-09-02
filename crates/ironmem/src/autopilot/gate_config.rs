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
    /// How long one dispatch into this repo may run before it is considered
    /// wedged and killed — the spec's open question 7, and the reason it
    /// lives *here* rather than as a constant.
    ///
    /// Rev 6's own recommendation, quoted: "make the wall-clock bound a
    /// **per-repo config value** (part of the same approved gate config the
    /// `/goal` condition is generated from), not a single global constant —
    /// repos' gate suites vary by an order of magnitude or more." Rung 0's
    /// probes ran 8–20 seconds against a trivial gate; `cargo test
    /// --workspace`, a Python suite and `xcodebuild` are not in the same
    /// class as each other, let alone as that. No number derived from those
    /// probes would be a measurement of anything.
    ///
    /// So there is deliberately **no default**. `None` means unbounded, which
    /// is exactly today's behavior, and every code path that dispatches
    /// without one says so out loud rather than pretending to a bound it does
    /// not have. It is [`super::onboard`]-adjacent but not inferred: nothing
    /// in a build manifest says how long its gate takes, and guessing here
    /// would be the single-arm-probe mistake the spec's method notes warn
    /// against. A human sets it, from
    /// [`super::lineage`]'s recorded dispatch durations once there are some.
    #[serde(default)]
    pub wall_clock_timeout_secs: Option<u64>,
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
    #[serde(default)]
    wall_clock_timeout_secs: Option<u64>,
}

impl TryFrom<GateConfigShadow> for GateConfig {
    type Error = String;

    fn try_from(raw: GateConfigShadow) -> Result<Self, Self::Error> {
        validate_gate_commands(&raw.gate_commands)?;
        validate_wall_clock_timeout(raw.wall_clock_timeout_secs)?;
        Ok(Self {
            repo: raw.repo,
            state: raw.state,
            gate_commands: raw.gate_commands,
            manifest_warnings: raw.manifest_warnings,
            proposed_at: raw.proposed_at,
            approved_at: raw.approved_at,
            wall_clock_timeout_secs: raw.wall_clock_timeout_secs,
        })
    }
}

/// Whether a wall-clock bound is usable. Enforced on every path a
/// `GateConfig` can come into existence, exactly as
/// [`validate_gate_commands`] is and for the same reason.
///
/// Zero is the only rejected value, and it is rejected because it does not
/// mean "no bound" — it means "kill every dispatch the instant it starts",
/// which would burn the issue's whole attempt budget on timeouts. Absence is
/// how "no bound" is spelled.
fn validate_wall_clock_timeout(secs: Option<u64>) -> Result<(), String> {
    if secs == Some(0) {
        return Err(
            "wall_clock_timeout_secs must be at least 1 second — omit it entirely for no bound,              since 0 would kill every dispatch immediately"
                .to_string(),
        );
    }
    Ok(())
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
    // A re-proposal carries the existing wall-clock bound forward. Every
    // other field here is *inferred*, so re-running inference legitimately
    // replaces it — but the bound is not inferred from anything (see the
    // field's doc), so re-inference has nothing to say about it, and dropping
    // it would silently un-bound the repo's dispatches as a side effect of an
    // unrelated re-onboard.
    let carried = read_current(db, &gate_config_key(repo))?
        .and_then(|drawer| serde_json::from_str::<GateConfig>(&drawer.content).ok())
        .and_then(|existing| existing.wall_clock_timeout_secs);
    let config = GateConfig {
        repo: repo.to_string(),
        state: GateConfigState::Pending,
        gate_commands,
        manifest_warnings,
        proposed_at: chrono::Utc::now().to_rfc3339(),
        approved_at: None,
        wall_clock_timeout_secs: carried,
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

/// Set (or clear, with `None`) `repo`'s per-dispatch wall-clock bound.
///
/// Deliberately **not** a state transition: it does not flip an `approved`
/// config back to `pending`. Tightening or relaxing a timeout does not change
/// what commands run, which is what the human approval gate exists to
/// authorize — and forcing a re-approval to adjust a timeout would give an
/// operator a standing reason to leave it unset.
pub fn set_wall_clock_timeout(
    db: &Database,
    repo: &str,
    secs: Option<u64>,
) -> Result<GateConfig, MemoryError> {
    validate_repo(repo)?;
    validate_wall_clock_timeout(secs).map_err(MemoryError::Validation)?;
    let key = gate_config_key(repo);
    let mut config = match read_current(db, &key)? {
        Some(drawer) => serde_json::from_str::<GateConfig>(&drawer.content)?,
        None => {
            return Err(MemoryError::NotFound(format!(
                "no gate config has been proposed for repo '{repo}'"
            )))
        }
    };
    config.wall_clock_timeout_secs = secs;
    let content = serde_json::to_string(&config)?;
    write_current(db, &key, &content)?;
    Ok(config)
}

/// The approved per-dispatch wall-clock bound for `repo`, if one is set.
///
/// `Ok(None)` covers both "no config" and "config with no bound"; callers
/// that need to *refuse* an unbounded dispatch check the config's existence
/// separately (they already must — see
/// [`super::run::approved_gate_commands`]).
pub fn wall_clock_timeout(db: &Database, repo: &str) -> Result<Option<u64>, MemoryError> {
    Ok(get_gate_config(db, repo)?.and_then(|config| config.wall_clock_timeout_secs))
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

    // ── rung 7: the per-repo wall-clock bound ───────────────────────────

    #[test]
    fn a_repo_has_no_wall_clock_bound_until_a_human_sets_one() {
        // No default, on purpose: nothing rung 0 or rung 2 measured supports
        // a number, and a repo's gate suite is the only thing that could.
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(&db, "ironmem", vec!["cargo test".into()], vec![]).unwrap();
        assert_eq!(wall_clock_timeout(&db, "ironmem").unwrap(), None);
    }

    #[test]
    fn the_wall_clock_bound_round_trips_and_can_be_cleared() {
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(&db, "ironmem", vec!["cargo test".into()], vec![]).unwrap();
        set_wall_clock_timeout(&db, "ironmem", Some(2_700)).unwrap();
        assert_eq!(wall_clock_timeout(&db, "ironmem").unwrap(), Some(2_700));
        set_wall_clock_timeout(&db, "ironmem", None).unwrap();
        assert_eq!(wall_clock_timeout(&db, "ironmem").unwrap(), None);
    }

    #[test]
    fn a_zero_wall_clock_bound_is_refused_on_every_path() {
        // Zero does not mean "no bound" — it means "kill every dispatch
        // immediately", which would burn the issue's whole attempt budget.
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(&db, "ironmem", vec!["cargo test".into()], vec![]).unwrap();
        assert!(set_wall_clock_timeout(&db, "ironmem", Some(0)).is_err());

        // And on the deserialization path, which a hand-edited row reaches
        // without going through the setter at all.
        let raw = r#"{"repo":"ironmem","state":"approved","gate_commands":["cargo test"],
            "manifest_warnings":[],"proposed_at":"2026-09-01T00:00:00Z","approved_at":null,
            "wall_clock_timeout_secs":0}"#;
        assert!(serde_json::from_str::<GateConfig>(raw).is_err());
    }

    #[test]
    fn setting_the_bound_does_not_revoke_an_existing_approval() {
        // Tightening a timeout does not change which commands run, which is
        // what the approval gate authorizes. Forcing a re-approval to adjust
        // it would give an operator a standing reason to leave it unset.
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(&db, "ironmem", vec!["cargo test".into()], vec![]).unwrap();
        approve_gate_config(&db, "ironmem").unwrap();
        let config = set_wall_clock_timeout(&db, "ironmem", Some(600)).unwrap();
        assert_eq!(config.state, GateConfigState::Approved);
        assert!(is_gate_config_approved(&db, "ironmem").unwrap());
    }

    #[test]
    fn re_proposing_carries_the_wall_clock_bound_forward() {
        // Every other field here is inferred, so re-inference replaces it.
        // This one is not inferred from anything, so a re-onboard has nothing
        // to say about it — and dropping it would silently un-bound the repo
        // as a side effect of an unrelated action.
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(&db, "ironmem", vec!["cargo test".into()], vec![]).unwrap();
        set_wall_clock_timeout(&db, "ironmem", Some(1_200)).unwrap();

        let reproposed =
            propose_gate_config(&db, "ironmem", vec!["cargo nextest run".into()], vec![]).unwrap();
        assert_eq!(
            reproposed.state,
            GateConfigState::Pending,
            "still re-approved by a human"
        );
        assert_eq!(reproposed.gate_commands(), ["cargo nextest run"]);
        assert_eq!(reproposed.wall_clock_timeout_secs, Some(1_200));
    }

    #[test]
    fn a_config_written_before_the_bound_existed_reads_back_as_unbounded() {
        // `#[serde(default)]`, checked against the literal pre-rung-7 shape
        // rather than against a round-trip of today's struct.
        let raw = r#"{"repo":"ironmem","state":"approved","gate_commands":["cargo test"],
            "manifest_warnings":[],"proposed_at":"2026-08-27T00:00:00Z",
            "approved_at":"2026-08-27T00:00:00Z"}"#;
        let config: GateConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(config.wall_clock_timeout_secs, None);
    }

    #[test]
    fn setting_a_bound_on_a_repo_with_no_config_is_an_error() {
        let db = Database::open_in_memory().unwrap();
        assert!(set_wall_clock_timeout(&db, "never-onboarded", Some(600)).is_err());
    }

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

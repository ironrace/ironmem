//! Approved gate config — `logical_key` per repo, `pending` → `approved`
//! (spec's *Repo onboarding* section).
//!
//! This module implements only the storage/state-transition half of that
//! section: writing a proposed config and flipping it to approved. Inferring
//! the gate commands from a repo's build manifests is [`super::onboard`]
//! (rung 3's Onboarder); nothing here inspects a repo at all.

use serde::{Deserialize, Serialize};

use crate::db::read_only::ReadOnlyDb;
use crate::db::schema::Database;
use crate::error::MemoryError;

use super::{
    read_current, read_current_read_only, repo_slug, validate_repo, write_current,
    EMPTY_GATE_COMMANDS_MSG,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateConfigState {
    Pending,
    Approved,
}

impl GateConfigState {
    /// The state's name, spelled as it is stored and serialized.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
        }
    }
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
    /// Non-fatal problems a human must read before approving. Mostly the
    /// Onboarder's: problems it hit while inferring `gate_commands`
    /// — e.g. a build manifest that exists but couldn't be read or parsed,
    /// encountered while a *different* stack was still recognized (see
    /// `onboard`'s module docs: one broken manifest must not veto a
    /// different stack's real command — but it must not vanish without a
    /// trace either, which is what this field closes). Always empty for a
    /// config proposed by hand via [`propose_gate_config`] directly. A human
    /// approving a `pending` config should read these first: an inferred
    /// gate that looks complete may be silently missing a stack whose
    /// manifest was broken.
    ///
    /// Not only the Onboarder's, and not always empty on a hand-authored
    /// proposal: [`propose_gate_config`] adds one of its own when a carried
    /// wall-clock bound no longer matches the gate being proposed, or when
    /// the stored config could not be read.
    ///
    /// Not always the Onboarder's, and not always empty on a hand-authored
    /// proposal: [`propose_gate_config`] adds one of its own when a carried
    /// wall-clock bound no longer matches the gate being proposed, or when
    /// the stored config could not be read.
    pub manifest_warnings: Vec<String>,
    /// The gate commands this bound was last blessed against — written by
    /// [`set_wall_clock_timeout`] when the bound is set, and again by
    /// [`approve_gate_config`], since a human approving a gate is saying the
    /// bound is right for *that* gate.
    ///
    /// Recorded rather than inferred because the alternative, comparing
    /// against whatever was proposed last, is edge-triggered: running
    /// `onboard` twice before approving would compare the second proposal
    /// against the first, find them identical, and drop the warning that the
    /// bound no longer measures this gate. Absent on configs written before
    /// this field existed, where the previous proposal's commands are the
    /// best available stand-in.
    #[serde(default)]
    pub wall_clock_timeout_commands: Option<Vec<String>>,
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
    ///
    /// See [`GateConfig::wall_clock_timeout_commands`] for what keeps it
    /// honest across a re-onboard.
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
    #[serde(default)]
    wall_clock_timeout_commands: Option<Vec<String>>,
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
            // Normalized rather than rejected, unlike `gate_commands`. A
            // calibration record with no bound to calibrate is stale data,
            // not a lie worth refusing a whole repo's config over — and
            // refusing would make every dispatch into that repo fail, which
            // is a worse answer than dropping a field nothing reads. Dropped
            // here, on the one path every `GateConfig` comes through, so it
            // cannot be carried forward or read back as a bound.
            wall_clock_timeout_commands: raw
                .wall_clock_timeout_secs
                .and(raw.wall_clock_timeout_commands),
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
            "wall_clock_timeout_secs must be at least 1 second — omit it entirely for no bound, \
             since 0 would kill every dispatch immediately"
                .to_string(),
        );
    }
    Ok(())
}

/// The logical-key prefix every gate config's key starts with.
const GATE_CONFIG_KEY_PREFIX: &str = "gate-config:";

/// How many stored gate configs [`stored_gate_configs_read_only`] will list.
/// A listing that fills it may be incomplete, so it is an error.
const STORED_GATE_CONFIG_LIMIT: usize = 1000;

fn gate_config_key(repo: &str) -> String {
    format!("{GATE_CONFIG_KEY_PREFIX}{}", repo_slug(repo))
}

/// One stored gate config, as [`stored_gate_configs_read_only`] lists it.
#[derive(Debug)]
pub(super) enum StoredGateConfig {
    /// A row that parses, by its `repo`.
    Parsed(String),
    /// A row that does not parse.
    Broken(BrokenGateConfig),
}

/// A stored gate config row that does not parse as a [`GateConfig`].
#[derive(Debug)]
pub struct BrokenGateConfig {
    /// The logical key the row is stored under: `gate-config:` followed by
    /// the slug of the repo name it was written for.
    pub(super) storage_key: String,
    /// The row's own `repo`, when its content is a JSON object that holds one
    /// as a string. `None` for content that is not JSON, or has no such field.
    pub(super) repo: Option<String>,
    /// Why the row does not parse.
    pub(super) error: serde_json::Error,
}

impl BrokenGateConfig {
    /// The slug in the row's storage key: the repo name the row was written
    /// for with every character outside `[A-Za-z0-9_.-]` replaced by `-`, so
    /// the name cannot be read back from it exactly.
    pub(super) fn slug(&self) -> &str {
        self.storage_key
            .strip_prefix(GATE_CONFIG_KEY_PREFIX)
            .unwrap_or(&self.storage_key)
    }
}

/// Why [`stored_gate_configs_read_only`] could not list the stored configs.
#[derive(Debug)]
pub(super) enum StoredGateConfigsError {
    /// The read failed.
    Read(MemoryError),
    /// The listing reached [`STORED_GATE_CONFIG_LIMIT`] rows, so it may be
    /// incomplete, and an incomplete list cannot show that no near-match
    /// exists.
    LimitReached { limit: usize },
}

/// Every stored gate config, read-only (#420): the `repo` of each row that
/// parses, and for each row that does not, the key it is stored under, the
/// parse error and the `repo` its content still holds. A row marked
/// superseded is listed, because the read by name
/// ([`get_gate_config_read_only`]) is a lookup by id that returns such a row
/// too.
pub(super) fn stored_gate_configs_read_only(
    db: &ReadOnlyDb,
) -> Result<Vec<StoredGateConfig>, StoredGateConfigsError> {
    let source_prefix = crate::mcp::tools::LOGICAL_KEY_SOURCE_PREFIX;
    let drawers = db
        .get_drawers_by_source_prefix(
            super::WING,
            super::ROOM,
            &format!("{source_prefix}{GATE_CONFIG_KEY_PREFIX}"),
            STORED_GATE_CONFIG_LIMIT,
        )
        .map_err(StoredGateConfigsError::Read)?;
    if drawers.len() >= STORED_GATE_CONFIG_LIMIT {
        return Err(StoredGateConfigsError::LimitReached {
            limit: STORED_GATE_CONFIG_LIMIT,
        });
    }
    Ok(drawers
        .into_iter()
        .map(
            |drawer| match serde_json::from_str::<GateConfig>(&drawer.content) {
                Ok(config) => StoredGateConfig::Parsed(config.repo),
                Err(error) => StoredGateConfig::Broken(BrokenGateConfig {
                    storage_key: drawer
                        .source_file
                        .strip_prefix(source_prefix)
                        .unwrap_or(&drawer.source_file)
                        .to_string(),
                    repo: held_repo(&drawer.content),
                    error,
                }),
            },
        )
        .collect())
}

/// The `repo` that row content holds, read without [`GateConfig`]'s guard:
/// `Some` when the content is a JSON object whose `repo` is a string.
fn held_repo(content: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()?
        .get("repo")?
        .as_str()
        .map(str::to_string)
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
/// carried through (a hand-authored proposal has none to report; pass an
/// empty vec), and this function may add one of its own — see the carried
/// wall-clock bound below.
pub fn propose_gate_config(
    db: &Database,
    repo: &str,
    gate_commands: Vec<String>,
    mut manifest_warnings: Vec<String>,
) -> Result<GateConfig, MemoryError> {
    validate_repo(repo)?;
    validate_gate_commands(&gate_commands).map_err(MemoryError::Validation)?;
    // A re-proposal carries the existing wall-clock bound forward. Every
    // other field here is *inferred*, so re-running inference legitimately
    // replaces it — but the bound is not inferred from anything (see the
    // field's doc), so re-inference has nothing to say about it, and dropping
    // it would silently un-bound the repo's dispatches as a side effect of an
    // unrelated re-onboard.
    // Every other read path propagates a deserialize failure with `?`. This
    // one must not: a broken row has to stay re-onboardable. But it must not
    // *vanish* either — this is the one path that then overwrites the row, so
    // a silent `.ok()` destroys both the bound and the evidence it ever
    // existed, and the operator approves a normal-looking gate whose
    // dispatches are now unbounded.
    let existing = match read_current(db, &gate_config_key(repo))? {
        None => None,
        Some(drawer) => match serde_json::from_str::<GateConfig>(&drawer.content) {
            Ok(config) => Some(config),
            Err(err) => {
                manifest_warnings.push(format!(
                    "the stored gate config for '{repo}' could not be read ({err}) and is being                      replaced — any per-dispatch wall-clock bound it carried is lost; re-set it                      before approving"
                ));
                None
            }
        },
    };
    let carried = existing
        .as_ref()
        .and_then(|existing| existing.wall_clock_timeout_secs);
    // Carrying it forward silently is only safe while the gate it was
    // calibrated against is the gate it now bounds. A human sets the bound
    // from recorded dispatch durations; once re-inference changes what the
    // gate executes — a check command inference did not used to propose, say
    // — those durations measured something else, and a bound that is now too
    // tight kills dispatches mid-gate as "wedged". Nothing else in the
    // proposal would say so.
    // Only meaningful alongside a bound: a calibration record with nothing to
    // calibrate is stale data, and carrying it forward would keep it alive
    // through every future proposal.
    let calibrated_for = carried.and_then(|_| {
        existing.as_ref().and_then(|existing| {
            existing
                .wall_clock_timeout_commands
                .clone()
                .or_else(|| Some(existing.gate_commands().to_vec()))
        })
    });
    if let (Some(secs), Some(calibrated_for)) = (carried, calibrated_for.as_ref()) {
        if calibrated_for != &gate_commands {
            manifest_warnings.push(format!(
                "the {secs}s per-dispatch wall-clock bound was calibrated against `{}`, not the \
                 gate proposed here — re-check it before approving",
                calibrated_for.join(" && ")
            ));
        }
    }
    let config = GateConfig {
        repo: repo.to_string(),
        state: GateConfigState::Pending,
        gate_commands,
        manifest_warnings,
        wall_clock_timeout_commands: calibrated_for,
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
        // Approving is a human saying this bound is right for *this* gate, so
        // it re-calibrates the record. Without this the carried-bound warning
        // is level-triggered against a gate two proposals ago and re-fires on
        // every future re-onboard, long after the human checked it — which is
        // how an approver learns to skim warnings.
        if config.wall_clock_timeout_secs.is_some() {
            config.wall_clock_timeout_commands = Some(config.gate_commands().to_vec());
        }
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
    // What the bound was calibrated against, recorded at the moment it is
    // set. A later re-onboard compares against this rather than against the
    // last proposal, so the warning survives being re-proposed.
    config.wall_clock_timeout_commands = secs.map(|_| config.gate_commands().to_vec());
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

/// [`get_gate_config`] over a read-only handle (#420), for
/// `ironmem autopilot status`.
///
/// Goes through the same `GateConfig` deserialize guard as every other read:
/// a stored row with no real gate is an `Err`, never a config.
pub(super) fn get_gate_config_read_only(
    db: &ReadOnlyDb,
    repo: &str,
) -> Result<Option<GateConfig>, MemoryError> {
    match read_current_read_only(db, &gate_config_key(repo))? {
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

/// Fixtures for the unit tests of this module's siblings.
#[cfg(test)]
pub(super) mod testing {
    use super::*;

    /// Store `content` as `repo`'s gate config exactly as given, skipping
    /// every check a proposal goes through, so a test can read back a row no
    /// writer would produce.
    pub(in crate::autopilot) fn store_raw_gate_config(db: &Database, repo: &str, content: &str) {
        write_current(db, &gate_config_key(repo), content).unwrap();
    }

    /// Store a parseable gate config for each of `count` repos, named
    /// `many/repo-<n>`, in one transaction.
    pub(in crate::autopilot) fn store_gate_configs(db: &Database, count: usize) {
        db.with_transaction(|tx| {
            for n in 0..count {
                let repo = format!("many/repo-{n}");
                let content = serde_json::json!({
                    "repo": repo,
                    "state": "pending",
                    "gate_commands": ["cargo test"],
                    "manifest_warnings": [],
                    "proposed_at": "2026-01-01T00:00:00+00:00",
                    "approved_at": null,
                })
                .to_string();
                crate::autopilot::write_current_tx(tx, &gate_config_key(&repo), &content)?;
            }
            Ok(())
        })
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::testing::on_disk_db;

    #[test]
    fn a_state_is_named_as_it_is_serialized() {
        for state in [GateConfigState::Pending, GateConfigState::Approved] {
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(state.as_str())
            );
        }
    }

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
        // Carried onto a *different* gate, so the human re-approving has to
        // be told the bound was calibrated against the old one.
        assert_eq!(reproposed.manifest_warnings.len(), 1);
        assert!(
            reproposed.manifest_warnings[0].contains("1200s")
                && reproposed.manifest_warnings[0].contains("cargo test"),
            "expected the warning to quote the bound and the gate it measured, got: {:?}",
            reproposed.manifest_warnings
        );
    }

    #[test]
    fn approving_re_calibrates_the_bound_so_the_warning_does_not_re_fire() {
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(&db, "ironmem", vec!["cargo test".into()], vec![]).unwrap();
        set_wall_clock_timeout(&db, "ironmem", Some(1_200)).unwrap();
        let wider = vec!["cargo fmt --all -- --check".into(), "cargo test".into()];
        let warned = propose_gate_config(&db, "ironmem", wider.clone(), vec![]).unwrap();
        assert_eq!(warned.manifest_warnings.len(), 1);

        approve_gate_config(&db, "ironmem").unwrap();
        let after = propose_gate_config(&db, "ironmem", wider, vec![]).unwrap();

        assert!(
            after.manifest_warnings.is_empty(),
            "the human has checked this bound against this gate: {:?}",
            after.manifest_warnings
        );
        assert_eq!(
            after.wall_clock_timeout_commands,
            Some(vec![
                "cargo fmt --all -- --check".to_string(),
                "cargo test".to_string()
            ]),
            "approval re-calibrates the record, not just the warning"
        );
    }

    #[test]
    fn the_carried_bound_warning_survives_a_second_re_onboard() {
        // Comparing against the *last proposal* would be edge-triggered: the
        // second re-onboard sees identical commands and drops the warning,
        // and `approve` then blesses a gate bounded by a timeout calibrated
        // for a narrower one.
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(&db, "ironmem", vec!["cargo test".into()], vec![]).unwrap();
        set_wall_clock_timeout(&db, "ironmem", Some(1_200)).unwrap();
        let wider = vec!["cargo fmt --all -- --check".into(), "cargo test".into()];

        propose_gate_config(&db, "ironmem", wider.clone(), vec![]).unwrap();
        let second = propose_gate_config(&db, "ironmem", wider, vec![]).unwrap();

        assert_eq!(
            second.manifest_warnings.len(),
            1,
            "{:?}",
            second.manifest_warnings
        );
        assert!(
            second.manifest_warnings[0].contains("cargo test")
                && !second.manifest_warnings[0].contains("--check"),
            "the warning must still name the gate the bound measured, got: {:?}",
            second.manifest_warnings
        );
    }

    #[test]
    fn re_proposing_the_same_commands_carries_the_bound_without_a_warning() {
        // The bound still measures what it measured, so saying anything
        // here would be noise on every re-onboard that changed nothing.
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(&db, "ironmem", vec!["cargo test".into()], vec![]).unwrap();
        set_wall_clock_timeout(&db, "ironmem", Some(1_200)).unwrap();

        let reproposed =
            propose_gate_config(&db, "ironmem", vec!["cargo test".into()], vec![]).unwrap();

        assert_eq!(reproposed.wall_clock_timeout_secs, Some(1_200));
        assert!(
            reproposed.manifest_warnings.is_empty(),
            "{:?}",
            reproposed.manifest_warnings
        );
    }

    #[test]
    fn a_changed_gate_with_no_bound_set_has_nothing_to_warn_about() {
        let db = Database::open_in_memory().unwrap();
        propose_gate_config(&db, "ironmem", vec!["cargo test".into()], vec![]).unwrap();

        let reproposed =
            propose_gate_config(&db, "ironmem", vec!["cargo nextest run".into()], vec![]).unwrap();

        assert!(
            reproposed.manifest_warnings.is_empty(),
            "{:?}",
            reproposed.manifest_warnings
        );
    }

    #[test]
    fn a_bound_set_before_the_calibration_record_existed_still_warns_when_the_gate_widens() {
        // The rollout path: every already-onboarded repo has a bound with no
        // calibration record, and this PR's whole purpose is to widen those
        // repos' gates. If the legacy fallback regressed, the bound would
        // carry onto a materially longer gate with no warning, killing
        // dispatches mid-gate as "wedged".
        let db = Database::open_in_memory().unwrap();
        let legacy = r#"{"repo":"ironmem","state":"approved","gate_commands":["cargo test"],
            "manifest_warnings":[],"proposed_at":"2026-08-27T00:00:00Z",
            "approved_at":"2026-08-27T00:00:00Z","wall_clock_timeout_secs":1200}"#;
        write_current(&db, &gate_config_key("ironmem"), legacy).unwrap();

        let widened = propose_gate_config(
            &db,
            "ironmem",
            vec!["cargo fmt --all -- --check".into(), "cargo test".into()],
            vec![],
        )
        .unwrap();

        assert_eq!(widened.wall_clock_timeout_secs, Some(1_200));
        assert_eq!(
            widened.wall_clock_timeout_commands,
            Some(vec!["cargo test".to_string()]),
            "the legacy record is the gate the bound was measured against"
        );
        assert_eq!(widened.manifest_warnings.len(), 1);
        assert!(widened.manifest_warnings[0].contains("cargo test"));
    }

    #[test]
    fn a_calibration_record_with_no_bound_to_calibrate_is_dropped_on_read() {
        // Normalized rather than rejected: refusing would make every dispatch
        // into that repo fail over a field nothing reads.
        let raw = r#"{"repo":"ironmem","state":"approved","gate_commands":["cargo test"],
            "manifest_warnings":[],"wall_clock_timeout_commands":["cargo nextest run"],
            "proposed_at":"2026-08-27T00:00:00Z","approved_at":"2026-08-27T00:00:00Z"}"#;

        let config: GateConfig = serde_json::from_str(raw).unwrap();

        assert_eq!(config.wall_clock_timeout_secs, None);
        assert_eq!(config.wall_clock_timeout_commands, None);
    }

    #[test]
    fn a_stored_config_that_cannot_be_read_is_replaced_but_reported() {
        // The only read path that does not propagate a deserialize failure —
        // because a broken row must stay re-onboardable — and also the only
        // one that then overwrites it. Silence here destroys the bound and
        // the evidence it existed.
        let db = Database::open_in_memory().unwrap();
        write_current(&db, &gate_config_key("ironmem"), "{ not json").unwrap();

        let config =
            propose_gate_config(&db, "ironmem", vec!["cargo test".into()], vec![]).unwrap();

        assert_eq!(config.manifest_warnings.len(), 1);
        assert!(
            config.manifest_warnings[0].contains("could not be read"),
            "got: {:?}",
            config.manifest_warnings
        );
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

    // ── #420: the read-only read ────────────────────────────────────────

    #[test]
    fn the_read_only_read_returns_exactly_what_the_writer_stored() {
        let (_dir, path) = on_disk_db();
        let stored = {
            let db = Database::open(&path).unwrap();
            propose_gate_config(&db, "owner/repo", vec!["cargo test".into()], vec![]).unwrap();
            approve_gate_config(&db, "owner/repo").unwrap()
        };

        let ro = Database::open_read_only(&path).unwrap();

        assert_eq!(
            get_gate_config_read_only(&ro, "owner/repo").unwrap(),
            Some(stored)
        );
    }

    #[test]
    fn the_read_only_read_reports_a_repo_never_onboarded_as_none() {
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            propose_gate_config(&db, "other/repo", vec!["cargo test".into()], vec![]).unwrap();
        }

        let ro = Database::open_read_only(&path).unwrap();

        assert_eq!(get_gate_config_read_only(&ro, "owner/repo").unwrap(), None);
    }

    #[test]
    fn the_read_only_read_refuses_a_stored_config_with_no_gate() {
        // The same deserialize guard every other read path goes through: a
        // row with no gate commands is an error, never an approved config.
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            write_current(
                &db,
                &gate_config_key("owner/repo"),
                r#"{"repo":"owner/repo","state":"approved","gate_commands":[],"proposed_at":"2026-01-01T00:00:00+00:00","approved_at":null}"#,
            )
            .unwrap();
        }

        let ro = Database::open_read_only(&path).unwrap();
        let err = get_gate_config_read_only(&ro, "owner/repo").unwrap_err();

        assert!(matches!(err, MemoryError::Json(_)), "{err}");
        assert!(err.to_string().contains(EMPTY_GATE_COMMANDS_MSG), "{err}");
    }

    /// The parsed repo names in `listing`, sorted, and its broken rows by
    /// storage key.
    fn split(listing: Vec<StoredGateConfig>) -> (Vec<String>, Vec<BrokenGateConfig>) {
        let mut parsed = Vec::new();
        let mut broken = Vec::new();
        for row in listing {
            match row {
                StoredGateConfig::Parsed(repo) => parsed.push(repo),
                StoredGateConfig::Broken(row) => broken.push(row),
            }
        }
        parsed.sort();
        broken.sort_by(|a, b| a.storage_key.cmp(&b.storage_key));
        (parsed, broken)
    }

    #[test]
    fn stored_configs_lists_a_broken_row_with_its_key_its_error_and_the_name_it_holds() {
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            propose_gate_config(&db, "owner/one", vec!["cargo test".into()], vec![]).unwrap();
            propose_gate_config(&db, "owner/two", vec!["cargo test".into()], vec![]).unwrap();
            // Not JSON at all: no name can be read from the content.
            write_current(&db, &gate_config_key("owner/broken"), "not json").unwrap();
            // JSON that fails the guard: the content still names its repo.
            write_current(
                &db,
                &gate_config_key("Owner/Guarded"),
                r#"{"repo":"Owner/Guarded","state":"approved","gate_commands":[],"proposed_at":"2026-01-01T00:00:00+00:00","approved_at":null}"#,
            )
            .unwrap();
        }

        let ro = Database::open_read_only(&path).unwrap();
        let (parsed, broken) = split(stored_gate_configs_read_only(&ro).unwrap());

        assert_eq!(
            parsed,
            vec!["owner/one".to_string(), "owner/two".to_string()]
        );
        assert_eq!(broken.len(), 2, "{broken:?}");
        let guarded = &broken[0];
        assert_eq!(guarded.storage_key, "gate-config:Owner-Guarded");
        assert_eq!(guarded.slug(), "Owner-Guarded");
        assert_eq!(guarded.repo.as_deref(), Some("Owner/Guarded"));
        assert!(
            guarded.error.to_string().contains(EMPTY_GATE_COMMANDS_MSG),
            "{}",
            guarded.error
        );
        let not_json = &broken[1];
        assert_eq!(not_json.storage_key, "gate-config:owner-broken");
        assert_eq!(not_json.slug(), "owner-broken");
        assert_eq!(not_json.repo, None);
        assert!(
            not_json.error.to_string().contains("expected"),
            "{}",
            not_json.error
        );
    }

    #[test]
    fn a_broken_row_holds_no_name_when_its_repo_is_not_a_string() {
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            write_current(&db, &gate_config_key("a/number"), r#"{"repo":7}"#).unwrap();
            write_current(&db, &gate_config_key("a/list"), r#"["repo","a/list"]"#).unwrap();
        }

        let ro = Database::open_read_only(&path).unwrap();
        let (parsed, broken) = split(stored_gate_configs_read_only(&ro).unwrap());

        assert_eq!(parsed, Vec::<String>::new());
        assert_eq!(broken.len(), 2, "{broken:?}");
        assert!(broken.iter().all(|row| row.repo.is_none()), "{broken:?}");
    }

    #[test]
    fn stored_configs_is_empty_for_a_database_with_no_gate_config() {
        let (_dir, path) = on_disk_db();
        let ro = Database::open_read_only(&path).unwrap();
        let (parsed, broken) = split(stored_gate_configs_read_only(&ro).unwrap());
        assert_eq!(parsed, Vec::<String>::new());
        assert!(broken.is_empty(), "{broken:?}");
    }

    #[test]
    fn a_listing_one_short_of_the_limit_is_returned_whole() {
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            testing::store_gate_configs(&db, STORED_GATE_CONFIG_LIMIT - 1);
        }

        let ro = Database::open_read_only(&path).unwrap();
        let (parsed, broken) = split(stored_gate_configs_read_only(&ro).unwrap());

        assert_eq!(parsed.len(), STORED_GATE_CONFIG_LIMIT - 1);
        assert!(broken.is_empty(), "{broken:?}");
    }

    #[test]
    fn a_listing_that_fills_the_limit_is_refused() {
        // A listing cut off at the limit cannot show that no near-match
        // exists, and the query asks for no more rows than the limit, so the
        // count can reach it and never pass it.
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            testing::store_gate_configs(&db, STORED_GATE_CONFIG_LIMIT);
        }

        let ro = Database::open_read_only(&path).unwrap();
        let listed = stored_gate_configs_read_only(&ro);

        assert!(
            matches!(
                listed,
                Err(StoredGateConfigsError::LimitReached {
                    limit: STORED_GATE_CONFIG_LIMIT
                })
            ),
            "expected the limit, got {}",
            match &listed {
                Ok(rows) => format!("a listing of {} rows", rows.len()),
                Err(err) => format!("{err:?}"),
            }
        );
    }
}

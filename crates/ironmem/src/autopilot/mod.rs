//! Autopilot backlog runner (build-ladder rungs 1-3).
//!
//! See `docs/iron/specs/2026-08-21-autonomous-backlog-runner-design.md` for
//! the full design. Rung 1 built the storage half — the `backlog-lineage`
//! drawer room and its knowledge-graph edges. Rung 2 adds the [`dispatch`]
//! primitive (build the IC's argv, run it, parse its result JSON) and the
//! [`turn_prompt`] template that fills the `/goal` condition — together, the
//! *IC lifecycle* CLI invocation the spec's rung-0/rung-2 validation rounds
//! measured. Rung 3 adds [`onboard`], the Onboarder that infers gate
//! commands from a repo's build manifests and proposes them via
//! [`gate_config`]. The Lead's orchestration loop, the Reviewer, and merge
//! authority are later rungs.
//!
//! # The five drawer kinds, one room
//!
//! The spec's *Storage* section defines five distinct drawer kinds, all
//! living in the same room (`ROOM`, `"backlog-lineage"`) but distinguished by
//! whether — and how — `logical_key` is used:
//!
//! 1. [`lineage::AttemptRecord`] — plain `add_drawer` calls, **no
//!    `logical_key`, ever**. One drawer per attempt, append-only.
//! 2. [`lineage::IssueStatus`] — `logical_key` per issue: best-so-far state,
//!    overwritten on each update.
//! 3. [`dispatch_state::DispatchState`] — `logical_key` per in-flight issue:
//!    the Lead's crash-safe memory.
//! 4. [`budget::BudgetLedgerEntry`] — `logical_key` per date: the daily spend
//!    ledger, accumulated across invocations.
//! 5. [`gate_config::GateConfig`] — `logical_key` per repo: the
//!    `pending` → `approved` gate-config state machine (storage/transition;
//!    [`onboard`] is the rung-3 Onboarder that infers the proposed content).
//!
//! # The `logical_key` hazard
//!
//! `add_drawer`'s `logical_key` *rewrites* the drawer in that wing/room —
//! that is exactly what makes kinds 2–5 above work as "current state", and
//! exactly why kind 1 (attempt lineage) must never use it: doing so would
//! silently destroy every earlier attempt's history the moment a new one is
//! filed. [`write_current`] is the **only** write path in this module that
//! takes a `logical_key`; [`lineage::record_attempt`] deliberately never
//! calls it — its drawer id is derived straight from content via
//! `crate::db::drawers::generate_id`, so there is no `logical_key` argument
//! in that code path to mis-supply in the first place. See
//! `lineage::tests::n_failed_attempts_produce_n_distinct_drawers` for the
//! regression guard.
//!
//! # Wing and room
//!
//! The spec's storage table names a single room (`backlog-lineage`,
//! mentioned five times as "same room") and is silent on wing. Rather than
//! inventing a per-repo wing scheme the spec doesn't ask for, every kind
//! above shares one fixed wing ([`WING`], `"autopilot"`) and disambiguates by
//! repo inside the logical key or record body instead — a judgment call,
//! documented here so it's easy to revisit.

pub mod budget;
pub mod dispatch;
pub mod dispatch_state;
pub mod gate_config;
pub mod lineage;
pub mod onboard;
pub mod scrub;
pub mod turn_prompt;

use crate::db::drawers::{generate_id, Drawer};
use crate::db::schema::Database;
use crate::error::MemoryError;
// Imported (not duplicated) from `crate::mcp::tools`, which re-exports them
// from the `drawers` tool module: a drawer written through
// `mcp__ironmem__add_drawer{logical_key}` and one written through this
// module for the same logical key must compute the same id/source-file, and
// a hand-copied literal here could silently drift out of sync with the
// original.
use crate::mcp::tools::{LOGICAL_KEY_ID_PREFIX, LOGICAL_KEY_SOURCE_PREFIX};

pub use budget::BudgetLedgerEntry;
pub use dispatch::{DispatchOutcome, DispatchSpec, SessionMode, Verdict};
pub use dispatch_state::DispatchState;
pub use gate_config::{GateConfig, GateConfigState};
pub use lineage::{AttemptOutcome, AttemptRecord, IssueStatus, RecordedAttempt};
pub use onboard::{infer_gate_commands, onboard_repo, InferredGates};
pub use turn_prompt::{PriorAttempt, TurnPromptInputs};

/// Drawer wing shared by every Autopilot backlog-lineage record. See the
/// module doc's *Wing and room* section for why this is a single constant
/// rather than one wing per repo.
pub const WING: &str = "autopilot";

/// The single drawer room the spec defines for all five lineage kinds.
pub const ROOM: &str = "backlog-lineage";

/// `added_by` recorded on every drawer this module writes, so a
/// `get_drawer`/`search` reader can tell Autopilot's own bookkeeping apart
/// from human- or MCP-client-authored content in the same database.
const ADDED_BY: &str = "autopilot";

/// A GitHub issue, scoped to its repo. Autopilot spans "all write-access
/// repos" (see the spec's Problem section), so an issue number alone is
/// ambiguous — every lineage kind that's keyed per-issue carries the repo
/// alongside it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IssueRef {
    pub repo: String,
    pub number: u64,
}

impl IssueRef {
    pub fn new(repo: impl Into<String>, number: u64) -> Self {
        Self {
            repo: repo.into(),
            number,
        }
    }

    /// Human-readable `repo#number` form, used as the attempt record's
    /// `issue` field (which the spec's shape gives no separate `repo`
    /// sibling to).
    pub fn canonical(&self) -> String {
        format!("{}#{}", self.repo, self.number)
    }

    /// Filesystem/logical-key-safe slug: `repo_slug-number`. Used to build
    /// every per-issue logical key in this module.
    pub(crate) fn slug(&self) -> String {
        format!("{}-{}", repo_slug(&self.repo), self.number)
    }

    /// The knowledge-graph entity name for this issue. Must satisfy
    /// `sanitize::sanitize_name`'s character class so a human can still
    /// `kg_query` it directly (no `:` or `/`), which is why it uses
    /// [`IssueRef::slug`] rather than [`IssueRef::canonical`].
    pub(crate) fn entity_name(&self) -> String {
        format!("issue-{}", self.slug())
    }
}

/// Reduce a repo identifier (e.g. `"owner/repo"`) to characters safe for both
/// a logical key (`sanitize::sanitize_logical_key`'s charset) and a
/// knowledge-graph entity name (`sanitize::sanitize_name`'s, which is
/// stricter — no `:`). Anything outside `[A-Za-z0-9_.-]` becomes `-`.
pub(crate) fn repo_slug(repo: &str) -> String {
    repo.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Minimal validation for a repo identifier. Deliberately permissive on
/// character set (repo strings such as `"owner/repo"` are expected and are
/// never used as a drawer wing/room/logical-key literal directly — only
/// [`repo_slug`]'s output is), but still rejects the empty/oversized/control
/// character cases a malformed caller could otherwise persist.
pub(crate) fn validate_repo(repo: &str) -> Result<(), MemoryError> {
    let trimmed = repo.trim();
    if trimmed.is_empty() {
        return Err(MemoryError::Validation(
            "repo must be a non-empty string".into(),
        ));
    }
    if trimmed.chars().count() > 200 {
        return Err(MemoryError::Validation(
            "repo exceeds maximum length of 200".into(),
        ));
    }
    if trimmed.contains('\0') || trimmed.chars().any(|c| c.is_control()) {
        return Err(MemoryError::Validation(
            "repo contains control characters".into(),
        ));
    }
    Ok(())
}

/// Shared between [`gate_config::propose_gate_config`]'s `Result`-returning
/// storage-boundary check and [`turn_prompt::render`]'s `assert!` — both
/// exist to catch the same "a dispatch needs a real gate to satisfy"
/// invariant, at two different points a caller can violate it, so both
/// quote this one string rather than two independently-maintained literals
/// that could silently drift apart.
pub(crate) const EMPTY_GATE_COMMANDS_MSG: &str =
    "gate_commands must not be empty — a dispatch needs a real gate to satisfy";

/// A zero vector of the bundled embedder's dimensionality. Every Autopilot
/// drawer is written directly against [`Database`], not through the `App`
/// layer that owns a real `Embedder` — these records exist to be found by
/// exact `logical_key`/`id` lookup and knowledge-graph traversal (see the
/// spec's note that semantic search alone can't reliably enumerate an
/// issue's attempts), not by semantic search, so paying to load the
/// embedding model on Autopilot's write path buys nothing today. Re-embedding
/// these drawers for real, if a later rung wants them to also participate in
/// `search`, is a follow-up rather than something this storage layer needs to
/// decide now.
fn zero_embedding() -> Vec<f32> {
    vec![0.0; ironrace_embed::EMBED_DIM]
}

/// The deterministic drawer id for a given logical key, computed exactly the
/// way `mcp__ironmem__add_drawer`/`get_drawer` do it — see
/// [`LOGICAL_KEY_ID_PREFIX`]'s caveat.
fn logical_drawer_id(key: &str) -> String {
    generate_id(&format!("{LOGICAL_KEY_ID_PREFIX}{key}"), WING, ROOM)
}

/// Write (or overwrite) the current drawer for `key` in this module's
/// wing/room. This is the **only** write path in this module that uses a
/// logical key — kinds 2–5 in the module doc call this, kind 1 (attempt
/// lineage) never does.
pub(crate) fn write_current(
    db: &Database,
    key: &str,
    content: &str,
) -> Result<String, MemoryError> {
    let id = logical_drawer_id(key);
    let source_file = format!("{LOGICAL_KEY_SOURCE_PREFIX}{key}");
    let embedding = zero_embedding();
    db.with_transaction(|tx| {
        Database::insert_drawer_tx(
            tx,
            &id,
            content,
            &embedding,
            WING,
            ROOM,
            &source_file,
            ADDED_BY,
        )?;
        Database::wal_log_tx(
            tx,
            "autopilot_write_current",
            &serde_json::json!({ "id": &id, "logical_key": key }),
            None,
        )?;
        Ok(())
    })?;
    Ok(id)
}

/// Read the current drawer for `key` in this module's wing/room, if any.
pub(crate) fn read_current(db: &Database, key: &str) -> Result<Option<Drawer>, MemoryError> {
    let id = logical_drawer_id(key);
    db.get_drawer(&id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_slug_replaces_unsafe_characters() {
        assert_eq!(repo_slug("ironrace/ironmem"), "ironrace-ironmem");
        assert_eq!(repo_slug("plain-repo"), "plain-repo");
        assert_eq!(repo_slug("weird repo!!"), "weird-repo--");
    }

    #[test]
    fn issue_ref_slug_and_entity_name_avoid_reserved_characters() {
        let issue = IssueRef::new("ironrace/ironmem", 283);
        assert_eq!(issue.slug(), "ironrace-ironmem-283");
        assert_eq!(issue.entity_name(), "issue-ironrace-ironmem-283");
        assert_eq!(issue.canonical(), "ironrace/ironmem#283");
        // No `/`, `#`, or `:` — safe for both sanitize_name and
        // sanitize_logical_key's character classes.
        assert!(!issue.slug().contains('/'));
        assert!(!issue.entity_name().contains('/'));
        assert!(!issue.entity_name().contains(':'));
    }

    #[test]
    fn validate_repo_rejects_empty_and_control_characters() {
        assert!(validate_repo("").is_err());
        assert!(validate_repo("   ").is_err());
        assert!(validate_repo("owner/repo\u{0}").is_err());
        assert!(validate_repo("owner/repo").is_ok());
    }

    #[test]
    fn write_current_is_idempotent_on_the_computed_id() {
        let db = Database::open_in_memory().unwrap();
        let id_first = write_current(&db, "test-key", "v1").unwrap();
        let id_second = write_current(&db, "test-key", "v2").unwrap();
        assert_eq!(
            id_first, id_second,
            "same logical key must resolve to the same drawer id"
        );
        let drawer = db.get_drawer(&id_second).unwrap().unwrap();
        assert_eq!(drawer.content, "v2");
    }

    #[test]
    fn logical_key_prefixes_are_imported_from_the_shared_mcp_add_drawer_scheme() {
        // `LOGICAL_KEY_ID_PREFIX`/`LOGICAL_KEY_SOURCE_PREFIX` are `use`d from
        // `crate::mcp::tools` (see the import above), not duplicated as
        // separate literals — so this pins today's actual values rather than
        // asserting a local copy stayed in sync with itself. If
        // `mcp/tools/drawers.rs` ever changes either value, this fails here
        // instead of the two write paths silently diverging onto different
        // drawer ids for the same logical key.
        assert_eq!(LOGICAL_KEY_ID_PREFIX, "logical-key:");
        assert_eq!(LOGICAL_KEY_SOURCE_PREFIX, "logical:");
    }

    #[test]
    fn generate_id_never_reuses_an_id_for_distinct_content() {
        // The primitive `lineage::record_attempt` builds its append-only
        // write path on: distinct content must hash to distinct ids within
        // the same wing/room.
        let id_a = generate_id("content a", WING, ROOM);
        let id_b = generate_id("content b", WING, ROOM);
        assert_ne!(id_a, id_b);
    }
}

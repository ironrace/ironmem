//! The session registry — build-ladder rung 7.
//!
//! One job: answer "is this IC session alive?" for the spec's *Lead crash-safe
//! state* reconciliation table and the liveness half of its process-health
//! check. The mechanism is `claude agents --json`, which rung 4 measured
//! enumerates both interactive and background sessions **without a TTY** — the
//! property that lets a plain Rust supervisor ask the question at all.
//!
//! # Liveness only, by measurement
//!
//! Rung 4 measured the limit: `-p` sessions expose **no `status` field**
//! (interactive ones do). Every IC is a `-p` session, so busy-versus-idle is
//! simply unavailable, and the registry gives *liveness* and nothing else.
//! [`AgentEntry::status`] is therefore parsed but deliberately never read by
//! any decision in this crate — it exists so a reader of a captured snapshot
//! can see what was actually there, not as an input.
//! `status_is_never_read_for_liveness` pins that.
//!
//! # The envelope, measured (rung 7, 2026-09-01)
//!
//! Rungs 5 and 6 both shipped a parser for a response shape nobody had
//! captured. This one was captured, by running `claude agents --json` — which
//! costs nothing and writes nothing — before the parser was finished:
//!
//! - A **bare JSON array**. No wrapper object.
//! - Every row carries `name`, `sessionId`, `cwd`, `kind` and `startedAt`.
//! - `kind` is `"background"` or `"interactive"`.
//! - The liveness-ish field is spelled **`state`** on background rows and
//!   **`status`** on interactive ones. Neither is read here; both are
//!   accepted so a captured snapshot shows what was there.
//! - Background rows' `name` is a human sentence ("Fix the approval bug"),
//!   not a slug — which is why [`super::supervise::IC_SESSION_PREFIX`] is a
//!   sound way to tell Autopilot's sessions from the human's own.
//!
//! The `agents`/`sessions` wrapper [`RawRegistry`] also accepts was written
//! before that run and is kept as tolerance for a future shape, not because
//! anything emits it.
//!
//! ## What the same run showed that is *not* good news
//!
//! The listed background sessions included ones started **weeks earlier**.
//! Rung 4 saw the same thing ("`--bg` agents persist for weeks"). So
//! "listed" is a weaker statement than "working", and it is **not known**
//! whether a finished `-p` session lingers in this list — answering that
//! costs a paid dispatch, so it stays unanswered rather than assumed.
//!
//! [`super::supervise`] is built so that the answer does not change whether
//! it is correct, only how quickly it notices: a listed session is never
//! declared dead (the spec forbids it — the ping was answered), but neither
//! is it reported as healthy on listedness alone. See
//! [`super::supervise::ProcessHealth::AliveButStalled`].
//!
//! # The error contract, and why the empty list is not the fallback
//!
//! [`AgentRegistry::list`] mirrors [`super::run::Dispatcher`],
//! [`super::review::ReviewRunner`] and [`super::gh::GhRunner`]: a failure to
//! **start** `claude` is [`MemoryError::NotFound`], and a `claude` that ran and
//! exited non-zero is not an `Err` at all — it is a [`RegistryOutput`] with
//! `success: false`.
//!
//! [`snapshot`] then folds all three failure shapes — spawn failure, non-zero
//! exit, unparseable stdout — into [`RegistrySnapshot::Unavailable`], and
//! that distinction is the whole point of this module:
//!
//! > **An unreadable registry must never degrade to an empty one.**
//!
//! A valid `[]` really does mean every IC is gone, and reconciliation acts on
//! it. But a parse error rendered as an empty list would read as "every IC is
//! dead" — restarting or quarantining every in-flight issue at once, off a
//! JSON shape change. So `Unavailable` yields [`Liveness::Unknown`] for every
//! query, and every caller holds. This is rung 6's lesson 18 (make the
//! unmeasured thing fail toward "unknown", never toward a number) applied to a
//! set rather than a scalar.

use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;

use crate::error::MemoryError;

/// The argv for enumerating sessions. Rung 4 measured this exact form.
pub fn build_argv() -> Vec<String> {
    vec!["agents".to_string(), "--json".to_string()]
}

/// One raw `claude` invocation's result. A non-zero exit is reported here
/// rather than as an `Err` — see the module doc's error contract.
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

/// How [`snapshot`] enumerates sessions. A trait so [`super::supervise`]'s
/// whole reconciliation — which reads and writes real drawers — is testable
/// without spawning `claude`, exactly as the three runner traits before it.
pub trait AgentRegistry {
    /// Run `claude agents --json`. A failure to **spawn** is
    /// [`MemoryError::NotFound`]; a non-zero exit is a successful call
    /// returning `success: false`.
    fn list(&mut self) -> Result<RegistryOutput, MemoryError>;
}

/// The real registry: `claude agents --json`.
pub struct ClaudeAgentRegistry {
    bin: PathBuf,
}

impl ClaudeAgentRegistry {
    /// Resolve `claude` on PATH, reusing `launcher`'s own binary validation
    /// exactly as [`super::dispatch::resolve_claude_binary`],
    /// [`super::review::resolve_codex_binary`] and
    /// [`super::gh::resolve_gh_binary`] do.
    pub fn resolve() -> Result<Self, MemoryError> {
        Ok(Self {
            bin: super::dispatch::resolve_claude_binary()?,
        })
    }
}

impl AgentRegistry for ClaudeAgentRegistry {
    fn list(&mut self) -> Result<RegistryOutput, MemoryError> {
        let args = build_argv();
        let output = Command::new(&self.bin)
            .args(&args)
            .output()
            .map_err(|e| MemoryError::NotFound(format!("failed to start claude agents: {e}")))?;
        Ok(RegistryOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            success: output.status.success(),
        })
    }
}

/// One session as the registry reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentEntry {
    /// The session's `--name`. For an IC this is
    /// [`super::dispatch::ic_name`]'s `ic-<owner>-<repo>-<number>` slug,
    /// which is what makes a session addressable back to an issue.
    pub name: String,
    /// Whichever of the registry's two liveness-ish fields this row carried:
    /// `status` on an interactive session, `state` on a background one, and
    /// neither on the `-p` sessions every IC is (measured, rungs 4 and 7).
    ///
    /// **Never** read as a liveness or progress signal — see the module doc.
    /// It is captured only so a snapshot shows what the registry actually
    /// said.
    pub status: Option<String>,
}

/// A shape-tolerant view of one registry entry.
///
/// `name` is the only required field, because it is the only one any decision
/// depends on. Everything else `claude` may emit is ignored rather than
/// rejected: a registry that grows a field must not become unparseable, since
/// unparseable means [`RegistrySnapshot::Unavailable`] means every supervision
/// decision holds.
#[derive(Debug, Deserialize)]
struct RawAgent {
    name: String,
    /// `state` is the background-session spelling of the same idea; aliased
    /// rather than added as a second field because nothing reads either one,
    /// and two unread fields would only invite someone to start.
    #[serde(default, alias = "state")]
    status: Option<String>,
}

/// The envelope `claude agents --json` may wrap its entries in.
///
/// Accepts a bare array or an object keyed by `agents` or `sessions`. The
/// exact envelope was measured to *work* by rung 4 but its shape was never
/// captured, so this parses by shape rather than pinning one path — the same
/// compensation rung 5's token parser makes for the same reason. A shape none
/// of these match is an error, not an empty list.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawRegistry {
    Bare(Vec<RawAgent>),
    Wrapped {
        #[serde(alias = "sessions")]
        agents: Vec<RawAgent>,
    },
}

/// Parse `claude agents --json` stdout.
///
/// Returns `Err` — never an empty `Vec` — for anything it cannot understand,
/// so [`snapshot`] can tell "no sessions" from "no answer".
pub fn parse_agents(stdout: &str) -> Result<Vec<AgentEntry>, String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err("registry produced no output".to_string());
    }
    let raw: RawRegistry = serde_json::from_str(trimmed)
        .map_err(|e| format!("registry output was not recognizable JSON: {e}"))?;
    let agents = match raw {
        RawRegistry::Bare(agents) => agents,
        RawRegistry::Wrapped { agents } => agents,
    };
    Ok(agents
        .into_iter()
        .map(|a| AgentEntry {
            name: a.name,
            status: a.status,
        })
        .collect())
}

/// What the registry could tell us about one session.
///
/// Three states, not two: "not listed" and "could not ask" are different
/// facts, and collapsing them is the mistake this whole module exists to
/// prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The registry listed this session.
    Alive,
    /// The registry answered, and this session was not in it.
    NotListed,
    /// The registry could not be read at all.
    Unknown,
}

/// One reading of the whole registry.
#[derive(Debug, Clone, PartialEq)]
pub enum RegistrySnapshot {
    Available(Vec<AgentEntry>),
    /// The registry could not be read. Carries why, so a held reconciliation
    /// can say what it is waiting on rather than reporting a bare "unknown".
    Unavailable {
        reason: String,
    },
}

impl RegistrySnapshot {
    /// Liveness of one named session.
    pub fn liveness(&self, name: &str) -> Liveness {
        match self {
            RegistrySnapshot::Unavailable { .. } => Liveness::Unknown,
            RegistrySnapshot::Available(agents) => {
                if agents.iter().any(|a| a.name == name) {
                    Liveness::Alive
                } else {
                    Liveness::NotListed
                }
            }
        }
    }

    /// Every listed session name, or an empty slice when the registry could
    /// not be read.
    ///
    /// Callers enumerating *live* sessions (orphan detection) must check
    /// [`RegistrySnapshot::is_available`] first: an empty result here is
    /// ambiguous by construction, and "no sessions were listed" is not
    /// grounds for concluding anything about sessions that exist.
    pub fn names(&self) -> Vec<&str> {
        match self {
            RegistrySnapshot::Unavailable { .. } => Vec::new(),
            RegistrySnapshot::Available(agents) => agents.iter().map(|a| a.name.as_str()).collect(),
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, RegistrySnapshot::Available(_))
    }
}

/// Read the registry once, folding every failure into
/// [`RegistrySnapshot::Unavailable`].
///
/// Infallible on purpose. A supervisor reconciling ten in-flight issues must
/// not abort all ten because the registry was momentarily unreadable; it must
/// hold on all ten, which is what an `Unavailable` snapshot makes every
/// downstream decision do.
pub fn snapshot(registry: &mut dyn AgentRegistry) -> RegistrySnapshot {
    let output = match registry.list() {
        Ok(output) => output,
        Err(e) => {
            return RegistrySnapshot::Unavailable {
                reason: format!("could not run the session registry: {e}"),
            }
        }
    };
    if !output.success {
        return RegistrySnapshot::Unavailable {
            reason: format!(
                "session registry exited non-zero: {}",
                super::scrub::scrub_and_bound(output.stderr.trim(), 500).text
            ),
        };
    }
    match parse_agents(&output.stdout) {
        Ok(agents) => RegistrySnapshot::Available(agents),
        Err(reason) => RegistrySnapshot::Unavailable { reason },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registry that replays a canned result.
    pub(crate) struct FakeRegistry {
        pub(crate) result: Result<RegistryOutput, MemoryError>,
        pub(crate) calls: u32,
    }

    impl FakeRegistry {
        pub(crate) fn ok(stdout: &str) -> Self {
            Self {
                result: Ok(RegistryOutput {
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                    success: true,
                }),
                calls: 0,
            }
        }
    }

    impl AgentRegistry for FakeRegistry {
        fn list(&mut self) -> Result<RegistryOutput, MemoryError> {
            self.calls += 1;
            match &self.result {
                Ok(output) => Ok(output.clone()),
                Err(e) => Err(MemoryError::Validation(e.to_string())),
            }
        }
    }

    #[test]
    fn argv_is_the_measured_form() {
        assert_eq!(
            build_argv(),
            vec!["agents".to_string(), "--json".to_string()]
        );
    }

    // ── parsing ─────────────────────────────────────────────────────────

    /// The literal shape `claude agents --json` returned on 2026-09-01,
    /// names and paths replaced. Kept verbatim in structure so a future
    /// change to the real envelope fails here rather than in production —
    /// where an unparseable registry takes every supervision decision
    /// offline (safely, but completely).
    const MEASURED_ENVELOPE: &str = r#"[
      {
        "id": "521fb7c9",
        "cwd": "/Users/someone/git-repos/a-project",
        "kind": "background",
        "startedAt": 1783490379236,
        "sessionId": "521fb7c9-8d49-4549-9b66-4c40e9e1992f",
        "name": "Fix a bug in the thing",
        "state": "blocked"
      },
      {
        "pid": 59798,
        "cwd": "/Users/someone/git-repos/ironmem",
        "kind": "interactive",
        "startedAt": 1788305484502,
        "sessionId": "eda9fa23-cd08-4e3a-ad9d-93c7692f0210",
        "name": "ironmem-05",
        "status": "busy"
      },
      {
        "cwd": "/Users/someone/git-repos/ironmem",
        "kind": "background",
        "startedAt": 1788305484502,
        "sessionId": "11111111-2222-3333-4444-555555555555",
        "name": "ic-ironrace-ironmem-283"
      }
    ]"#;

    #[test]
    fn parses_the_measured_registry_envelope() {
        let agents = parse_agents(MEASURED_ENVELOPE).unwrap();
        assert_eq!(agents.len(), 3);
        // Both spellings of the liveness-ish field are captured...
        assert_eq!(agents[0].status.as_deref(), Some("blocked"));
        assert_eq!(agents[1].status.as_deref(), Some("busy"));
        // ...and a row with neither is still a perfectly good entry.
        assert_eq!(agents[2].status, None);

        // ...but only listedness is ever consulted.
        let snap = RegistrySnapshot::Available(agents);
        assert_eq!(snap.liveness("ic-ironrace-ironmem-283"), Liveness::Alive);
        assert_eq!(
            snap.liveness("ic-ironrace-ironmem-999"),
            Liveness::NotListed
        );
    }

    #[test]
    fn a_humans_own_session_name_is_not_mistaken_for_an_ic() {
        // Background rows carry human sentences as names. Nothing about them
        // resembles `ic-<owner>-<repo>-<n>`, which is what makes the prefix
        // test in `supervise::reconcile` sound.
        let agents = parse_agents(MEASURED_ENVELOPE).unwrap();
        let ic_shaped: Vec<&str> = agents
            .iter()
            .map(|a| a.name.as_str())
            .filter(|n| n.starts_with(crate::autopilot::supervise::IC_SESSION_PREFIX))
            .collect();
        assert_eq!(ic_shaped, vec!["ic-ironrace-ironmem-283"]);
    }

    #[test]
    fn parses_a_bare_array() {
        let agents = parse_agents(r#"[{"name":"ic-owner-repo-1"},{"name":"other"}]"#).unwrap();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].name, "ic-owner-repo-1");
    }

    #[test]
    fn parses_an_agents_or_sessions_envelope() {
        let a = parse_agents(r#"{"agents":[{"name":"ic-a-b-1"}]}"#).unwrap();
        assert_eq!(a[0].name, "ic-a-b-1");
        let b = parse_agents(r#"{"sessions":[{"name":"ic-a-b-2"}]}"#).unwrap();
        assert_eq!(b[0].name, "ic-a-b-2");
    }

    #[test]
    fn unknown_fields_do_not_make_the_registry_unparseable() {
        // A `claude` that grows a field must not silently take every
        // supervision decision offline.
        let agents =
            parse_agents(r#"[{"name":"ic-a-b-1","mode":"bypass","created":"yesterday"}]"#).unwrap();
        assert_eq!(agents.len(), 1);
    }

    #[test]
    fn an_entry_without_a_name_is_a_parse_error_not_a_skipped_row() {
        // `name` is the only field any decision depends on. A row missing it
        // means the shape is not what we think it is, and quietly dropping
        // that row would under-report liveness — the dangerous direction.
        assert!(parse_agents(r#"[{"session_id":"abc"}]"#).is_err());
    }

    #[test]
    fn a_valid_empty_array_is_an_empty_list_not_an_error() {
        assert_eq!(parse_agents("[]").unwrap(), Vec::new());
    }

    #[test]
    fn empty_or_garbage_output_is_an_error_not_an_empty_list() {
        assert!(parse_agents("").is_err());
        assert!(parse_agents("   ").is_err());
        assert!(parse_agents("not json at all").is_err());
        assert!(parse_agents(r#"{"unexpected":"envelope"}"#).is_err());
    }

    #[test]
    fn status_is_never_read_for_liveness() {
        // Rung 4 measured that `-p` sessions carry no `status`. Liveness is
        // listedness, so an entry with no status is just as alive as one
        // with a status of anything at all.
        let agents =
            parse_agents(r#"[{"name":"ic-a-b-1"},{"name":"human","status":"idle"}]"#).unwrap();
        assert_eq!(agents[0].status, None);
        assert_eq!(agents[1].status.as_deref(), Some("idle"));
        let snap = RegistrySnapshot::Available(agents);
        assert_eq!(snap.liveness("ic-a-b-1"), Liveness::Alive);
        assert_eq!(snap.liveness("human"), Liveness::Alive);
    }

    // ── snapshot: the three failure shapes all fold to Unavailable ───────

    #[test]
    fn a_valid_empty_registry_is_available_and_reports_not_listed() {
        let mut registry = FakeRegistry::ok("[]");
        let snap = snapshot(&mut registry);
        assert!(snap.is_available());
        assert_eq!(snap.liveness("ic-a-b-1"), Liveness::NotListed);
    }

    #[test]
    fn a_spawn_failure_is_unavailable_not_an_empty_registry() {
        struct Failing;
        impl AgentRegistry for Failing {
            fn list(&mut self) -> Result<RegistryOutput, MemoryError> {
                Err(MemoryError::NotFound("claude not on PATH".into()))
            }
        }
        let snap = snapshot(&mut Failing);
        assert!(!snap.is_available());
        assert_eq!(snap.liveness("ic-a-b-1"), Liveness::Unknown);
    }

    #[test]
    fn a_non_zero_exit_is_unavailable() {
        struct Failing;
        impl AgentRegistry for Failing {
            fn list(&mut self) -> Result<RegistryOutput, MemoryError> {
                Ok(RegistryOutput {
                    stdout: String::new(),
                    stderr: "not logged in".into(),
                    success: false,
                })
            }
        }
        let snap = snapshot(&mut Failing);
        match &snap {
            RegistrySnapshot::Unavailable { reason } => assert!(reason.contains("not logged in")),
            other => panic!("expected Unavailable, got {other:?}"),
        }
        assert_eq!(snap.liveness("ic-a-b-1"), Liveness::Unknown);
    }

    #[test]
    fn unparseable_output_is_unavailable_never_an_empty_list() {
        // The single most important behavior in this module: a JSON shape
        // change must not read as "every IC is dead".
        let mut registry = FakeRegistry::ok("<html>login required</html>");
        let snap = snapshot(&mut registry);
        assert!(!snap.is_available());
        assert_eq!(snap.liveness("ic-a-b-1"), Liveness::Unknown);
        assert!(snap.names().is_empty());
    }

    #[test]
    fn a_non_zero_exit_scrubs_its_stderr_before_reporting_it() {
        // The reason string is surfaced to a CLI and persisted in a
        // supervision record, so it goes through the same scrub every other
        // Autopilot write path uses.
        struct Failing;
        impl AgentRegistry for Failing {
            fn list(&mut self) -> Result<RegistryOutput, MemoryError> {
                Ok(RegistryOutput {
                    stdout: String::new(),
                    stderr: "auth failed for ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
                    success: false,
                })
            }
        }
        match snapshot(&mut Failing) {
            RegistrySnapshot::Unavailable { reason } => {
                assert!(!reason.contains("ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn names_are_empty_for_an_unavailable_registry_and_is_available_says_so() {
        struct Failing;
        impl AgentRegistry for Failing {
            fn list(&mut self) -> Result<RegistryOutput, MemoryError> {
                Err(MemoryError::NotFound("gone".into()))
            }
        }
        let snap = snapshot(&mut Failing);
        assert!(snap.names().is_empty());
        assert!(
            !snap.is_available(),
            "an empty names() must be distinguishable from a genuinely empty registry"
        );
    }
}

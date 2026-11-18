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
//! The gate config is read by [`read_gate_config_status`] through
//! [`ReadOnlyDb`], a type with no write or migrate method, over a connection
//! opened `SQLITE_OPEN_READ_ONLY`. The labels are read with one
//! `gh label list`. Nothing here writes the database file or creates a drawer
//! or a label, though a read-only connection to a WAL-mode database may create
//! the `-wal`/`-shm` side files.
//!
//! The label half is read-only because this module only ever builds a
//! `label list` argv, not because a type forbids more.
//!
//! # A failed read is never a missing label
//!
//! [`LabelStatus`] is an enum, not a struct of optional fields: when `gh` is
//! missing or could not be started, exited non-zero, printed output that is
//! not the expected JSON, returned a label row with no name, returned a list
//! long enough to have been cut off, or was never run because `--path` is not
//! a directory, the status is [`LabelStatus::Failed`] and carries no label
//! answers at all. Its JSON has no `agent` or `risk` key, so a consumer must
//! check `read` first; a strict lookup finds nothing, though a loose one (a
//! missing key read as false) can still be fooled, which is why `read` exists.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use crate::db::read_only::ReadOnlyDb;
use crate::db::schema::Database;
use crate::error::MemoryError;

use super::gate_config::{get_gate_config_read_only, GateConfigState};
use super::gh::{repo_labels, GhRunner};
use super::labels::AgentLabel;
use super::validate_repo;

/// Everything `ironmem autopilot status` reports for one repo. Its
/// serialized form is the command's `--json` output, a contract the
/// `/evaluate-issue` router reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoStatus {
    repo: String,
    /// The absolute path of the database file the gate config was read from.
    db: String,
    /// Whether a gate config exists at all, in any state. Always equal to
    /// `gate_config.is_some()`; stated so a reader need not infer it.
    onboarded: bool,
    /// `None` (JSON `null`) for a repo that was never onboarded.
    gate_config: Option<GateConfigStatus>,
    labels: LabelStatus,
}

impl RepoStatus {
    /// The one place `onboarded` is derived, so it cannot disagree with
    /// `gate_config`.
    pub fn new(
        repo: &str,
        db: &Path,
        gate_config: Option<GateConfigStatus>,
        labels: LabelStatus,
    ) -> Self {
        Self {
            repo: repo.to_string(),
            // Absolute, so the report names one file whatever the cwd.
            db: std::path::absolute(db)
                .unwrap_or_else(|_| db.to_path_buf())
                .display()
                .to_string(),
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
    /// Problems a human must read before approving; empty when there are none.
    pub manifest_warnings: Vec<String>,
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

    /// Match the repo's label names by name — trimmed and case-insensitive — never by search.
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
            .filter(|name| super::queue::risk_class_of(name).is_some())
            .cloned()
            .collect();
        risk.sort_by_cached_key(|name| (name.to_ascii_lowercase(), name.clone()));
        Self::Ok { agent, risk }
    }
}

/// Refuse a string that is not shaped like a repo name. The shape is
/// `owner/name` or `host/owner/name`, each part non-empty, made only of ASCII
/// letters, digits, `.`, `_` and `-`, and not starting with `-`. This does not
/// know every name `gh` refuses, and letter case cannot be checked here, so
/// `OWNER/REPO` is simply not onboarded.
fn check_repo_shape(repo: &str) -> Result<(), MemoryError> {
    validate_repo(repo)?;
    let segments: Vec<&str> = repo.split('/').collect();
    let segment_ok = |segment: &&str| {
        !segment.is_empty()
            && !segment.starts_with('-')
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    if (2..=3).contains(&segments.len()) && segments.iter().all(segment_ok) {
        Ok(())
    } else {
        Err(MemoryError::Validation(format!(
            "'{repo}' is not a repo name — expected `owner/name` (or `host/owner/name`), each \
             part made of letters, digits, `.`, `_` and `-`, and not starting with `-`"
        )))
    }
}

/// Read `repo`'s gate config from the database at `db_path`, opening it
/// read-only. The repo is checked before the database is touched, so a bad
/// repo is reported even when the database is missing, and a SQLite failure
/// is explained rather than passed through bare.
pub fn read_gate_config_status(
    db_path: &Path,
    repo: &str,
) -> Result<Option<GateConfigStatus>, MemoryError> {
    check_repo_shape(repo)?;
    let explain = |err: MemoryError| match err {
        MemoryError::Db(e) => explain_db_error(db_path, &e),
        other => other,
    };
    let db = match Database::open_read_only(db_path) {
        // `open_read_only` calls every `CannotOpen` "not found"; a file that
        // is there but cannot be opened is a different problem.
        Err(MemoryError::NotFound(_)) if db_path.exists() => {
            return Err(MemoryError::Io(std::io::Error::other(format!(
                "could not open the database at {}: the file exists but could not be opened — \
                 check its permissions and that it is a file",
                db_path.display()
            ))));
        }
        other => other.map_err(explain)?,
    };
    gate_config_status(&db, repo).map_err(explain)
}

/// An I/O failure naming `db_path`, with a hint only for the SQLite errors the
/// hint is true of. A busy or locked database gets neither hint.
fn explain_db_error(db_path: &Path, e: &rusqlite::Error) -> MemoryError {
    let mut message = format!(
        "could not read the database at {}: {e}. `status` opens it read-only and never \
         writes it.",
        db_path.display()
    );
    if let rusqlite::Error::SqliteFailure(failure, detail) = e {
        // A foreign or empty SQLite file arrives as a generic `SQLITE_ERROR`
        // (code `Unknown`) whose message is "no such table: drawers".
        let no_such_table = detail
            .as_deref()
            .is_some_and(|text| text.starts_with("no such table"));
        match failure.code {
            rusqlite::ErrorCode::ReadOnly => message.push_str(
                " A WAL-mode database whose -wal/-shm side files are missing can only be read \
                 from a directory SQLite can create them in.",
            ),
            code if code == rusqlite::ErrorCode::NotADatabase || no_such_table => message
                .push_str(" If this is not an ironmem database, pass the right one with --db."),
            _ => {}
        }
    }
    MemoryError::Io(std::io::Error::other(message))
}

/// Read `repo`'s gate config, whatever state it is in. `Ok(None)` is a repo
/// that was never onboarded; an `Err` is a stored config that does not parse
/// or that belongs to another repo name. The caller has already checked the
/// repo's shape ([`read_gate_config_status`]).
fn gate_config_status(
    db: &ReadOnlyDb,
    repo: &str,
) -> Result<Option<GateConfigStatus>, MemoryError> {
    let Some(config) = get_gate_config_read_only(db, repo)? else {
        return Ok(None);
    };
    if config.repo != repo {
        return Err(MemoryError::Validation(format!(
            "the gate config stored under this key belongs to '{}', not '{repo}' — two repo \
             names that reduce to the same storage key share one config, so this status cannot \
             be reported",
            config.repo
        )));
    }
    let gate_commands = config.gate_commands().to_vec();
    Ok(Some(GateConfigStatus {
        state: config.state,
        gate_commands,
        manifest_warnings: config.manifest_warnings,
        proposed_at: config.proposed_at,
        approved_at: config.approved_at,
    }))
}

/// `arg` quoted for a POSIX shell: unchanged when it is non-empty and made only
/// of characters a shell never treats specially, otherwise in single quotes
/// with each `'` written as `'\''`.
fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | ':' | '@' | '-'));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// The human-readable report. When `hint_db` is true (the command was run
/// with `--db`), the `onboard` and `approve` hints end with `--db` and the
/// report's absolute database path, so they act on the database that was
/// read wherever they are run from. The `labels` hint takes no `--db`.
pub fn render_text(status: &RepoStatus, hint_db: bool) -> String {
    let repo = &status.repo;
    let quoted_repo = shell_quote(repo);
    let db_flag = if hint_db {
        format!(" --db {}", shell_quote(&status.db))
    } else {
        String::new()
    };
    let mut out = String::new();
    match &status.gate_config {
        None => out.push_str(&format!(
            "{repo}: not onboarded — no gate config. Run `ironmem autopilot onboard \
             {quoted_repo} --path .{db_flag}` from the repo's checkout first.\n"
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
            if !config.manifest_warnings.is_empty() {
                out.push_str(match config.state {
                    GateConfigState::Pending => "  warnings (read these before approving):\n",
                    GateConfigState::Approved => "  warnings recorded on the proposal:\n",
                });
                for warning in &config.manifest_warnings {
                    out.push_str(&format!("    - {warning}\n"));
                }
            }
            out.push_str(&format!("  proposed: {}\n", config.proposed_at));
            match &config.approved_at {
                Some(at) => out.push_str(&format!("  approved: {at}\n")),
                None => out.push_str(&format!(
                    "  approved: not yet — run `ironmem autopilot approve {quoted_repo}{db_flag}`\n"
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
            for (label, present) in agent {
                let word = if *present { "present" } else { "MISSING" };
                out.push_str(&format!("  {label} — {word}\n"));
            }
            if risk.is_empty() {
                out.push_str("  risk:* — none\n");
            } else {
                out.push_str(&format!("  risk:* — {}\n", risk.join(", ")));
            }
            if agent.values().any(|present| !present) {
                out.push_str(&format!(
                    "  create the missing agent:* labels with `ironmem autopilot labels \
                     {quoted_repo}`\n"
                ));
            }
        }
    }
    out.push_str(&format!("Database: {}\n", status.db));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::gate_config::{approve_gate_config, propose_gate_config};
    use crate::autopilot::gh::testing::ScriptedGh;
    use crate::autopilot::gh::{label_list_argv, GhOutput};
    use crate::autopilot::testing::on_disk_db;

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
                manifest_warnings: vec![],
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
        let err = read_gate_config_status(&path, "  ").unwrap_err();
        assert!(matches!(err, MemoryError::Validation(_)), "{err}");
    }

    #[test]
    fn a_malformed_repo_name_is_refused_not_reported_as_not_onboarded() {
        let (_dir, path) = on_disk_db();
        for bad in [
            "not a repo",
            "ironmem",
            "owner/",
            "/name",
            "a/b/c/d",
            "owner /name",
            "-R/x",
            "owner/-x",
            "owner/re$po;x",
            "owner/it's",
            "owner/repo\\n",
        ] {
            let err = read_gate_config_status(&path, bad).unwrap_err();
            match err {
                MemoryError::Validation(message) => {
                    assert!(message.contains("is not a repo name"), "{bad}: {message}");
                    assert!(
                        message.contains("not starting with `-`"),
                        "{bad}: {message}"
                    );
                }
                other => panic!("{bad}: expected Validation, got {other:?}"),
            }
        }
        for good in [
            "owner/repo",
            "my-org/my.repo_2",
            "ghe.example.com/owner/repo",
        ] {
            assert_eq!(
                read_gate_config_status(&path, good).unwrap(),
                None,
                "{good}"
            );
        }
    }

    #[test]
    fn reading_by_path_explains_a_file_that_is_not_a_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        std::fs::write(
            &path,
            "this is not sqlite, it is a text file of some length\n".repeat(50),
        )
        .unwrap();
        let err = read_gate_config_status(&path, "owner/repo").unwrap_err();
        match err {
            MemoryError::Io(io) => {
                let message = io.to_string();
                assert!(
                    message.contains("could not read the database at"),
                    "{message}"
                );
                assert!(message.contains(&path.display().to_string()), "{message}");
                assert!(message.contains("never writes it"), "{message}");
                assert!(
                    message.contains("pass the right one with --db"),
                    "{message}"
                );
                assert!(!message.contains("-wal/-shm"), "{message}");
            }
            other => panic!("expected the mapped Io error, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_sqlite_file_is_explained_as_not_an_ironmem_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.sqlite3");
        rusqlite::Connection::open(&path).unwrap();
        let err = read_gate_config_status(&path, "owner/repo").unwrap_err();
        match err {
            MemoryError::Io(io) => {
                let message = io.to_string();
                assert!(message.contains("no such table"), "{message}");
                assert!(
                    message.contains("pass the right one with --db"),
                    "{message}"
                );
                assert!(!message.contains("-wal/-shm"), "{message}");
            }
            other => panic!("expected the mapped Io error, got {other:?}"),
        }
    }

    #[test]
    fn a_database_file_that_cannot_be_opened_is_not_reported_as_missing() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, path) = on_disk_db();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Running as root, the file opens anyway and the case cannot arise.
        if std::fs::File::open(&path).is_ok() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            return;
        }
        let result = read_gate_config_status(&path, "owner/repo");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        match result.unwrap_err() {
            MemoryError::Io(io) => {
                let message = io.to_string();
                assert!(
                    message.contains("could not open the database at"),
                    "{message}"
                );
                assert!(
                    message.contains("the file exists but could not be opened"),
                    "{message}"
                );
                assert!(!message.contains("db not found"), "{message}");
            }
            other => panic!("expected the Io error, got {other:?}"),
        }
    }

    #[test]
    fn a_read_only_failure_gets_the_wal_sentence_and_a_locked_one_gets_neither() {
        let readonly = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_READONLY),
            Some("attempt to write a readonly database".into()),
        );
        let message = explain_db_error(Path::new("/x/m.db"), &readonly).to_string();
        assert!(message.contains("-wal/-shm"), "{message}");
        assert!(!message.contains("pass the right one"), "{message}");

        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        );
        let message = explain_db_error(Path::new("/x/m.db"), &busy).to_string();
        assert!(message.contains("database is locked"), "{message}");
        assert!(!message.contains("-wal/-shm"), "{message}");
        assert!(!message.contains("pass the right one"), "{message}");
    }

    #[test]
    fn a_relative_database_path_is_reported_as_an_absolute_one() {
        let status = RepoStatus::new(
            "owner/repo",
            Path::new("m.db"),
            None,
            LabelStatus::failed("x"),
        );
        assert!(Path::new(&status.db).is_absolute(), "{}", status.db);
        assert!(status.db.ends_with("m.db"), "{}", status.db);
    }

    #[test]
    fn a_repo_padded_with_whitespace_is_refused() {
        let (_dir, path) = on_disk_db();
        for bad in [" owner/repo", "owner/repo "] {
            assert!(
                matches!(
                    read_gate_config_status(&path, bad),
                    Err(MemoryError::Validation(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn reading_by_path_keeps_not_found_for_a_missing_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.sqlite3");
        let err = read_gate_config_status(&path, "owner/repo").unwrap_err();
        match err {
            MemoryError::NotFound(message) => {
                assert!(message.contains("db not found at"), "{message}")
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        assert!(!path.exists());
    }

    #[test]
    fn reading_by_path_checks_the_repo_before_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.sqlite3");
        let err = read_gate_config_status(&path, "not a repo").unwrap_err();
        match err {
            MemoryError::Validation(message) => {
                assert!(message.contains("is not a repo name"), "{message}")
            }
            other => panic!("expected the repo error, got {other:?}"),
        }
    }

    #[test]
    fn reading_by_path_reports_a_stored_config() {
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            propose_gate_config(&db, "owner/repo", vec!["cargo test".into()], vec![]).unwrap();
        }
        let status = read_gate_config_status(&path, "owner/repo")
            .unwrap()
            .unwrap();
        assert_eq!(status.gate_commands, vec!["cargo test".to_string()]);
    }

    #[test]
    fn the_text_report_ends_with_the_database_that_answered() {
        let status = RepoStatus::new(
            "owner/repo",
            Path::new("/tmp/mem.db"),
            None,
            read_labels(&[]),
        );
        let text = render_text(&status, false);
        assert!(text.ends_with("Database: /tmp/mem.db\n"), "{text}");
    }

    #[test]
    fn a_stored_config_that_does_not_parse_is_an_error_not_a_status() {
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            crate::autopilot::write_current(
                &db,
                &crate::autopilot::gate_config::gate_config_key("owner/repo"),
                r#"{"repo":"owner/repo","state":"approved","gate_commands":[],"proposed_at":"2026-01-01T00:00:00+00:00","approved_at":null}"#,
            )
            .unwrap();
        }

        let err = read_gate_config_status(&path, "owner/repo").unwrap_err();

        assert!(matches!(err, MemoryError::Json(_)), "{err:?}");
        assert!(
            err.to_string()
                .contains(crate::autopilot::EMPTY_GATE_COMMANDS_MSG),
            "{err}"
        );
    }

    #[test]
    fn a_config_stored_under_a_colliding_key_is_refused() {
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            propose_gate_config(&db, "foo-bar/baz", vec!["cargo test".into()], vec![]).unwrap();
        }

        let ro = Database::open_read_only(&path).unwrap();
        let err = gate_config_status(&ro, "foo/bar-baz").unwrap_err();

        match err {
            MemoryError::Validation(message) => {
                assert!(message.contains("'foo-bar/baz'"), "{message}");
                assert!(message.contains("'foo/bar-baz'"), "{message}");
            }
            other => panic!("expected a Validation error, got {other:?}"),
        }
    }

    #[test]
    fn risk_labels_sort_case_insensitively_where_a_byte_sort_would_differ() {
        // A byte sort would put the capital `Z` first.
        let status = read_labels(&["Risk:Zeta", "risk:alpha"]);
        assert_eq!(
            status,
            LabelStatus::Ok {
                agent: agent_map(false, false, false),
                risk: vec!["risk:alpha".into(), "Risk:Zeta".into()],
            }
        );
    }

    // ── the report ──────────────────────────────────────────────────────

    #[test]
    fn a_repo_that_is_not_onboarded_serializes_a_null_gate_config() {
        let status = RepoStatus::new(
            "owner/repo",
            Path::new("/tmp/mem.db"),
            None,
            LabelStatus::failed("boom"),
        );
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            serde_json::json!({
                "repo": "owner/repo",
                "db": "/tmp/mem.db",
                "onboarded": false,
                "gate_config": null,
                "labels": { "read": "failed", "error": "boom" },
            })
        );
    }

    #[test]
    fn the_text_report_says_not_onboarded_and_that_labels_were_not_read() {
        let text = render_text(
            &RepoStatus::new(
                "owner/repo",
                Path::new("/tmp/mem.db"),
                None,
                LabelStatus::failed("gh label list on owner/repo failed (exit Some(1)): HTTP 401"),
            ),
            false,
        );
        assert!(text.contains("owner/repo: not onboarded"), "{text}");
        assert!(
            text.contains(
                "Run `ironmem autopilot onboard owner/repo --path .` from the repo's checkout first."
            ),
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
        let text = render_text(
            &RepoStatus::new(
                "owner/repo",
                Path::new("/tmp/mem.db"),
                Some(config_status(GateConfigState::Pending, vec![], None)),
                read_labels(&["agent:ready", "agent:blocked", "risk:logic"]),
            ),
            false,
        );
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

    fn config_status(
        state: GateConfigState,
        warnings: Vec<String>,
        approved_at: Option<&str>,
    ) -> GateConfigStatus {
        GateConfigStatus {
            state,
            gate_commands: vec!["cargo test".into()],
            manifest_warnings: warnings,
            proposed_at: "2026-01-01T00:00:00+00:00".into(),
            approved_at: approved_at.map(str::to_string),
        }
    }

    #[test]
    fn stored_manifest_warnings_are_reported() {
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            propose_gate_config(
                &db,
                "owner/repo",
                vec!["cargo test".into()],
                vec!["manifest X could not be read".into()],
            )
            .unwrap();
        }

        let ro = Database::open_read_only(&path).unwrap();
        let status = gate_config_status(&ro, "owner/repo").unwrap().unwrap();

        assert_eq!(
            status.manifest_warnings,
            vec!["manifest X could not be read".to_string()]
        );
    }

    #[test]
    fn the_text_report_prints_manifest_warnings_after_the_commands() {
        let text = render_text(
            &RepoStatus::new(
                "owner/repo",
                Path::new("/tmp/mem.db"),
                Some(config_status(
                    GateConfigState::Pending,
                    vec!["manifest X could not be read".into()],
                    None,
                )),
                read_labels(&[]),
            ),
            false,
        );
        assert!(
            text.contains(
                "  - cargo test\n  warnings (read these before approving):\n    - manifest X could not be read\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn an_approved_config_labels_its_warnings_as_recorded_not_as_pre_approval() {
        let text = render_text(
            &RepoStatus::new(
                "owner/repo",
                Path::new("/tmp/mem.db"),
                Some(config_status(
                    GateConfigState::Approved,
                    vec!["manifest X could not be read".into()],
                    Some("2026-02-02T00:00:00+00:00"),
                )),
                read_labels(&[]),
            ),
            false,
        );
        assert!(
            text.contains(
                "  warnings recorded on the proposal:\n    - manifest X could not be read\n"
            ),
            "{text}"
        );
        assert!(!text.contains("before approving"), "{text}");
    }

    #[test]
    fn the_text_report_has_no_warnings_block_when_there_are_none() {
        let text = render_text(
            &RepoStatus::new(
                "owner/repo",
                Path::new("/tmp/mem.db"),
                Some(config_status(GateConfigState::Pending, vec![], None)),
                read_labels(&[]),
            ),
            false,
        );
        assert!(!text.contains("warnings"), "{text}");
    }

    #[test]
    fn a_gate_config_status_serializes_every_key() {
        assert_eq!(
            serde_json::to_value(config_status(
                GateConfigState::Pending,
                vec!["w".into()],
                None
            ))
            .unwrap(),
            serde_json::json!({
                "state": "pending",
                "gate_commands": ["cargo test"],
                "manifest_warnings": ["w"],
                "proposed_at": "2026-01-01T00:00:00+00:00",
                "approved_at": null,
            })
        );
    }

    #[test]
    fn a_full_repo_status_serializes_the_pinned_shape() {
        let status = RepoStatus::new(
            "owner/repo",
            Path::new("/tmp/mem.db"),
            Some(config_status(GateConfigState::Pending, vec![], None)),
            read_labels(&["agent:ready", "risk:logic"]),
        );
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            serde_json::json!({
                "repo": "owner/repo",
                "db": "/tmp/mem.db",
                "onboarded": true,
                "gate_config": {
                    "state": "pending",
                    "gate_commands": ["cargo test"],
                    "manifest_warnings": [],
                    "proposed_at": "2026-01-01T00:00:00+00:00",
                    "approved_at": null,
                },
                "labels": {
                    "read": "ok",
                    "agent": {
                        "agent:ready": true,
                        "agent:blocked": false,
                        "agent:exhausted": false,
                    },
                    "risk": ["risk:logic"],
                },
            })
        );
    }

    #[test]
    fn the_text_report_for_an_approved_config_shows_the_approval_time() {
        let text = render_text(
            &RepoStatus::new(
                "owner/repo",
                Path::new("/tmp/mem.db"),
                Some(config_status(
                    GateConfigState::Approved,
                    vec![],
                    Some("2026-02-02T00:00:00+00:00"),
                )),
                read_labels(&[]),
            ),
            false,
        );
        assert!(text.contains("owner/repo: gate config approved"), "{text}");
        assert!(
            text.contains("  approved: 2026-02-02T00:00:00+00:00"),
            "{text}"
        );
        assert!(!text.contains("not yet"), "{text}");
    }

    #[test]
    fn an_empty_label_list_is_a_read_with_every_label_missing() {
        let status = read_labels(&[]);
        assert_eq!(
            status,
            LabelStatus::Ok {
                agent: agent_map(false, false, false),
                risk: vec![],
            }
        );
        let text = render_text(
            &RepoStatus::new("owner/repo", Path::new("/tmp/mem.db"), None, status),
            false,
        );
        assert!(text.contains("  risk:* — none"), "{text}");
        assert!(text.contains("  agent:ready — MISSING"), "{text}");
    }

    #[test]
    fn gh_output_that_is_not_json_is_a_failed_read() {
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::ok("this is not json"))]);
        let status = LabelStatus::read(&mut gh, "owner/repo");
        assert!(matches!(status, LabelStatus::Failed { .. }), "{status:?}");
    }

    #[test]
    fn an_unrecognized_risk_class_is_still_listed() {
        let status = read_labels(&["risk:banana"]);
        assert_eq!(
            status,
            LabelStatus::Ok {
                agent: agent_map(false, false, false),
                risk: vec!["risk:banana".into()],
            }
        );
    }

    #[test]
    fn hints_carry_the_db_the_command_was_run_with_quoted() {
        let status = RepoStatus::new(
            "owner/repo",
            Path::new("/tmp/x y.sqlite3"),
            Some(config_status(GateConfigState::Pending, vec![], None)),
            read_labels(&[]),
        );
        let text = render_text(&status, true);
        assert!(
            text.contains("ironmem autopilot approve owner/repo --db '/tmp/x y.sqlite3'"),
            "{text}"
        );

        let not_onboarded = RepoStatus::new(
            "owner/repo",
            Path::new("/tmp/x y.sqlite3"),
            None,
            read_labels(&[]),
        );
        let text = render_text(&not_onboarded, true);
        assert!(
            text.contains(
                "ironmem autopilot onboard owner/repo --path . --db '/tmp/x y.sqlite3'` from the repo's checkout first."
            ),
            "{text}"
        );
        assert!(!text.contains('<') && !text.contains('>'), "{text}");
    }

    #[test]
    fn a_relative_database_is_hinted_by_its_absolute_path() {
        let status = RepoStatus::new(
            "owner/repo",
            Path::new("rel.sqlite3"),
            None,
            read_labels(&[]),
        );
        let text = render_text(&status, true);
        let absolute = std::path::absolute("rel.sqlite3").unwrap();
        assert!(
            text.contains(&format!(
                "--db {}`",
                shell_quote(&absolute.display().to_string())
            )),
            "{text}"
        );
        assert!(!text.contains("--db rel.sqlite3"), "{text}");
    }

    #[test]
    fn no_db_is_hinted_when_none_was_given() {
        let status = RepoStatus::new(
            "owner/repo",
            Path::new("/tmp/mem.db"),
            None,
            read_labels(&[]),
        );
        let text = render_text(&status, false);
        assert!(!text.contains("--db"), "{text}");
        assert!(
            text.contains(
                "Run `ironmem autopilot onboard owner/repo --path .` from the repo's checkout first."
            ),
            "{text}"
        );
    }

    #[test]
    fn a_missing_agent_label_gets_a_create_hint_without_a_db() {
        let status = RepoStatus::new(
            "owner/repo",
            Path::new("/tmp/mem.db"),
            None,
            read_labels(&["agent:ready"]),
        );
        let text = render_text(&status, true);
        assert!(
            text.contains(
                "  create the missing agent:* labels with `ironmem autopilot labels owner/repo`\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn no_create_hint_when_no_label_is_missing_or_the_read_failed() {
        let all = RepoStatus::new(
            "owner/repo",
            Path::new("/tmp/mem.db"),
            None,
            read_labels(&["agent:ready", "agent:blocked", "agent:exhausted"]),
        );
        assert!(!render_text(&all, false).contains("create the missing"));
        let failed = RepoStatus::new(
            "owner/repo",
            Path::new("/tmp/mem.db"),
            None,
            LabelStatus::failed("boom"),
        );
        assert!(!render_text(&failed, false).contains("create the missing"));
    }

    #[test]
    fn the_label_lines_follow_the_map_and_never_invent_a_missing_entry() {
        let status = RepoStatus::new(
            "owner/repo",
            Path::new("/tmp/mem.db"),
            None,
            LabelStatus::Ok {
                agent: BTreeMap::from([("agent:ready", true)]),
                risk: vec![],
            },
        );
        let text = render_text(&status, false);
        assert!(text.contains("  agent:ready — present"), "{text}");
        assert!(!text.contains("agent:blocked"), "{text}");
    }

    #[test]
    fn shell_quote_quotes_only_what_needs_it() {
        assert_eq!(shell_quote("owner/repo"), "owner/repo");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        // zsh expands a leading `=word` to a command path.
        assert_eq!(shell_quote("=x"), "'=x'");
        assert_eq!(shell_quote("a=b"), "'a=b'");
    }
}

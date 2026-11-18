//! Public-CLI coverage for `ironmem autopilot status` (#420).
//!
//! `gh` is a local stub on an isolated PATH. It answers `label list` and
//! refuses everything else, so nothing here can reach GitHub and a `status`
//! that tried to create or edit a label would fail these tests.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ironmem::autopilot::gate_config::{self, GateConfig};
use ironmem::db::schema::Database;
use serde_json::{json, Value};

mod autopilot_support;
use autopilot_support::{executable, ironmem, isolated_path, REPO};

/// Bash builtins only, so it runs on a PATH that holds nothing else.
const GH_STUB: &str = r#"#!/bin/bash
echo "gh $*" >> "$GH_LOG"
pwd > "$GH_LOG.cwd"
if [ "$1 $2" != "label list" ]; then
  echo "status may only list labels, got: gh $*" >&2
  exit 97
fi
if [ -n "$GH_FAIL" ]; then
  echo "$GH_FAIL" >&2
  exit 1
fi
printf '%s\n' "$GH_LABELS"
"#;

/// Every label Autopilot needs, plus two that only resemble them.
const ALL_LABELS: &str = r#"[{"name":"bug"},{"name":"agent:blocked"},{"name":"agent:ready"},{"name":"agent:exhausted"},{"name":"risk:logic"},{"name":"risk:documentation"},{"name":"sev:critical"}]"#;

/// The one `gh` call a status run may make.
const EXPECTED_GH_LOG: &str = "gh label list --repo owner/repo --limit 1000 --json name\n";

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let stubs = root.path().join("stubs");
        std::fs::create_dir_all(&stubs).unwrap();
        executable(&stubs.join("gh"), GH_STUB);
        Self { root }
    }

    fn db_path(&self) -> PathBuf {
        self.root.path().join("memory.sqlite3")
    }

    fn log_path(&self) -> PathBuf {
        self.root.path().join("gh.log")
    }

    fn stubs(&self) -> PathBuf {
        self.root.path().join("stubs")
    }

    /// Open (creating and migrating if needed) the fixture database.
    fn writable_db(&self) -> Database {
        let db = Database::open(&self.db_path()).unwrap();
        db.migrate().unwrap();
        db
    }

    fn propose(&self, repo: &str, commands: &[&str]) {
        let commands = commands.iter().map(|c| c.to_string()).collect();
        gate_config::propose_gate_config(&self.writable_db(), repo, commands, Vec::new()).unwrap();
    }

    fn approve(&self, repo: &str) {
        gate_config::approve_gate_config(&self.writable_db(), repo).unwrap();
    }

    fn stored(&self, repo: &str) -> GateConfig {
        gate_config::get_gate_config(&self.writable_db(), repo)
            .unwrap()
            .unwrap()
    }

    /// The one place a run's environment is set: `path_var` as `PATH`, the
    /// stub answering `labels` (or failing with `fail` on stderr), and the
    /// ambient `IRONMEM_*` settings removed so a developer's shell cannot
    /// change the result.
    fn env(&self, command: &mut Command, path_var: &Path, labels: &str, fail: Option<&str>) {
        command
            .env("PATH", path_var)
            .env("GH_LOG", self.log_path())
            .env("GH_LABELS", labels)
            .env_remove("IRONMEM_DB_PATH")
            .env_remove("IRONMEM_MCP_MODE")
            .env_remove("IRONMEM_EMBED_MODE");
        if let Some(message) = fail {
            command.env("GH_FAIL", message);
        } else {
            command.env_remove("GH_FAIL");
        }
    }

    /// `ironmem autopilot status <repo> --db <db> --path <path_arg>` (the text
    /// report), in the environment [`Fixture::env`] sets.
    fn plain(
        &self,
        repo: &str,
        db: &Path,
        path_arg: &Path,
        path_var: &Path,
        labels: &str,
        fail: Option<&str>,
    ) -> Command {
        let mut command = ironmem();
        command
            .args(["autopilot", "status", repo])
            .arg("--db")
            .arg(db)
            .arg("--path")
            .arg(path_arg);
        self.env(&mut command, path_var, labels, fail);
        command
    }

    /// The `--json` form of [`Fixture::plain`].
    fn command(
        &self,
        repo: &str,
        db: &Path,
        path_arg: &Path,
        path_var: &Path,
        labels: &str,
        fail: Option<&str>,
    ) -> Command {
        let mut command = self.plain(repo, db, path_arg, path_var, labels, fail);
        command.arg("--json");
        command
    }

    /// `ironmem autopilot status owner/repo --json` against `db`.
    fn status_at(&self, db: &Path, path_var: &Path, labels: &str, fail: Option<&str>) -> Output {
        self.command(REPO, db, self.root.path(), path_var, labels, fail)
            .output()
            .unwrap()
    }

    fn status(&self, labels: &str, fail: Option<&str>) -> Output {
        self.status_at(&self.db_path(), &isolated_path(&self.stubs()), labels, fail)
    }

    fn gh_log(&self) -> String {
        std::fs::read_to_string(self.log_path()).unwrap_or_default()
    }
}

/// The report a successful run printed.
fn report(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "status exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout is not one JSON document ({err}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn an_approved_repo_with_every_label_reports_the_pinned_json() {
    let fixture = Fixture::new();
    fixture.propose(
        REPO,
        &["cargo fmt --all -- --check", "cargo test --workspace"],
    );
    fixture.approve(REPO);
    let stored = fixture.stored(REPO);

    let output = fixture.status(ALL_LABELS, None);

    assert_eq!(
        report(&output),
        json!({
            "repo": "owner/repo",
            "db": fixture.db_path().display().to_string(),
            "onboarded": true,
            "gate_config": {
                "state": "approved",
                "gate_commands": ["cargo fmt --all -- --check", "cargo test --workspace"],
                "manifest_warnings": [],
                "proposed_at": stored.proposed_at,
                "approved_at": stored.approved_at,
            },
            "labels": {
                "read": "ok",
                "agent": {
                    "agent:ready": true,
                    "agent:blocked": true,
                    "agent:exhausted": true,
                },
                "risk": ["risk:documentation", "risk:logic"],
            },
        })
    );
    assert!(stored.approved_at.is_some());
    assert_eq!(fixture.gh_log(), EXPECTED_GH_LOG);
}

#[test]
fn a_pending_repo_missing_a_label_reports_pending_and_the_missing_label() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    let stored = fixture.stored(REPO);

    let output = fixture.status(
        r#"[{"name":"agent:ready"},{"name":"agent:blocked"},{"name":"sev:critical"}]"#,
        None,
    );

    assert_eq!(
        report(&output),
        json!({
            "repo": "owner/repo",
            "db": fixture.db_path().display().to_string(),
            "onboarded": true,
            "gate_config": {
                "state": "pending",
                "gate_commands": ["cargo test"],
                "manifest_warnings": [],
                "proposed_at": stored.proposed_at,
                "approved_at": null,
            },
            "labels": {
                "read": "ok",
                "agent": {
                    "agent:ready": true,
                    "agent:blocked": true,
                    "agent:exhausted": false,
                },
                "risk": [],
            },
        })
    );
}

#[test]
fn a_repo_with_no_gate_config_is_not_onboarded_and_still_exits_zero() {
    let fixture = Fixture::new();
    // Another repo's config, so the database exists and is not empty.
    fixture.propose("other/repo", &["cargo test"]);
    fixture.approve("other/repo");

    let output = fixture.status(ALL_LABELS, None);

    assert_eq!(output.status.code(), Some(0));
    let report = report(&output);
    assert_eq!(report["onboarded"], json!(false));
    assert_eq!(report["gate_config"], Value::Null);
    assert_eq!(report["labels"]["read"], json!("ok"));
}

#[test]
fn a_gh_failure_is_a_failed_label_read_not_missing_labels() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    fixture.approve(REPO);

    let output = fixture.status(
        ALL_LABELS,
        Some("HTTP 401: Bad credentials (https://api.github.com/graphql)"),
    );

    let report = report(&output);
    assert_eq!(report["gate_config"]["state"], json!("approved"));
    let labels = report["labels"].as_object().unwrap();
    let mut keys: Vec<&str> = labels.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["error", "read"],
        "a failed read carries no label answers: {labels:?}"
    );
    assert_eq!(labels["read"], json!("failed"));
    let error = labels["error"].as_str().unwrap();
    assert!(error.contains("gh label list on owner/repo"), "{error}");
    assert!(error.contains("HTTP 401: Bad credentials"), "{error}");
}

#[test]
fn a_missing_gh_is_a_failed_label_read_and_the_gate_config_is_still_reported() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    fixture.approve(REPO);
    // A PATH holding one empty directory: no `gh` anywhere, real or stub.
    let empty = fixture.root.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();

    let output = fixture.status_at(&fixture.db_path(), &empty, ALL_LABELS, None);

    let report = report(&output);
    assert_eq!(report["gate_config"]["state"], json!("approved"));
    assert_eq!(report["labels"]["read"], json!("failed"));
    assert!(report["labels"].get("agent").is_none());
    assert_eq!(fixture.gh_log(), "", "no gh ran");
}

#[test]
fn status_changes_nothing_in_the_database() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    fixture.approve(REPO);
    let db = fixture.db_path();
    // SQLite may create `-wal`/`-shm` side files next to a WAL database; they
    // are not asserted on here, only the database file itself.
    // `Database::open`, the read-write opener, re-chmods the file to 0600.
    // A mode it would not leave alone shows which opener ran.
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o640)).unwrap();
    let bytes_before = std::fs::read(&db).unwrap();
    let drawers_before = Database::open_read_only(&db)
        .unwrap()
        .count_drawers(None)
        .unwrap();

    let output = fixture.status(ALL_LABELS, None);

    assert_eq!(report(&output)["gate_config"]["state"], json!("approved"));
    assert!(
        std::fs::read(&db).unwrap() == bytes_before,
        "status changed the database file's bytes"
    );
    assert_eq!(mode(&db), 0o640, "status re-chmodded the database file");
    assert_eq!(
        Database::open_read_only(&db)
            .unwrap()
            .count_drawers(None)
            .unwrap(),
        drawers_before
    );
    assert_eq!(fixture.gh_log(), EXPECTED_GH_LOG, "one read-only gh call");
}

#[test]
fn status_never_creates_a_missing_database() {
    let fixture = Fixture::new();
    let absent_dir = fixture.root.path().join("absent");
    let absent_db = absent_dir.join("memory.sqlite3");

    let output = fixture.status_at(
        &absent_db,
        &isolated_path(&fixture.stubs()),
        ALL_LABELS,
        None,
    );

    assert_eq!(
        output.status.code(),
        Some(1),
        "an unreadable gate config is exit 1, not a report"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("db not found at"), "{stderr}");
    assert!(output.stdout.is_empty(), "no report on stdout");
    assert!(!absent_db.exists(), "status created the database file");
    assert!(
        !absent_dir.exists(),
        "status created the database directory"
    );
    assert_eq!(
        fixture.gh_log(),
        "",
        "gh is not asked once the status is unreadable"
    );
}

#[test]
fn the_text_report_is_the_default() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);

    let output = fixture
        .plain(
            REPO,
            &fixture.db_path(),
            fixture.root.path(),
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("owner/repo: gate config pending"),
        "{stdout}"
    );
    assert!(stdout.contains("  agent:ready — present"), "{stdout}");
    assert!(
        stdout.ends_with(&format!("Database: {}\n", fixture.db_path().display())),
        "{stdout}"
    );
    // The hinted command names the database this run read, not the default.
    let hint = format!(
        "ironmem autopilot approve owner/repo --db {}",
        fixture.db_path().display()
    );
    assert!(stdout.contains(&hint), "{stdout}");
}

/// Assert the run failed the way an unreadable gate config does: exit 1,
/// nothing on stdout, and `needles` on stderr. No `gh` ran.
fn assert_unreadable(fixture: &Fixture, output: &Output, needles: &[&str]) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "no report on stdout");
    let stderr = String::from_utf8_lossy(&output.stderr);
    for needle in needles {
        assert!(stderr.contains(needle), "{needle:?} not in: {stderr}");
    }
    assert_eq!(
        fixture.gh_log(),
        "",
        "gh is not asked once the status is unreadable"
    );
}

#[test]
fn a_malformed_repo_name_exits_one_instead_of_reporting_not_onboarded() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);

    let output = fixture
        .command(
            "not a repo",
            &fixture.db_path(),
            fixture.root.path(),
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .output()
        .unwrap();

    assert_unreadable(&fixture, &output, &["is not a repo name"]);
}

#[test]
fn a_config_stored_under_a_colliding_key_exits_one() {
    let fixture = Fixture::new();
    // `foo-bar/baz` and `foo/bar-baz` reduce to the same storage key.
    fixture.propose("foo-bar/baz", &["cargo test"]);

    let output = fixture
        .command(
            "foo/bar-baz",
            &fixture.db_path(),
            fixture.root.path(),
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .output()
        .unwrap();

    assert_unreadable(&fixture, &output, &["'foo-bar/baz'", "'foo/bar-baz'"]);
}

#[test]
fn a_file_that_is_not_a_database_exits_one_and_names_the_path() {
    let fixture = Fixture::new();
    let not_db = fixture.root.path().join("notes.txt");
    std::fs::write(
        &not_db,
        "this is not sqlite, it is a text file\n".repeat(100),
    )
    .unwrap();

    let output = fixture.status_at(&not_db, &isolated_path(&fixture.stubs()), ALL_LABELS, None);

    let shown = not_db.display().to_string();
    assert_unreadable(&fixture, &output, &["could not read the database", &shown]);
}

#[test]
fn status_never_migrates_an_old_database() {
    let fixture = Fixture::new();
    let db = fixture.db_path();
    {
        // Only the first migration: `schema_version` reports 1, far behind
        // the latest, so a migrating opener would visibly change it.
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(include_str!("../migrations/001_init.sql"))
            .unwrap();
    }
    let version_before = Database::open_read_only(&db)
        .unwrap()
        .schema_version()
        .unwrap();
    let bytes_before = std::fs::read(&db).unwrap();

    let output = fixture.status(ALL_LABELS, None);

    assert_eq!(report(&output)["onboarded"], json!(false));
    assert_eq!(
        Database::open_read_only(&db)
            .unwrap()
            .schema_version()
            .unwrap(),
        version_before
    );
    assert!(
        std::fs::read(&db).unwrap() == bytes_before,
        "status changed the old database's bytes"
    );
}

#[test]
fn path_is_the_directory_gh_runs_in() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);

    let output = fixture.status(ALL_LABELS, None);

    assert!(output.status.success());
    let cwd = std::fs::read_to_string(format!("{}.cwd", fixture.log_path().display())).unwrap();
    assert_eq!(
        std::fs::canonicalize(cwd.trim()).unwrap(),
        std::fs::canonicalize(fixture.root.path()).unwrap()
    );
}

#[test]
fn a_path_that_is_not_a_directory_is_a_failed_label_read_and_gh_does_not_run() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    fixture.approve(REPO);
    let missing = fixture.root.path().join("no-such-dir");

    let output = fixture
        .command(
            REPO,
            &fixture.db_path(),
            &missing,
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .output()
        .unwrap();

    let report = report(&output);
    assert_eq!(report["gate_config"]["state"], json!("approved"));
    assert_eq!(report["labels"]["read"], json!("failed"));
    assert!(report["labels"].get("agent").is_none());
    let error = report["labels"]["error"].as_str().unwrap();
    assert!(error.contains("--path"), "{error}");
    assert_eq!(fixture.gh_log(), "", "gh did not run");
}

#[test]
fn a_path_that_is_a_regular_file_is_a_failed_label_read_and_gh_does_not_run() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    let file = fixture.root.path().join("a-file.txt");
    std::fs::write(&file, "not a directory\n").unwrap();

    let output = fixture
        .command(
            REPO,
            &fixture.db_path(),
            &file,
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .output()
        .unwrap();

    let report = report(&output);
    assert_eq!(report["labels"]["read"], json!("failed"));
    let error = report["labels"]["error"].as_str().unwrap();
    assert!(error.contains("--path"), "{error}");
    assert_eq!(fixture.gh_log(), "", "gh did not run");
}

#[test]
fn a_wal_database_in_a_read_only_directory_exits_one_and_explains_the_side_files() {
    let fixture = Fixture::new();
    let dir = fixture.root.path().join("ro");
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("memory.sqlite3");
    {
        let db = Database::open(&db_path).unwrap();
        db.migrate().unwrap();
        gate_config::propose_gate_config(&db, REPO, vec!["cargo test".into()], Vec::new()).unwrap();
    }
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db_path.display()));
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    // As root the directory is still writable and the case cannot arise.
    let probe = dir.join("probe");
    if std::fs::write(&probe, "x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let output = fixture.status_at(&db_path, &isolated_path(&fixture.stubs()), ALL_LABELS, None);
    // Restore before asserting, so a failure still lets the tempdir drop.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_unreadable(
        &fixture,
        &output,
        &["could not read the database", "-wal/-shm"],
    );
}

#[test]
fn hints_name_the_absolute_database_when_db_was_given_relative() {
    let fixture = Fixture::new();
    // Another repo's config, so the database exists and REPO is not onboarded.
    fixture.propose("other/repo", &["cargo test"]);

    let output = fixture
        .plain(
            REPO,
            Path::new("memory.sqlite3"),
            fixture.root.path(),
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .current_dir(fixture.root.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let hint = stdout
        .lines()
        .find(|line| line.contains("autopilot onboard"))
        .unwrap_or_else(|| panic!("no onboard hint: {stdout}"));
    let value = hint
        .split("--db ")
        .nth(1)
        .and_then(|rest| rest.split('`').next())
        .unwrap_or_else(|| panic!("no --db in the hint: {hint}"))
        .trim_matches('\'');
    assert!(
        Path::new(value).is_absolute(),
        "the hinted --db is not absolute: {hint}"
    );
    assert_eq!(
        std::fs::canonicalize(value).unwrap(),
        std::fs::canonicalize(fixture.db_path()).unwrap()
    );
}

#[test]
fn a_dash_led_repo_name_exits_one() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);

    // After `--`, so clap passes the dash-led name through as the repo.
    let mut command = ironmem();
    command
        .args(["autopilot", "status", "--json", "--db"])
        .arg(fixture.db_path())
        .args(["--", "-R/x"]);
    fixture.env(
        &mut command,
        &isolated_path(&fixture.stubs()),
        ALL_LABELS,
        None,
    );
    let output = command.output().unwrap();

    assert_unreadable(&fixture, &output, &["is not a repo name"]);
}

#[test]
fn a_stored_config_that_does_not_parse_exits_one() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    {
        let conn = rusqlite::Connection::open(fixture.db_path()).unwrap();
        let changed = conn
            .execute(
                "UPDATE drawers SET content = ?1 \
                 WHERE wing = 'autopilot' AND room = 'backlog-lineage' \
                 AND source_file LIKE '%gate-config:%'",
                [r#"{"repo":"owner/repo","state":"approved","gate_commands":[],"proposed_at":"2026-01-01T00:00:00+00:00","approved_at":null}"#],
            )
            .unwrap();
        assert_eq!(
            changed, 1,
            "the UPDATE must hit exactly the gate config row"
        );
    }

    let output = fixture.status(ALL_LABELS, None);

    assert_unreadable(&fixture, &output, &["gate_commands must not be empty"]);
}

#[test]
fn a_name_that_only_resembles_an_onboarded_repo_exits_one_and_names_the_stored_one() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    fixture.approve(REPO);

    let output = fixture
        .command(
            "Owner/Repo",
            &fixture.db_path(),
            fixture.root.path(),
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .output()
        .unwrap();

    assert_unreadable(
        &fixture,
        &output,
        &["'Owner/Repo'", "'owner/repo'", "ask with the stored name"],
    );
}

#[test]
fn a_git_suffix_exits_one() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);

    let output = fixture
        .command(
            "owner/repo.git",
            &fixture.db_path(),
            fixture.root.path(),
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .output()
        .unwrap();

    assert_unreadable(&fixture, &output, &["drop the `.git` suffix"]);
}

#[test]
fn a_host_with_a_port_and_a_full_url_exit_one() {
    // Each ends in the onboarded `owner/repo`, so a status that stopped
    // refusing the shape would answer with that repo's near-miss error
    // instead; the message pins which rule refused it.
    for name in [
        "ghe.example.com:8443/owner/repo",
        "https://github.com/owner/repo",
        "git@github.com:owner/repo",
    ] {
        let fixture = Fixture::new();
        fixture.propose(REPO, &["cargo test"]);

        let output = fixture
            .command(
                name,
                &fixture.db_path(),
                fixture.root.path(),
                &isolated_path(&fixture.stubs()),
                ALL_LABELS,
                None,
            )
            .output()
            .unwrap();

        assert_unreadable(
            &fixture,
            &output,
            &[&format!("'{name}' is not a repo name")],
        );
    }
}

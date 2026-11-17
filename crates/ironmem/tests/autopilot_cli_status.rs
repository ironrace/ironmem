//! Public-CLI coverage for `ironmem autopilot status` (#420).
//!
//! `gh` is a local stub on an isolated PATH. It answers `label list` and
//! refuses everything else, so nothing here can reach GitHub and a `status`
//! that tried to create or edit a label would fail these tests.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use ironmem::autopilot::gate_config::{self, GateConfig};
use ironmem::db::schema::Database;
use serde_json::{json, Value};

mod autopilot_support;
use autopilot_support::{executable, ironmem, isolated_path, REPO};

/// Bash builtins only, so it runs on a PATH that holds nothing else.
const GH_STUB: &str = r#"#!/bin/bash
echo "gh $*" >> "$GH_LOG"
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

    /// `ironmem autopilot status owner/repo --json` against `db`, with the
    /// stub answering `labels`, or failing with `fail` on stderr.
    fn status_at(&self, db: &Path, path_var: &Path, labels: &str, fail: Option<&str>) -> Output {
        let mut command = ironmem();
        command
            .args(["autopilot", "status", REPO, "--json", "--db"])
            .arg(db)
            .arg("--path")
            .arg(self.root.path())
            .env("PATH", path_var)
            .env("GH_LOG", self.log_path())
            .env("GH_LABELS", labels);
        if let Some(message) = fail {
            command.env("GH_FAIL", message);
        } else {
            command.env_remove("GH_FAIL");
        }
        command.output().unwrap()
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

    let output = ironmem()
        .args(["autopilot", "status", REPO, "--db"])
        .arg(fixture.db_path())
        .arg("--path")
        .arg(fixture.root.path())
        .env("PATH", isolated_path(&fixture.stubs()))
        .env("GH_LOG", fixture.log_path())
        .env("GH_LABELS", ALL_LABELS)
        .env_remove("GH_FAIL")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("owner/repo: gate config pending"),
        "{stdout}"
    );
    assert!(stdout.contains("  agent:ready — present"), "{stdout}");
    // The hinted command names the database this run read, not the default.
    let hint = format!(
        "ironmem autopilot approve owner/repo --db {}",
        fixture.db_path().display()
    );
    assert!(stdout.contains(&hint), "{stdout}");
}

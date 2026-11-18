//! Public-CLI coverage for `ironmem autopilot status` (#420).
//!
//! `gh` is a local stub on an isolated PATH. It answers `label list` and
//! refuses everything else, and it logs every call, so nothing here can reach
//! GitHub and a `status` that tried to create or edit a label, or asked
//! twice, would fail the tests that compare the log. `HOME` is an empty
//! directory inside the fixture, so no run reads or writes the developer's
//! home, where the default database lives.

#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use ironmem::autopilot::dispatch_state::{self, DispatchState};
use ironmem::autopilot::gate_config::{self, GateConfig};
use ironmem::autopilot::implementer::Implementer;
use ironmem::autopilot::IssueRef;
use ironmem::db::schema::Database;
use serde_json::{json, Value};

mod autopilot_support;
use autopilot_support::{executable, ironmem, isolated_path, REPO};

/// Bash builtins only, so it needs nothing from PATH. The log holds one line
/// per argument, so two arguments cannot read as one that has a space in it.
const GH_STUB: &str = r#"#!/bin/bash
printf '%s\n' gh "$@" >> "$GH_LOG"
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

/// All three `agent:*` labels and two `risk:*` labels, plus two that are
/// neither: `bug`, and `sev:critical`, which a fuzzy `--search "risk:"`
/// returns.
const ALL_LABELS: &str = r#"[{"name":"bug"},{"name":"agent:blocked"},{"name":"agent:ready"},{"name":"agent:exhausted"},{"name":"risk:logic"},{"name":"risk:documentation"},{"name":"sev:critical"}]"#;

/// The one `gh` call a status run for [`REPO`] may make, one argument per
/// line.
const EXPECTED_GH_LOG: &str = "gh\nlabel\nlist\n--repo\nowner/repo\n--limit\n1000\n--json\nname\n";

struct Fixture {
    /// Removes the directory when the fixture is dropped.
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // As `std::path::absolute` spells it, which is how `status` names a
        // database: a `TMPDIR` holding `//` or `/./` then compares equal.
        let root = std::path::absolute(dir.path()).unwrap();
        let stubs = root.join("stubs");
        std::fs::create_dir_all(&stubs).unwrap();
        executable(&stubs.join("gh"), GH_STUB);
        std::fs::create_dir_all(root.join("home")).unwrap();
        Self { _dir: dir, root }
    }

    /// The fixture's directory.
    fn root(&self) -> &Path {
        &self.root
    }

    /// The `HOME` every run gets: a directory that starts empty.
    fn home(&self) -> PathBuf {
        self.root().join("home")
    }

    /// The names of everything in [`Fixture::home`].
    fn home_entries(&self) -> Vec<String> {
        std::fs::read_dir(self.home())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    fn db_path(&self) -> PathBuf {
        self.root().join("memory.sqlite3")
    }

    fn log_path(&self) -> PathBuf {
        self.root().join("gh.log")
    }

    fn stubs(&self) -> PathBuf {
        self.root().join("stubs")
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
    /// fixture's empty directory as `HOME`, the stub answering `labels` (or
    /// failing with `fail` on stderr), and five variables removed so that a
    /// developer's shell cannot change the result. `IRONMEM_DB_PATH`,
    /// `IRONMEM_MCP_MODE` and `IRONMEM_EMBED_MODE` redirect the database or
    /// fail config loading. `BASH_ENV` names a file the stub's bash would
    /// run first. `RUST_LOG` turns on log lines, and a run that reports must
    /// print nothing on stderr.
    fn env(&self, command: &mut Command, path_var: &Path, labels: &str, fail: Option<&str>) {
        command
            .env("PATH", path_var)
            .env("HOME", self.home())
            .env("GH_LOG", self.log_path())
            .env("GH_LABELS", labels)
            .env_remove("IRONMEM_DB_PATH")
            .env_remove("IRONMEM_MCP_MODE")
            .env_remove("IRONMEM_EMBED_MODE")
            .env_remove("BASH_ENV")
            .env_remove("RUST_LOG");
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
        self.command(REPO, db, self.root(), path_var, labels, fail)
            .output()
            .unwrap()
    }

    fn status(&self, labels: &str, fail: Option<&str>) -> Output {
        self.status_at(&self.db_path(), &isolated_path(&self.stubs()), labels, fail)
    }

    /// `ironmem autopilot status <args>` run in the fixture root with no
    /// `--db` and no `--path` of its own: the command line the router uses.
    fn bare(&self, args: &[&str]) -> Command {
        let mut command = ironmem();
        command
            .args(["autopilot", "status"])
            .args(args)
            .current_dir(self.root());
        self.env(
            &mut command,
            &isolated_path(&self.stubs()),
            ALL_LABELS,
            None,
        );
        command
    }

    /// The directory the stub `gh` last ran in.
    fn gh_cwd(&self) -> PathBuf {
        let cwd = std::fs::read_to_string(format!("{}.cwd", self.log_path().display())).unwrap();
        std::fs::canonicalize(cwd.trim()).unwrap()
    }

    fn gh_log(&self) -> String {
        std::fs::read_to_string(self.log_path()).unwrap_or_default()
    }
}

/// The report a successful run printed. A run that reports prints nothing
/// else: stderr is empty.
fn report(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "status exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "a successful run wrote to stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout is not one JSON document ({err}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// The text report a successful run printed. As for [`report`], a run that
/// reports prints nothing else: stderr is empty.
fn text_report(output: &Output) -> String {
    assert!(
        output.status.success(),
        "status exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "a successful run wrote to stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).expect("the text report is UTF-8")
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The value of `--db` in the hinted command on `line`, without the quotes
/// the report puts around a path a shell would split. Exact for a path that
/// holds no `'`, which no fixture path does.
fn hinted_db(line: &str) -> &str {
    line.split("--db ")
        .nth(1)
        .and_then(|rest| rest.split('`').next())
        .unwrap_or_else(|| panic!("no --db in the hint: {line}"))
        .trim_matches('\'')
}

/// The line of `text` that hints `command`.
fn hint_line<'a>(text: &'a str, command: &str) -> &'a str {
    text.lines()
        .find(|line| line.contains(command))
        .unwrap_or_else(|| panic!("no `{command}` hint in: {text}"))
}

/// The size of the database's `-wal` side file, 0 when there is none.
fn wal_len(db: &Path) -> u64 {
    std::fs::metadata(format!("{}-wal", db.display())).map_or(0, |wal| wal.len())
}

/// Every drawer's id, content and supersession mark, in id order, read over
/// a read-only connection.
fn drawer_dump(db: &Path) -> Vec<(String, String, Option<String>)> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut statement = conn
        .prepare("SELECT id, content, superseded_by FROM drawers ORDER BY id")
        .unwrap();
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

/// Replace the content of the one stored gate config with `content`, as a
/// hand edit would.
fn overwrite_the_gate_config(db: &Path, content: &str) {
    let conn = rusqlite::Connection::open(db).unwrap();
    let changed = conn
        .execute(
            "UPDATE drawers SET content = ?1 \
             WHERE wing = 'autopilot' AND room = 'backlog-lineage' \
             AND source_file LIKE '%gate-config:%'",
            [content],
        )
        .unwrap();
    assert_eq!(
        changed, 1,
        "the UPDATE must hit exactly the gate config row"
    );
}

/// A stored gate config naming `repo` that fails `GateConfig`'s guard: it
/// has no gate commands.
fn no_gate(repo: &str) -> String {
    json!({
        "repo": repo,
        "state": "approved",
        "gate_commands": [],
        "proposed_at": "2026-01-01T00:00:00+00:00",
        "approved_at": null,
    })
    .to_string()
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

    // The layout, which parsing the report as JSON does not see: indented by
    // two spaces, keys in the documented order, and one newline at the end.
    let printed = String::from_utf8(output.stdout.clone()).unwrap();
    assert!(
        printed.starts_with(&format!(
            "{{\n  \"repo\": \"owner/repo\",\n  \"db\": {},\n  \"onboarded\": true,\n  \
             \"gate_config\": {{\n    \"state\": \"approved\",\n",
            json!(fixture.db_path().display().to_string())
        )),
        "{printed}"
    );
    assert!(printed.ends_with("  }\n}\n"), "{printed}");
    assert!(!printed.ends_with("\n\n"), "{printed}");
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
    assert_eq!(
        fixture.gh_log(),
        EXPECTED_GH_LOG,
        "one label list, and no attempt to create the missing label"
    );
}

#[test]
fn a_repo_with_no_gate_config_is_not_onboarded_and_still_exits_zero() {
    let fixture = Fixture::new();
    // Another repo's config, so the database exists and is not empty.
    fixture.propose("other/repo", &["cargo test"]);
    fixture.approve("other/repo");
    let bytes_before = std::fs::read(fixture.db_path()).unwrap();

    let output = fixture.status(ALL_LABELS, None);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        report(&output),
        json!({
            "repo": "owner/repo",
            "db": fixture.db_path().display().to_string(),
            "onboarded": false,
            "gate_config": null,
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
    assert_eq!(
        fixture.gh_log(),
        EXPECTED_GH_LOG,
        "one label list, and no attempt to onboard the repo"
    );
    assert!(
        std::fs::read(fixture.db_path()).unwrap() == bytes_before,
        "status changed the database file's bytes"
    );
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
    // No "Not found:" in front, the exit code as a number, and where to look.
    assert_eq!(
        labels["error"],
        json!(
            "gh label list on owner/repo failed (exit 1): HTTP 401: Bad credentials \
             (https://api.github.com/graphql) — if the cause is authentication, check `gh auth \
             status`"
        )
    );
    assert_eq!(fixture.gh_log(), EXPECTED_GH_LOG, "one call, no retry");
}

#[test]
fn a_missing_gh_is_a_failed_label_read_and_the_gate_config_is_still_reported() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    fixture.approve(REPO);
    // A PATH holding one empty directory: no `gh` anywhere, real or stub.
    let empty = fixture.root().join("empty");
    std::fs::create_dir_all(&empty).unwrap();

    let output = fixture.status_at(&fixture.db_path(), &empty, ALL_LABELS, None);

    let report = report(&output);
    assert_eq!(report["gate_config"]["state"], json!("approved"));
    assert_eq!(report["labels"]["read"], json!("failed"));
    assert!(report["labels"].get("agent").is_none());
    let error = report["labels"]["error"].as_str().unwrap();
    assert!(error.starts_with("could not find `gh`"), "{error}");
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
    //
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
    let absent_dir = fixture.root().join("absent");
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
    assert!(
        stderr.contains(&format!(
            "Error: Not found: db not found at {}",
            absent_db.display()
        )),
        "{stderr}"
    );
    // The error says how to name a database that exists.
    assert!(stderr.contains("`--db <path>`"), "{stderr}");
    assert!(stderr.contains("`IRONMEM_DB_PATH`"), "{stderr}");
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
fn a_missing_database_is_created_by_the_onboard_its_error_names() {
    let fixture = Fixture::new();
    let db = fixture.db_path();

    let output = fixture.status(ALL_LABELS, None);

    assert_unreadable(
        &fixture,
        &output,
        &[
            &format!("Error: Not found: db not found at {}", db.display()),
            "If Autopilot has never been set up with this database, `ironmem autopilot onboard` \
             creates it",
        ],
    );
    assert!(!db.exists(), "status created the database file");

    // What the error says is true: `onboard` creates the database.
    let checkout = fixture.root().join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    std::fs::write(checkout.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    let mut onboard = ironmem();
    onboard
        .args(["autopilot", "onboard", REPO, "--path", ".", "--db"])
        .arg(&db)
        .current_dir(&checkout);
    fixture.env(
        &mut onboard,
        &isolated_path(&fixture.stubs()),
        ALL_LABELS,
        None,
    );
    let onboarded = onboard.output().unwrap();
    assert!(onboarded.status.success(), "{onboarded:?}");

    let report = report(&fixture.status(ALL_LABELS, None));
    assert_eq!(report["gate_config"]["state"], json!("pending"));
}

#[test]
fn the_text_report_is_the_default() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);

    let output = fixture
        .plain(
            REPO,
            &fixture.db_path(),
            fixture.root(),
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .output()
        .unwrap();

    let stdout = text_report(&output);
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
    // Compared as a path, so a temp directory a shell would quote still
    // matches.
    let hint = hint_line(&stdout, "ironmem autopilot approve owner/repo --db ");
    assert_eq!(hinted_db(hint), fixture.db_path().display().to_string());
    assert_eq!(fixture.gh_log(), EXPECTED_GH_LOG, "one read-only gh call");
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
            fixture.root(),
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
            fixture.root(),
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .output()
        .unwrap();

    assert_unreadable(&fixture, &output, &["'foo-bar/baz'", "'foo/bar-baz'"]);
}

#[test]
fn a_broken_config_of_the_colliding_name_exits_one_and_names_no_onboard_for_the_asked_name() {
    let fixture = Fixture::new();
    // Onboarded as `foo-bar/baz`; the row is then overwritten with one that
    // does not parse and still names that repo.
    fixture.propose("foo-bar/baz", &["cargo test"]);
    overwrite_the_gate_config(&fixture.db_path(), &no_gate("foo-bar/baz"));

    let output = fixture
        .command(
            "foo/bar-baz",
            &fixture.db_path(),
            fixture.root(),
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .output()
        .unwrap();

    assert_unreadable(
        &fixture,
        &output,
        &[
            "Error: JSON error: the gate config stored under the key 'gate-config:foo-bar-baz' in ",
            &fixture.db_path().display().to_string(),
            "gate_commands must not be empty",
            "its `repo` is \"foo-bar/baz\", not 'foo/bar-baz'",
        ],
    );
    // That command would overwrite the row of `foo-bar/baz`.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("autopilot onboard foo/bar-baz"),
        "{stderr}"
    );
}

#[test]
fn a_file_that_is_not_a_database_exits_one_and_names_the_path() {
    let fixture = Fixture::new();
    let not_db = fixture.root().join("notes.txt");
    std::fs::write(
        &not_db,
        "this is not sqlite, it is a text file\n".repeat(100),
    )
    .unwrap();

    let output = fixture.status_at(&not_db, &isolated_path(&fixture.stubs()), ALL_LABELS, None);

    let shown = not_db.display().to_string();
    assert_unreadable(
        &fixture,
        &output,
        &[
            "Error: Database error: could not read the database at",
            &shown,
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("IO error"), "{stderr}");
}

#[test]
fn an_empty_db_path_exits_one_instead_of_reporting_a_relative_path() {
    let fixture = Fixture::new();

    let output = fixture.status_at(
        Path::new(""),
        &isolated_path(&fixture.stubs()),
        ALL_LABELS,
        None,
    );

    assert_unreadable(
        &fixture,
        &output,
        &["Error: Validation error: the database path is empty — check `--db` and `IRONMEM_DB_PATH`"],
    );
}

#[test]
fn an_empty_ironmem_db_path_exits_one_and_the_error_names_the_variable() {
    let fixture = Fixture::new();

    let output = fixture
        .bare(&[REPO, "--json"])
        .env("IRONMEM_DB_PATH", "")
        .output()
        .unwrap();

    assert_unreadable(
        &fixture,
        &output,
        &["Error: Validation error: the database path is empty — check `--db` and `IRONMEM_DB_PATH`"],
    );
    assert_eq!(fixture.home_entries(), Vec::<String>::new());
}

#[test]
fn a_relative_db_from_a_removed_working_directory_exits_one_and_says_what_is_missing() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    let gone = fixture.root().join("gone");
    std::fs::create_dir_all(&gone).unwrap();

    // The shell enters the directory, removes it, and becomes `ironmem`:
    // the only way to start a process whose working directory is gone.
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", r#"cd "$1" && rmdir "$1" && shift && exec "$@""#, "sh"])
        .arg(&gone)
        .arg(autopilot_support::bin())
        .args([
            "autopilot",
            "status",
            REPO,
            "--json",
            "--db",
            "memory.sqlite3",
        ])
        .arg("--path")
        .arg(fixture.root());
    autopilot_support::scrub_git_env(&mut command);
    fixture.env(
        &mut command,
        &isolated_path(&fixture.stubs()),
        ALL_LABELS,
        None,
    );
    let output = command.output().unwrap();

    assert!(!gone.exists(), "the shell did not remove the directory");
    assert_unreadable(
        &fixture,
        &output,
        &[
            "Error: IO error: could not make the database path 'memory.sqlite3' absolute: ",
            " — a relative path is read against the working directory, which could not be read; \
             pass an absolute `--db`",
        ],
    );
}

#[test]
fn a_malformed_repo_name_is_reported_before_a_missing_database() {
    let fixture = Fixture::new();
    let absent_db = fixture.root().join("absent.sqlite3");

    let output = fixture
        .command(
            "not a repo",
            &absent_db,
            fixture.root(),
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .output()
        .unwrap();

    assert_unreadable(&fixture, &output, &["is not a repo name"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("db not found"), "{stderr}");
    assert!(!absent_db.exists(), "status created the database file");
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

    report(&output);
    assert_eq!(
        fixture.gh_cwd(),
        std::fs::canonicalize(fixture.root()).unwrap()
    );
}

#[test]
fn a_path_that_is_not_a_directory_is_a_failed_label_read_and_gh_does_not_run() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    fixture.approve(REPO);
    let missing = fixture.root().join("no-such-dir");

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
    let file = fixture.root().join("a-file.txt");
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
fn a_path_gh_cannot_be_started_in_is_a_failed_label_read_that_names_the_path() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    // A directory this user can see and cannot enter: the check that it is
    // a directory passes, and starting `gh` in it fails.
    let locked = fixture.root().join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // As root the directory can be entered and the case cannot arise.
    if std::fs::read_dir(&locked).is_ok() {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: running as root, a directory mode does not stop gh starting in it");
        return;
    }

    let output = fixture
        .command(
            REPO,
            &fixture.db_path(),
            &locked,
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .output()
        .unwrap();
    // Restore before asserting, so a failure still lets the tempdir drop.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    let report = report(&output);
    assert_eq!(report["gate_config"]["state"], json!("pending"));
    assert_eq!(report["labels"]["read"], json!("failed"));
    let error = report["labels"]["error"].as_str().unwrap();
    assert!(error.starts_with("failed to start "), "{error}");
    // The failure is about the directory, so the error has to name it, and
    // the flag it came from: `gh` itself is fine.
    assert!(
        error.ends_with(&format!(
            " — gh is started in the directory `--path` names, '{}', so check that this user \
             can enter it",
            locked.display()
        )),
        "{error}"
    );
    assert_eq!(fixture.gh_log(), "", "gh did not run");
}

#[test]
fn a_wal_database_in_a_read_only_directory_exits_one_and_explains_the_side_files() {
    let fixture = Fixture::new();
    let dir = fixture.root().join("ro");
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
        eprintln!("skipped: running as root, a directory mode does not stop the write");
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
            fixture.root(),
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .current_dir(fixture.root())
        .output()
        .unwrap();

    let stdout = text_report(&output);
    let hint = hint_line(&stdout, "autopilot onboard");
    let value = hinted_db(hint);
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
fn a_stored_config_that_does_not_parse_exits_one_and_the_remedy_it_names_works() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    overwrite_the_gate_config(&fixture.db_path(), &no_gate(REPO));

    let output = fixture.status(ALL_LABELS, None);

    let db = fixture.db_path().display().to_string();
    assert_unreadable(
        &fixture,
        &output,
        &[
            &format!("Error: JSON error: the gate config stored for 'owner/repo' in {db} does not parse: gate_commands must not be empty"),
            "To replace it, run `ironmem autopilot onboard owner/repo --path . --db ",
            "` from the repo's checkout, then `ironmem autopilot approve owner/repo --db ",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(hinted_db(hint_line(&stderr, "autopilot onboard")), db);

    // The remedy is real: `onboard`, run as the error says, replaces the row.
    let checkout = fixture.root().join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    std::fs::write(checkout.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    let mut onboard = ironmem();
    onboard
        .args(["autopilot", "onboard", REPO, "--path", ".", "--db"])
        .arg(fixture.db_path())
        .current_dir(&checkout);
    fixture.env(
        &mut onboard,
        &isolated_path(&fixture.stubs()),
        ALL_LABELS,
        None,
    );
    let onboarded = onboard.output().unwrap();
    assert!(onboarded.status.success(), "{onboarded:?}");

    let pending = report(&fixture.status(ALL_LABELS, None));
    assert_eq!(pending["gate_config"]["state"], json!("pending"));
    assert_eq!(
        pending["gate_config"]["gate_commands"],
        json!(["cargo test"])
    );

    // And so is the second command the error gives.
    let mut approve = ironmem();
    approve
        .args(["autopilot", "approve", REPO, "--db"])
        .arg(fixture.db_path());
    fixture.env(
        &mut approve,
        &isolated_path(&fixture.stubs()),
        ALL_LABELS,
        None,
    );
    let approved = approve.output().unwrap();
    assert!(approved.status.success(), "{approved:?}");

    let report = report(&fixture.status(ALL_LABELS, None));
    assert_eq!(report["gate_config"]["state"], json!("approved"));
    assert_eq!(
        report["gate_config"]["gate_commands"],
        pending["gate_config"]["gate_commands"]
    );
}

#[test]
fn a_broken_config_under_another_spelling_exits_one_instead_of_reading_not_onboarded() {
    let fixture = Fixture::new();
    // Onboarded as `Owner/Repo`; the row is then overwritten with one that
    // does not parse.
    fixture.propose("Owner/Repo", &["cargo test"]);
    overwrite_the_gate_config(&fixture.db_path(), &no_gate("Owner/Repo"));

    let output = fixture.status(ALL_LABELS, None);

    assert_unreadable(
        &fixture,
        &output,
        &[
            "Error: JSON error: 'owner/repo' has no gate config under that exact name",
            &fixture.db_path().display().to_string(),
            "the config stored under the key 'gate-config:Owner-Repo'",
            "\"Owner/Repo\"",
            "does not parse: gate_commands must not be empty",
        ],
    );
}

#[test]
fn a_broken_config_whose_name_cannot_be_read_is_matched_by_its_storage_key() {
    let fixture = Fixture::new();
    fixture.propose("github.com/owner/repo", &["cargo test"]);
    overwrite_the_gate_config(&fixture.db_path(), "this is not json");

    let output = fixture.status(ALL_LABELS, None);

    assert_unreadable(
        &fixture,
        &output,
        &[
            "'owner/repo' has no gate config under that exact name",
            "the config stored under the key 'gate-config:github.com-owner-repo' does not parse: \
             expected",
        ],
    );
}

#[test]
fn a_broken_config_of_another_repo_does_not_stop_this_repos_report() {
    let fixture = Fixture::new();
    fixture.propose("other/thing", &["cargo test"]);
    overwrite_the_gate_config(&fixture.db_path(), "this is not json");

    let report = report(&fixture.status(ALL_LABELS, None));

    assert_eq!(report["onboarded"], json!(false));
    assert_eq!(report["gate_config"], Value::Null);
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
            fixture.root(),
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
            fixture.root(),
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
    // Each ends in the onboarded `owner/repo`. A status that stopped refusing
    // the shape would answer the first two with that repo's near-miss error
    // and report the third as not onboarded (its owner part would be
    // `git@github.com:owner`); the message pins which rule refused it.
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
                fixture.root(),
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

// ── the command line the router uses: no --db, no --path ────────────────

#[test]
fn status_without_db_reads_ironmem_db_path() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    fixture.approve(REPO);

    let output = fixture
        .bare(&[REPO, "--json"])
        .env("IRONMEM_DB_PATH", fixture.db_path())
        .output()
        .unwrap();

    let report = report(&output);
    assert_eq!(report["db"], json!(fixture.db_path().display().to_string()));
    assert_eq!(report["gate_config"]["state"], json!("approved"));
    assert_eq!(report["labels"]["read"], json!("ok"));
    // `--path` defaults to the directory the command ran in.
    assert_eq!(
        fixture.gh_cwd(),
        std::fs::canonicalize(fixture.root()).unwrap()
    );
    assert_eq!(
        fixture.home_entries(),
        Vec::<String>::new(),
        "status created something in the home directory"
    );
}

#[test]
fn db_flag_wins_over_ironmem_db_path() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    let absent = fixture.root().join("absent.sqlite3");

    let output = fixture
        .bare(&[REPO, "--json", "--db"])
        .arg(fixture.db_path())
        .env("IRONMEM_DB_PATH", &absent)
        .output()
        .unwrap();

    let report = report(&output);
    assert_eq!(report["db"], json!(fixture.db_path().display().to_string()));
    assert_eq!(report["gate_config"]["state"], json!("pending"));
    assert!(!absent.exists());
}

#[test]
fn the_default_database_is_never_created() {
    let fixture = Fixture::new();

    let output = fixture.bare(&[REPO, "--json"]).output().unwrap();

    let default_db = fixture.home().join(".ironrace-memory/memory.sqlite3");
    assert_unreadable(
        &fixture,
        &output,
        &[&format!("db not found at {}", default_db.display())],
    );
    assert_eq!(
        fixture.home_entries(),
        Vec::<String>::new(),
        "status created something in the home directory"
    );
}

#[test]
fn no_db_is_hinted_when_the_database_came_from_the_environment() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);

    let output = fixture
        .bare(&[REPO])
        .env("IRONMEM_DB_PATH", fixture.db_path())
        .output()
        .unwrap();

    let stdout = text_report(&output);
    assert!(
        stdout.contains("run `ironmem autopilot approve owner/repo`\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("--db"), "{stdout}");
    assert!(
        stdout.ends_with(&format!("Database: {}\n", fixture.db_path().display())),
        "{stdout}"
    );
}

// ── exit status ─────────────────────────────────────────────────────────

#[test]
fn a_usage_error_exits_two_and_touches_nothing() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    let bytes_before = std::fs::read(fixture.db_path()).unwrap();

    // No repo, an unknown flag, and a dash-led name that is not after `--`.
    let cases: [&[&str]; 3] = [&[], &[REPO, "--bogus"], &["-R/x"]];
    for extra in cases {
        let output = fixture
            .bare(extra)
            .arg("--json")
            .arg("--db")
            .arg(fixture.db_path())
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(2), "{extra:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{extra:?}: no report on stdout");
        assert_eq!(fixture.gh_log(), "", "{extra:?}: gh is not asked");
        assert!(
            std::fs::read(fixture.db_path()).unwrap() == bytes_before,
            "{extra:?}: the database file's bytes changed"
        );
    }
}

#[test]
fn a_non_utf8_argument_is_a_usage_error() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    let bytes_before = std::fs::read(fixture.db_path()).unwrap();

    // Neither run creates a file, so a filesystem that refuses such a name
    // does not matter.
    let not_utf8_db = fixture
        .bare(&[REPO, "--json", "--db"])
        .arg(OsStr::from_bytes(b"/no-such-dir-\xff/memory.sqlite3"))
        .output()
        .unwrap();
    let not_utf8_repo = fixture
        .bare(&["--json", "--db"])
        .arg(fixture.db_path())
        .arg(OsStr::from_bytes(b"owner/re\xffpo"))
        .output()
        .unwrap();

    for (what, output) in [("--db", not_utf8_db), ("the repo", not_utf8_repo)] {
        assert_eq!(output.status.code(), Some(2), "{what}: {output:?}");
        assert!(output.stdout.is_empty(), "{what}: no report on stdout");
        assert_eq!(fixture.gh_log(), "", "{what}: gh is not asked");
    }
    assert!(
        std::fs::read(fixture.db_path()).unwrap() == bytes_before,
        "the database file's bytes changed"
    );
}

#[test]
fn a_default_database_under_a_non_utf8_home_exits_one() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let fixture = Fixture::new();

    // No `--db` and no `IRONMEM_DB_PATH`: the database is the default one
    // under `HOME`, which does not exist and is never created.
    let output = fixture
        .bare(&[REPO, "--json"])
        .env("HOME", OsStr::from_bytes(b"/no-such-home-\xff"))
        .output()
        .unwrap();

    assert_unreadable(
        &fixture,
        &output,
        &[
            "Error: Validation error: the database path '/no-such-home-",
            "/.ironrace-memory/memory.sqlite3' is not valid UTF-8, so the report could not name \
             it exactly",
        ],
    );
}

#[test]
fn an_invalid_ironmem_env_setting_exits_one() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    fixture.approve(REPO);

    for variable in ["IRONMEM_MCP_MODE", "IRONMEM_EMBED_MODE"] {
        let output = fixture
            .command(
                REPO,
                &fixture.db_path(),
                fixture.root(),
                &isolated_path(&fixture.stubs()),
                ALL_LABELS,
                None,
            )
            .env(variable, "bogus")
            .output()
            .unwrap();

        assert_unreadable(
            &fixture,
            &output,
            &[&format!("Error: Config error: {variable} must be one of")],
        );
    }
}

#[test]
fn a_report_that_cannot_be_written_exits_one_without_a_panic() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    // A pipe whose reading end is closed: every write to it fails.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);

    for json in [true, false] {
        let mut command = fixture.plain(
            REPO,
            &fixture.db_path(),
            fixture.root(),
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        );
        if json {
            command.arg("--json");
        }
        let output = command
            .stdout(Stdio::from(writer.try_clone().unwrap()))
            .output()
            .unwrap();

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "json={json}: {stderr}");
        assert!(
            stderr.contains("Error: IO error: could not write the report to stdout: "),
            "json={json}: {stderr}"
        );
        assert!(!stderr.contains("panicked"), "json={json}: {stderr}");
    }
}

// ── read-only when there is something to change ─────────────────────────

#[test]
fn status_changes_nothing_when_there_is_something_to_change() {
    let fixture = Fixture::new();
    // A pending config an approval would change, another repo's approved
    // one, and a paused dispatch that a finishing run would clear.
    fixture.propose(REPO, &["cargo test"]);
    fixture.propose("other/repo", &["cargo test"]);
    fixture.approve("other/repo");
    dispatch_state::upsert_dispatch_state(
        &fixture.writable_db(),
        &DispatchState {
            implementer: Implementer::Claude,
            native_session_id: None,
            invocation_artifacts: None,
            issue: IssueRef::new(REPO, 7),
            worktree_path: fixture.root().join("wt").display().to_string(),
            ic_session_name: "autopilot-ic-owner-repo-7".into(),
            dispatch_class: "documentation".into(),
            attempt_n: 1,
            state: "paused-daily-budget".into(),
            started_at: "2026-09-03T00:00:00Z".into(),
            session_uuid: "11111111-2222-3333-4444-555555555555".into(),
            turn_n: 1,
            session_claimed: true,
            session_model: None,
            resume_refusals: 0,
            signing_held: false,
        },
    )
    .unwrap();
    let db = fixture.db_path();
    assert_eq!(
        wal_len(&db),
        0,
        "the fixture's writes must be in the database file before it is compared"
    );
    let bytes_before = std::fs::read(&db).unwrap();
    let drawers_before = drawer_dump(&db);
    assert_eq!(drawers_before.len(), 3, "{drawers_before:?}");

    let pending = report(&fixture.status(ALL_LABELS, None));
    let absent = report(
        &fixture
            .command(
                "never/onboarded",
                &db,
                fixture.root(),
                &isolated_path(&fixture.stubs()),
                "[]",
                None,
            )
            .output()
            .unwrap(),
    );

    assert_eq!(pending["gate_config"]["state"], json!("pending"));
    assert_eq!(absent["onboarded"], json!(false));
    assert!(
        std::fs::read(&db).unwrap() == bytes_before,
        "status changed the database file's bytes"
    );
    assert_eq!(wal_len(&db), 0, "status left a write in the -wal file");
    assert_eq!(drawer_dump(&db), drawers_before);
}

/// Every file under `dir` by its path relative to `dir`, with its bytes; a
/// symlink with its target; a directory with nothing.
fn tree_of(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, into: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let kind = std::fs::symlink_metadata(&path).unwrap().file_type();
            if kind.is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                into.insert(relative, target.into_os_string().into_encoded_bytes());
            } else if kind.is_dir() {
                into.insert(relative, Vec::new());
                walk(root, &path, into);
            } else {
                into.insert(relative, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut tree = BTreeMap::new();
    walk(dir, dir, &mut tree);
    tree
}

#[test]
fn status_changes_nothing_in_the_checkout_gh_runs_in() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    // A real checkout. Autopilot keeps each dispatch's invocation record in
    // the checkout's git directory, under `ironmem/autopilot/<issue>/`, and
    // later runs settle and prune those. A file is put where such a record
    // lives, so a status that rewrote or removed one would show.
    let checkout = fixture.root().join("checkout");
    autopilot_support::make_repo(&checkout);
    let record = checkout.join(".git/ironmem/autopilot/owner-repo-7/an-invocation");
    std::fs::create_dir_all(&record).unwrap();
    std::fs::write(record.join("exit.json"), r#"{"awaiting_quiet":{}}"#).unwrap();
    let before = tree_of(&checkout);
    assert!(
        before.contains_key(Path::new(
            ".git/ironmem/autopilot/owner-repo-7/an-invocation/exit.json"
        )),
        "{:?}",
        before.keys().collect::<Vec<_>>()
    );

    let run = |json: bool| {
        let mut command = fixture.plain(
            REPO,
            &fixture.db_path(),
            &checkout,
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        );
        if json {
            command.arg("--json");
        }
        command.output().unwrap()
    };
    report(&run(true));
    text_report(&run(false));

    assert_eq!(
        fixture.gh_cwd(),
        std::fs::canonicalize(&checkout).unwrap(),
        "gh did not run in the checkout"
    );
    let after = tree_of(&checkout);
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        before.keys().collect::<Vec<_>>(),
        "status added or removed a file in the checkout"
    );
    for (path, bytes) in &before {
        assert!(
            after[path] == *bytes,
            "status changed {} in the checkout",
            path.display()
        );
    }
}

// ── label and --db edge cases ───────────────────────────────────────────

#[test]
fn gh_output_that_is_empty_is_a_failed_label_read() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);

    let report = report(&fixture.status("", None));

    assert_eq!(report["gate_config"]["state"], json!("pending"));
    assert_eq!(report["labels"]["read"], json!("failed"));
    assert!(report["labels"].get("agent").is_none());
    let error = report["labels"]["error"].as_str().unwrap();
    assert_eq!(
        error,
        "gh label list on owner/repo exited 0, but its output could not be read as a label list \
         — it printed nothing on stdout"
    );
    assert_eq!(fixture.gh_log(), EXPECTED_GH_LOG);
}

#[test]
fn a_padded_label_name_is_the_label_it_names() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);

    let report = report(&fixture.status(
        r#"[{"name":" agent:ready "},{"name":"\tAgent:Blocked"}]"#,
        None,
    ));

    assert_eq!(
        report["labels"]["agent"],
        json!({
            "agent:ready": true,
            "agent:blocked": true,
            "agent:exhausted": false,
        })
    );
}

#[test]
fn a_host_qualified_repo_is_passed_to_gh_as_it_was_given() {
    let fixture = Fixture::new();
    fixture.propose("GitHub.com/Owner/Repo", &["cargo test"]);

    let output = fixture
        .command(
            "GitHub.com/Owner/Repo",
            &fixture.db_path(),
            fixture.root(),
            &isolated_path(&fixture.stubs()),
            ALL_LABELS,
            None,
        )
        .output()
        .unwrap();

    let report = report(&output);
    assert_eq!(report["repo"], json!("GitHub.com/Owner/Repo"));
    assert_eq!(report["gate_config"]["state"], json!("pending"));
    assert_eq!(
        fixture.gh_log(),
        "gh\nlabel\nlist\n--repo\nGitHub.com/Owner/Repo\n--limit\n1000\n--json\nname\n"
    );
}

#[test]
fn a_db_path_that_is_a_directory_exits_one_and_is_not_called_missing() {
    let fixture = Fixture::new();
    let dir = fixture.root().join("a-directory");
    std::fs::create_dir_all(&dir).unwrap();

    let output = fixture.status_at(&dir, &isolated_path(&fixture.stubs()), ALL_LABELS, None);

    // Said before SQLite is asked: it opens a directory without complaint
    // and fails on the first read with a message about the disk.
    assert_unreadable(
        &fixture,
        &output,
        &[&format!(
            "Error: Validation error: {} is a directory, not a database file — name the \
             database file with `--db <path>` or `IRONMEM_DB_PATH`",
            dir.display()
        )],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("not found"), "{stderr}");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

// ── the router's prose and the documented example against the real report ──

/// The three files that state the `/evaluate-issue` router.
const ROUTER_SURFACES: [&str; 3] = [
    "docs/EVALUATE_ISSUE.md",
    ".claude-plugin/commands/evaluate-issue.md",
    ".codex-plugin/prompts/evaluate-issue.md",
];

/// Where in the report a router sentence reads the keys it names.
#[derive(Clone, Copy)]
enum Reads {
    /// Every key is a top-level key of the report.
    TopLevel,
    /// Every key is a key of the object under this top-level key.
    Inside(&'static str),
    /// The first key is a top-level key, and the rest are keys of the object
    /// under it.
    TopLevelThenInside,
}

/// The report a router sentence is about.
#[derive(Clone, Copy)]
enum About {
    Approved,
    Pending,
    NotOnboarded,
}

/// A router sentence that names parts of the report.
struct RouterSentence {
    /// Words that find its line. The line is the whole sentence.
    marker: &'static str,
    about: About,
    reads: Reads,
    /// Where the report holds the value the sentence quotes (`"approved"`,
    /// `null`), as a JSON pointer. `None` for a sentence that quotes none.
    quotes: Option<&'static str>,
}

/// The router sentences that say which parts of the report it reads: the
/// approval condition, the parts it may use, the fix line for a pending
/// config, and what a `null` gate config means. Each sits on one line in
/// every router file; the collab-template lint pins the first three.
const ROUTER_REPORT_SENTENCES: [RouterSentence; 4] = [
    RouterSentence {
        marker: "Approved only when it exits 0",
        about: About::Approved,
        reads: Reads::TopLevelThenInside,
        quotes: Some("/gate_config/state"),
    },
    RouterSentence {
        marker: "Use only its exit status",
        about: About::Approved,
        reads: Reads::TopLevel,
        quotes: None,
    },
    RouterSentence {
        marker: "gate config `\"pending\"` → review its",
        about: About::Pending,
        reads: Reads::Inside("gate_config"),
        quotes: Some("/gate_config/state"),
    },
    RouterSentence {
        marker: "is a repo not onboarded",
        about: About::NotOnboarded,
        reads: Reads::TopLevel,
        quotes: Some("/gate_config"),
    },
];

/// Backticked spans in those sentences that are not a part of the report: a
/// `gh` command and two `ironmem autopilot` subcommands.
const NOT_REPORT_PARTS: [&str; 3] = ["gh label list", "onboard", "approve"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The text inside each pair of backticks on `line`.
fn backticked(line: &str) -> Vec<&str> {
    let pieces: Vec<&str> = line.split('`').collect();
    assert!(pieces.len() % 2 == 1, "unpaired backtick in: {line}");
    pieces.into_iter().skip(1).step_by(2).collect()
}

/// What is wrong with `sentence` as a description of `report`: a key it
/// names that the report does not hold where the sentence reads it (`reads`),
/// a value it quotes that the report does not hold at `quotes`, or nothing
/// named at all. A span is a value when it is `null` or a string in quotes,
/// and a key otherwise.
fn sentence_failures(
    sentence: &str,
    reads: Reads,
    quotes: Option<&str>,
    report: &Value,
) -> Vec<String> {
    let mut failures = Vec::new();
    let mut named = 0;
    let mut first_key: Option<&str> = None;
    for span in backticked(sentence) {
        if NOT_REPORT_PARTS.contains(&span) {
            continue;
        }
        named += 1;
        let quoted = span.strip_prefix('"').and_then(|s| s.strip_suffix('"'));
        let value = match (span, quoted) {
            ("null", _) => Some(Value::Null),
            (_, Some(text)) => Some(json!(text)),
            _ => None,
        };
        if let Some(value) = value {
            match quotes.map(|pointer| (pointer, report.pointer(pointer))) {
                Some((_, Some(reported))) if *reported == value => {}
                Some((pointer, reported)) => failures.push(format!(
                    "`{span}` is quoted, and the report holds {reported:?} at {pointer}"
                )),
                None => failures.push(format!(
                    "`{span}` is quoted, and this sentence is not expected to quote a value"
                )),
            }
            continue;
        }
        let (holder, place) = match (reads, first_key) {
            (Reads::TopLevel, _) | (Reads::TopLevelThenInside, None) => {
                (Some(report), "the top level of the report".to_string())
            }
            (Reads::Inside(key), _) | (Reads::TopLevelThenInside, Some(key)) => {
                (report.get(key), format!("the report's `{key}`"))
            }
        };
        first_key.get_or_insert(span);
        if holder.and_then(|holder| holder.get(span)).is_none() {
            failures.push(format!("`{span}` is not a key of {place}"));
        }
    }
    if named == 0 {
        failures.push("names no part of the report".to_string());
    }
    failures
}

/// The JSON type of `value`.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The JSON type of every path in `value`: `gate_config.state` → `string`.
/// An array is one entry and is not descended into, so two reports with
/// different numbers of gate commands have the same shape.
fn shape_of(value: &Value, path: &str, into: &mut BTreeMap<String, &'static str>) {
    into.insert(path.to_string(), kind_of(value));
    if let Value::Object(map) = value {
        for (key, inner) in map {
            shape_of(inner, &format!("{path}.{key}"), into);
        }
    }
}

/// The JSON types of the elements of every array in `value` that has any, by
/// the array's path as [`shape_of`] writes it.
fn element_kinds(value: &Value, path: &str, into: &mut BTreeMap<String, BTreeSet<&'static str>>) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                element_kinds(inner, &format!("{path}.{key}"), into);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            into.insert(path.to_string(), items.iter().map(kind_of).collect());
        }
        _ => {}
    }
}

/// The router is prose, and the report's keys are serde's: nothing but this
/// test ties the names in one to the names in the other. A report key that
/// was renamed would leave every pinned sentence in place and a router that
/// never finds an approved gate config. Each sentence is checked against the
/// report it is about and the level it reads: `state` has to be a key of
/// `gate_config`, not merely a key somewhere.
#[test]
fn the_router_reads_keys_the_report_has() {
    let fixture = Fixture::new();
    // Another repo's config, so the database exists before REPO is onboarded.
    fixture.propose("other/repo", &["cargo test"]);
    let not_onboarded = report(&fixture.status(ALL_LABELS, None));
    fixture.propose(REPO, &["cargo test"]);
    let pending = report(&fixture.status(ALL_LABELS, None));
    fixture.approve(REPO);
    let approved = report(&fixture.status(ALL_LABELS, None));

    // The approval condition, read from a real report with the keys the
    // router's sentence names.
    let gate_config = approved["gate_config"]
        .as_object()
        .expect("an approved repo's `gate_config` is an object");
    assert_eq!(gate_config["state"], json!("approved"));
    assert_eq!(gate_config["gate_commands"], json!(["cargo test"]));
    assert!(approved["db"].is_string());
    assert_eq!(pending["gate_config"]["state"], json!("pending"));
    assert_eq!(not_onboarded["gate_config"], Value::Null);

    let mut failures = Vec::new();
    for rel in ROUTER_SURFACES {
        let text = std::fs::read_to_string(repo_root().join(rel))
            .unwrap_or_else(|e| panic!("{rel}: cannot read router surface: {e}"));
        for sentence in &ROUTER_REPORT_SENTENCES {
            let marker = sentence.marker;
            let lines: Vec<&str> = text.lines().filter(|line| line.contains(marker)).collect();
            let [line] = lines.as_slice() else {
                failures.push(format!(
                    "{rel}: {marker:?} is on {} lines; expected exactly one",
                    lines.len()
                ));
                continue;
            };
            let report = match sentence.about {
                About::Approved => &approved,
                About::Pending => &pending,
                About::NotOnboarded => &not_onboarded,
            };
            for failure in sentence_failures(line, sentence.reads, sentence.quotes, report) {
                failures.push(format!("{rel}: the sentence holding {marker:?}: {failure}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "the router names parts of the status report that the report does not have:\n{}\n\n\
         approved: {approved}\npending: {pending}\nnot onboarded: {not_onboarded}",
        failures.join("\n")
    );
}

/// [`sentence_failures`] on sentences this file makes up, against a real
/// report: the router files are prose this test cannot change, so what the
/// check catches is shown here.
#[test]
fn a_router_sentence_is_checked_against_the_level_and_the_value_it_reads() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    fixture.approve(REPO);
    let approved = report(&fixture.status(ALL_LABELS, None));
    let nested = (Reads::TopLevelThenInside, Some("/gate_config/state"));
    let failures = |sentence: &str, (reads, quotes): (Reads, Option<&str>)| {
        sentence_failures(sentence, reads, quotes, &approved)
    };

    assert_eq!(
        failures(
            "its JSON `gate_config` is an object whose `state` is `\"approved\"`; keep its \
             `gate_commands`",
            nested
        ),
        Vec::<String>::new()
    );
    // `read` is a key of the report, under `labels`, not under `gate_config`.
    assert_eq!(
        failures(
            "`gate_config` is an object whose `read` is `\"approved\"`",
            nested
        ),
        ["`read` is not a key of the report's `gate_config`"]
    );
    // `state` is a key of `gate_config`, not of the report.
    assert_eq!(
        failures("use `state` and `db`", (Reads::TopLevel, None)),
        ["`state` is not a key of the top level of the report"]
    );
    assert_eq!(
        failures("review its `db`", (Reads::Inside("gate_config"), None)),
        ["`db` is not a key of the report's `gate_config`"]
    );
    // A value the report does not hold there, and one where none belongs.
    assert_eq!(
        failures("`gate_config` whose `state` is `\"pending\"`", nested),
        ["`\"pending\"` is quoted, and the report holds Some(String(\"approved\")) at /gate_config/state"]
    );
    assert_eq!(
        failures(
            "a `gate_config` of `null`",
            (Reads::TopLevel, Some("/gate_config"))
        )
        .len(),
        1
    );
    assert_eq!(
        failures("use `db`, never `\"approved\"`", (Reads::TopLevel, None)),
        ["`\"approved\"` is quoted, and this sentence is not expected to quote a value"]
    );
    // A command is not a part of the report, and a sentence has to name one.
    assert_eq!(
        failures("run `onboard`, then `approve`", (Reads::TopLevel, None)),
        ["names no part of the report"]
    );
}

/// The `--json` example in `docs/AUTOPILOT.md` has exactly the report's keys,
/// each holding the JSON type the report gives it; its lists hold what the
/// report's lists hold; and the three values a reader branches on are the
/// ones a report of the same repo (onboarded, approved, labels read) gives.
#[test]
fn the_documented_json_example_has_the_reports_shape() {
    let fixture = Fixture::new();
    fixture.propose(REPO, &["cargo test"]);
    fixture.approve(REPO);
    let real = report(&fixture.status(ALL_LABELS, None));

    let doc = std::fs::read_to_string(repo_root().join("docs/AUTOPILOT.md")).unwrap();
    let after = doc
        .split_once("`--json` prints this shape:")
        .expect("docs/AUTOPILOT.md introduces the status JSON example")
        .1;
    let block = after
        .split_once("```json\n")
        .and_then(|(_, rest)| rest.split_once("\n```"))
        .expect("a ```json block follows the introduction")
        .0;
    let example: Value = serde_json::from_str(block)
        .unwrap_or_else(|e| panic!("the documented example is not JSON ({e}):\n{block}"));

    let (mut documented, mut reported) = (BTreeMap::new(), BTreeMap::new());
    shape_of(&example, "", &mut documented);
    shape_of(&real, "", &mut reported);
    assert_eq!(
        documented, reported,
        "docs/AUTOPILOT.md's status example (left) and a real report (right) differ in shape"
    );

    // A string passes for any string above, so the values a consumer
    // branches on are compared too: `"state": "APPROVED"` is the same shape.
    for branched_on in ["/onboarded", "/gate_config/state", "/labels/read"] {
        assert_eq!(
            example.pointer(branched_on),
            real.pointer(branched_on),
            "docs/AUTOPILOT.md's status example gives {branched_on} a value no report of an \
             approved repo whose labels were read gives"
        );
    }

    // An array is one entry above, whatever it holds. Where the example and
    // the report both list something, they list the same type of thing.
    let (mut documented_lists, mut reported_lists) = (BTreeMap::new(), BTreeMap::new());
    element_kinds(&example, "", &mut documented_lists);
    element_kinds(&real, "", &mut reported_lists);
    let mut compared = Vec::new();
    for (path, documented_kinds) in &documented_lists {
        if let Some(reported_kinds) = reported_lists.get(path) {
            assert_eq!(
                documented_kinds, reported_kinds,
                "docs/AUTOPILOT.md's status example lists another type of value in {path}"
            );
            compared.push(path.as_str());
        }
    }
    assert_eq!(
        compared,
        [".gate_config.gate_commands", ".labels.risk"],
        "the example and the fixture's report should both list a gate command and a risk label"
    );
}

#[test]
fn element_kinds_tells_a_list_of_strings_from_a_list_of_anything_else() {
    let mut kinds = BTreeMap::new();
    element_kinds(
        &json!({
            "strings": ["a", "b"],
            "numbers": [1],
            "mixed": ["a", {}],
            "empty": [],
            "inner": { "objects": [{}] },
        }),
        "",
        &mut kinds,
    );
    assert_eq!(
        kinds,
        BTreeMap::from([
            (".strings".to_string(), BTreeSet::from(["string"])),
            (".numbers".to_string(), BTreeSet::from(["number"])),
            (".mixed".to_string(), BTreeSet::from(["object", "string"])),
            (".inner.objects".to_string(), BTreeSet::from(["object"])),
        ])
    );
}

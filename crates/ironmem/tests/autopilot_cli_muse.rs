//! Public-CLI coverage for selecting the Muse Code implementation harness.
//!
//! `muse` is a local stub on an isolated PATH that writes a minimal Muse
//! JSONL journal. Nothing in this test can reach a model or GitHub.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ironmem::autopilot::budget;
use ironmem::autopilot::dispatch_state;
use ironmem::autopilot::{Implementer, IssueRef};
use ironmem::db::schema::Database;

mod autopilot_support;
use autopilot_support::{approve, executable, git, ironmem, isolated_path, make_repo, REPO};

/// Appends `"$PWD|$*"` to `$MUSE_CALLS`, writes a journal on the requested
/// session's stream, and behaves per `$MUSE_MODE`. `$MUSE_COUNT` numbers the
/// invocations so `met` can answer `not_met` first and `met` on the resume.
///
/// `hang` replaces itself, under the same PID, with a process `ps` shows as
/// `<stubs>/muse-bin-1.4.13-test exec <the Muse argv>`, the shape the real
/// launcher's `exec` of the versioned binary leaves: bash renamed by
/// `exec -a`, running the script named `exec` it finds on `PATH`
/// ([`LIVE_MUSE_BODY`]).
const MUSE_STUB: &str = r#"#!/bin/bash
printf '%s\n' "$PWD|$*" >> "$MUSE_CALLS"
if [ "$MUSE_MODE" = hang ]; then
  exec -a "${0%/*}/muse-bin-1.4.13-test" /bin/bash "$@"
fi
sid=""; prompt=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --session-id) shift; sid="$1" ;;
    --prompt-file) shift; prompt="$1" ;;
  esac
  shift
done
[ -r "$prompt" ] || exit 9
n=$(cat "$MUSE_COUNT" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$MUSE_COUNT"
# Muse's measured refusals (1.4.13 and 1.4.14): stderr only, no journal, exit 1.
case "$MUSE_MODE:$n" in
  inuse:*) printf 'session %s is already in use\n' "$sid" >&2; /bin/sleep 0.5; exit 1 ;;
  unresumable:1|unresumable:2)
    printf 'failed to read retained session %s: session %s was created with provider echo; refusing to resume with provider meta; start a new session to switch providers\n' "$sid" "$sid" >&2
    /bin/sleep 0.5; exit 1 ;;
esac
rec() { printf '{"schema_version":1,"stream":{"kind":"%s","id":"%s"},"payload":%s}\n' "$1" "$2" "$3"; }
rec session "$sid" '{"kind":"command_accepted"}'
rec exec.usage "$sid" '{"kind":"run_usage","completed_calls":3}'
case "$MUSE_MODE:$n" in
  met:1) rec session "$sid" '{"kind":"run_terminal","terminal":"completed","text":"{\"verdict\":\"not_met\",\"reason\":\"keep going\"}","reason":null}' ;;
  met:*|unresumable:*) printf '42\n' > answer.txt
         rec session "$sid" '{"kind":"run_terminal","terminal":"completed","text":"{\"verdict\":\"met\",\"reason\":\"done\"}","reason":null}' ;;
  failed:*) rec session "$sid" '{"kind":"run_terminal","terminal":"failed","text":"","reason":"transport error"}'; /bin/sleep 0.5; exit 1 ;;
  killed:*) /bin/sleep 0.5; exit 143 ;;
  wrongsid:*) rec session 11111111-2222-3333-4444-555555555555 '{"kind":"run_terminal","terminal":"completed","text":"{\"verdict\":\"met\",\"reason\":\"x\"}","reason":null}' ;;
  unsigned:*) GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
                git -c commit.gpgsign=false commit -q --allow-empty -m bypass
              rec session "$sid" '{"kind":"run_terminal","terminal":"completed","text":"{\"verdict\":\"met\",\"reason\":\"done\"}","reason":null}' ;;
esac
/bin/sleep 0.5
"#;

/// What the live stub runs: it stays up until `$MUSE_RELEASE` exists (or a
/// minute passes), then exits like a killed run, with no terminal record.
const LIVE_MUSE_BODY: &str = r#"#!/bin/bash
i=0
while [ ! -e "$MUSE_RELEASE" ] && [ "$i" -lt 600 ]; do /bin/sleep 0.1; i=$((i+1)); done
/bin/sleep 0.5
exit 143
"#;

fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// The next fixture's issue number. Every fixture works its own issue: the
/// tests run in parallel, and a stub `muse` (a bash script, as the real
/// launcher is) shows in `ps` as a Muse root carrying its invocation's
/// marker. Autopilot rightly holds an issue while any process runs an
/// invocation for it, so fixtures sharing one would hold each other.
static NEXT_ISSUE: AtomicU64 = AtomicU64::new(7);

/// One prepared fixture: an approved repo, a stubs dir holding only `muse`,
/// the stub's bookkeeping files, and an issue number no other fixture uses.
struct Fixture {
    root: tempfile::TempDir,
    issue: u64,
    issue_arg: String,
    checkout: PathBuf,
    worktrees: PathBuf,
    stubs: PathBuf,
    db_path: PathBuf,
    calls: PathBuf,
    count: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let checkout = root.path().join("checkout");
        let worktrees = root.path().join("worktrees");
        let stubs = root.path().join("stubs");
        let db_path = root.path().join("memory.sqlite3");
        let calls = root.path().join("muse.calls");
        let count = root.path().join("muse.count");
        make_repo(&checkout);
        std::fs::create_dir_all(&stubs).unwrap();
        approve(&db_path);
        executable(&stubs.join("muse"), MUSE_STUB);
        executable(&stubs.join("exec"), LIVE_MUSE_BODY);
        let issue = NEXT_ISSUE.fetch_add(1, Ordering::Relaxed);
        Fixture {
            root,
            issue,
            issue_arg: issue.to_string(),
            checkout,
            worktrees,
            stubs,
            db_path,
            calls,
            count,
        }
    }

    /// `ironmem autopilot run owner/repo <issue> --implementer muse --model
    /// muse-wonder-1.0 … --json` plus `extra`, with the stub in `mode`.
    fn run(&self, mode: &str, extra: &[&str]) -> Output {
        self.run_command(mode, extra).output().unwrap()
    }

    fn run_command(&self, mode: &str, extra: &[&str]) -> Command {
        let mut args = vec![
            "autopilot",
            "run",
            REPO,
            &self.issue_arg,
            "--title",
            "t",
            "--implementer",
            "muse",
            "--model",
            "muse-wonder-1.0",
            "--db",
            self.db_path.to_str().unwrap(),
            "--path",
            self.checkout.to_str().unwrap(),
            "--worktree-root",
            self.worktrees.to_str().unwrap(),
            "--json",
        ];
        args.extend_from_slice(extra);
        let mut command = ironmem();
        command
            .args(&args)
            .env("PATH", isolated_path(&self.stubs))
            .env("MUSE_CALLS", &self.calls)
            .env("MUSE_COUNT", &self.count)
            .env("MUSE_MODE", mode)
            .env("MUSE_RELEASE", self.root.path().join("release"));
        command
    }

    /// `ironmem autopilot clear-dispatch owner/repo <issue> --json`.
    fn clear(&self) -> Output {
        self.clear_as(true)
    }

    /// `ironmem autopilot clear-dispatch owner/repo <issue>`, with `--json`
    /// when `json`.
    fn clear_as(&self, json: bool) -> Output {
        let mut command = ironmem();
        command
            .args([
                "autopilot",
                "clear-dispatch",
                REPO,
                &self.issue_arg,
                "--db",
                self.db_path.to_str().unwrap(),
            ])
            .env("PATH", isolated_path(&self.stubs));
        if json {
            command.arg("--json");
        }
        command.output().unwrap()
    }

    /// Replace the content of this fixture's dispatch-state drawer with
    /// `content`, as a newer binary (or a corruption) would leave it.
    fn overwrite_dispatch_state(&self, content: &str) {
        let db = Database::open(&self.db_path).unwrap();
        let source = format!("%dispatch-state:owner-repo-{}", self.issue);
        let updated = db
            .with_connection(|conn| {
                Ok(conn.execute(
                    "UPDATE drawers SET content = ?1 WHERE source_file LIKE ?2",
                    rusqlite::params![content, source],
                )?)
            })
            .unwrap();
        assert_eq!(updated, 1, "exactly one dispatch-state drawer to overwrite");
    }

    /// Every live process `ps` shows as this fixture's versioned Muse root:
    /// `(pid, args)`.
    fn live_muse_roots(&self) -> Vec<(u32, String)> {
        let unique = self.root.path().file_name().unwrap().to_str().unwrap();
        let ps = Command::new("/bin/ps")
            .args(["-A", "-o", "pid=,args="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&ps.stdout)
            .lines()
            .filter_map(|line| {
                let (pid, args) = line.trim().split_once(' ')?;
                (args.contains("/muse-bin-1.4.13-test exec ") && args.contains(unique))
                    .then(|| (pid.parse().unwrap(), args.trim().to_string()))
            })
            .collect()
    }

    /// Start `autopilot run` in the background with the stub in `hang` mode,
    /// and wait until `ps` shows its Muse root.
    fn start_live(&self, extra: &[&str]) -> LiveMuse<'_> {
        let controller = self
            .run_command("hang", extra)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let live = LiveMuse {
            fx: self,
            controller: Some(controller),
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.live_muse_roots().is_empty() {
            assert!(
                Instant::now() < deadline,
                "the live Muse root never appeared; calls={}",
                self.calls()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        live
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(&self.calls).unwrap_or_default()
    }

    fn issue_worktree(&self) -> PathBuf {
        self.worktrees.join(format!("owner-repo-{}", self.issue))
    }
}

/// A background `autopilot run` whose Muse root is live. Dropping it
/// releases the stub and kills whatever is left, so a failed assertion never
/// leaks a process.
struct LiveMuse<'a> {
    fx: &'a Fixture,
    controller: Option<Child>,
}

impl LiveMuse<'_> {
    /// SIGKILL the controller, as a crash would; the Muse root survives it.
    fn crash_controller(&mut self) {
        if let Some(mut controller) = self.controller.take() {
            controller.kill().unwrap();
            controller.wait().unwrap();
        }
    }

    /// SIGKILL the Muse root's process group and wait until `ps` loses it.
    fn kill_muse(&self) {
        for (pid, _) in self.fx.live_muse_roots() {
            // The group first (the supervisor makes the root its leader),
            // then the root itself in case it is not; either may be gone.
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .stderr(Stdio::null())
                .status();
            let _ = Command::new("/bin/kill")
                .args(["-KILL", &pid.to_string()])
                .stderr(Stdio::null())
                .status();
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.fx.live_muse_roots().is_empty() {
            assert!(Instant::now() < deadline, "Muse root survived");
            std::thread::sleep(Duration::from_millis(100));
        }
        // Let the body's last `sleep 0.1` child go too.
        std::thread::sleep(Duration::from_millis(500));
    }
}

impl Drop for LiveMuse<'_> {
    fn drop(&mut self) {
        let _ = std::fs::write(self.fx.root.path().join("release"), "");
        if let Some(mut controller) = self.controller.take() {
            let _ = controller.kill();
            let _ = controller.wait();
        }
        for (pid, _) in self.fx.live_muse_roots() {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", &pid.to_string()])
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn report(output: &Output, what: &str) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "{what}: stdout is not a JSON report ({e}):\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// The value that follows `flag` in one `$PWD|$*` call line.
fn flag_value<'a>(line: &'a str, flag: &str) -> &'a str {
    let argv = line.split_once('|').unwrap().1;
    let mut words = argv.split_whitespace();
    while let Some(word) = words.next() {
        if word == flag {
            return words
                .next()
                .unwrap_or_else(|| panic!("{flag} has no value"));
        }
    }
    panic!("{flag} missing from {line}")
}

#[test]
fn public_run_selects_muse_and_continues_the_same_session() {
    let fx = Fixture::new();
    let output = fx.run("met", &["--attempt-cap", "2"]);

    assert!(
        output.status.success(),
        "run failed:\nstdout={}\nstderr={}\ncalls={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        fx.calls()
    );
    let report = report(&output, "met");
    assert_eq!(report["terminal"]["terminal"], "met", "{report}");
    assert_eq!(report["unpriced_dispatches"], 2, "{report}");
    assert_eq!(
        report["dispatches"].as_array().unwrap().len(),
        2,
        "{report}"
    );
    assert_eq!(report["dispatches"][0]["verdict"], "not_met", "{report}");
    assert_eq!(report["dispatches"][1]["classification"], "met", "{report}");
    assert_eq!(
        std::fs::read_to_string(fx.issue_worktree().join("answer.txt")).unwrap(),
        "42\n"
    );

    let calls = fx.calls();
    let lines: Vec<&str> = calls.lines().collect();
    assert_eq!(lines.len(), 2, "unexpected dispatches: {calls}");
    let sessions: Vec<&str> = lines
        .iter()
        .map(|line| flag_value(line, "--session-id"))
        .collect();
    assert_eq!(
        sessions[0], sessions[1],
        "the resume must continue the same Muse session: {calls}"
    );
    assert!(
        uuid::Uuid::parse_str(sessions[0]).is_ok(),
        "--session-id must be a UUID: {calls}"
    );

    // Canonical: ironmem resolves the worktree root (macOS's /var is
    // /private/var), and the stub's $PWD is the resolved directory.
    let worktree = fx
        .issue_worktree()
        .canonicalize()
        .unwrap()
        .display()
        .to_string();
    let root = fx.root.path().canonicalize().unwrap().display().to_string();
    for line in &lines {
        let (pwd, argv) = line.split_once('|').unwrap();
        assert_eq!(pwd, worktree, "Muse must run in the issue worktree: {line}");
        assert!(argv.starts_with("exec --json "), "{line}");
        assert_eq!(flag_value(line, "--workspace"), worktree, "{line}");
        assert_eq!(flag_value(line, "--model"), "muse-wonder-1.0", "{line}");
        for needle in [
            "--output-schema ",
            "--prompt-file ",
            "--disable-sandbox",
            "--trust-workspace",
            "--no-foreign-personal-context",
            "--disable-request-user-input",
            "--approval-mode never",
        ] {
            assert!(argv.contains(needle), "{needle} missing: {line}");
        }
        // The fixture's own paths may sit under a temp dir whose name says
        // `claude`; only the words ironmem chose may not.
        assert!(
            !argv.replace(&root, "").contains("claude"),
            "a Claude word reached Muse: {line}"
        );
    }
}

#[test]
fn muse_failures_are_never_success_and_each_consumes_an_unpriced_slot() {
    for mode in ["failed", "killed", "wrongsid"] {
        let fx = Fixture::new();
        let day_before = today_utc();
        let output = fx.run(
            mode,
            &[
                "--attempt-cap",
                "1",
                "--max-unpriced-dispatches-per-day",
                "1",
            ],
        );
        let day_after = today_utc();

        assert!(
            output.status.success(),
            "{mode} run failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let report = report(&output, mode);
        assert_ne!(report["terminal"]["terminal"], "met", "{mode}: {report}");
        assert_eq!(report["unpriced_dispatches"], 1, "{mode}: {report}");
        assert_eq!(
            report["dispatches"].as_array().unwrap().len(),
            1,
            "{mode}: {report}"
        );
        assert_eq!(
            report["dispatches"][0]["classification"], "infrastructure_failure",
            "{mode}: {report}"
        );
        assert_eq!(fx.calls().lines().count(), 1, "{mode}: {}", fx.calls());

        // A run straddling UTC midnight books its one slot on either day.
        let db = Database::open(&fx.db_path).unwrap();
        let mut days = vec![day_before];
        if day_after != days[0] {
            days.push(day_after);
        }
        let booked: u32 = days
            .iter()
            .map(|day| {
                budget::get_daily_spend(&db, day)
                    .unwrap()
                    .map_or(0, |entry| entry.unpriced_dispatch_count)
            })
            .sum();
        assert_eq!(booked, 1, "{mode}: ledger must hold exactly one slot");
    }
}

#[test]
fn muse_does_not_require_claude_or_codex_on_path() {
    let fx = Fixture::new();
    let mut stub_names: Vec<String> = std::fs::read_dir(&fx.stubs)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    stub_names.sort();
    // `exec` is the live stub's body, which only the `hang` mode runs.
    assert_eq!(stub_names, ["exec", "muse"]);
    for dir in std::env::split_paths(&isolated_path(&fx.stubs)) {
        for harness in ["claude", "codex"] {
            assert!(
                !dir.join(harness).exists(),
                "{} provides {harness}",
                dir.display()
            );
        }
    }

    let output = fx.run("met", &["--attempt-cap", "2"]);
    assert!(
        output.status.success(),
        "run failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = report(&output, "met");
    assert_eq!(report["terminal"]["terminal"], "met", "{report}");
}

#[test]
fn muse_requires_a_model_before_any_side_effect() {
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("memory.sqlite3");
    let worktrees = root.path().join("worktrees");
    let checkout = root.path().join("checkout");
    let empty_path = root.path().join("empty-path");
    std::fs::create_dir_all(&empty_path).unwrap();

    let output = ironmem()
        .args([
            "autopilot",
            "run",
            REPO,
            "7",
            "--title",
            "t",
            "--implementer",
            "muse",
            "--db",
            db_path.to_str().unwrap(),
            "--path",
            checkout.to_str().unwrap(),
            "--worktree-root",
            worktrees.to_str().unwrap(),
            "--json",
        ])
        .env("PATH", &empty_path)
        .output()
        .unwrap();

    assert!(!output.status.success(), "a Muse run without --model ran");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--model is required for the Muse implementer"),
        "got: {stderr}"
    );
    assert!(
        !db_path.exists(),
        "validation must precede database creation"
    );
    assert!(
        !worktrees.exists(),
        "validation must precede worktree provisioning"
    );
}

/// Mirrors `invalid_model_spelling_is_rejected_before_all_cli_side_effects`
/// for the words only Muse's argv refuses. A refusal that came from the argv
/// inside `run_issue` would already have created the branch and worktree.
#[test]
fn a_muse_model_or_worktree_root_its_argv_refuses_is_rejected_before_all_side_effects() {
    for (case, model, worktree_dir, expected) in [
        ("interior-space", "muse wonder", "worktrees", "whitespace"),
        ("leading-dash", "-muse-wonder", "worktrees", "'-'"),
        ("root-space", "muse-wonder-1.0", "work trees", "whitespace"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("memory.sqlite3");
        let worktrees = root.path().join(worktree_dir);
        let checkout = root.path().join("checkout");
        make_repo(&checkout);
        let empty_path = root.path().join("empty-path");
        std::fs::create_dir_all(&empty_path).unwrap();
        let model_arg = format!("--model={model}");

        let output = ironmem()
            .args([
                "autopilot",
                "run",
                REPO,
                "7",
                "--title",
                "t",
                "--implementer",
                "muse",
                &model_arg,
                "--db",
                db_path.to_str().unwrap(),
                "--path",
                checkout.to_str().unwrap(),
                "--worktree-root",
                worktrees.to_str().unwrap(),
                "--json",
            ])
            .env("PATH", &empty_path)
            .output()
            .unwrap();

        assert!(!output.status.success(), "{case} must fail");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Muse") && stderr.contains(expected),
            "{case}: got: {stderr}"
        );
        assert!(
            !db_path.exists(),
            "{case}: validation must precede database creation"
        );
        assert!(
            !worktrees.exists(),
            "{case}: validation must precede worktree provisioning"
        );
        let branches = git(&checkout, &["branch", "--list", "autopilot/*"]).stdout;
        assert!(
            branches.is_empty(),
            "{case}: validation must precede branch creation: {}",
            String::from_utf8_lossy(&branches)
        );
    }
}

#[test]
fn an_unsigned_muse_commit_is_not_met() {
    let fx = Fixture::new();
    // After the seed commit, so only the stub's bypass commit is unsigned.
    git(&fx.checkout, &["config", "commit.gpgsign", "true"]);

    let output = fx.run("unsigned", &["--attempt-cap", "1"]);
    assert!(
        output.status.success(),
        "run failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = report(&output, "unsigned");
    assert_ne!(report["terminal"]["terminal"], "met", "{report}");
    assert_eq!(
        report["dispatches"].as_array().unwrap().len(),
        1,
        "{report}"
    );
    let dispatch = &report["dispatches"][0];
    assert_eq!(dispatch["classification"], "failed_attempt", "{report}");
    assert_eq!(dispatch["verdict"], "not_met", "{report}");
    let reason = dispatch["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("unsigned"), "{report}");

    // The bypass commit really landed: the guard, not a missing commit,
    // turned the met away.
    let subject = git(&fx.issue_worktree(), &["log", "-1", "--format=%s"]).stdout;
    assert_eq!(String::from_utf8_lossy(&subject).trim(), "bypass");
}

/// The safety invariant behind every hold: a live (or possibly live) Muse
/// invocation keeps its slot, and no run launches a second Muse over it,
/// before or after its controller crashes. The root is a real child of the
/// real supervisor, showing the real argv shape in `ps`.
#[test]
fn a_live_muse_invocation_holds_its_slot_and_no_second_muse_is_launched() {
    let fx = Fixture::new();
    let mut live = fx.start_live(&["--attempt-cap", "1"]);

    let roots = fx.live_muse_roots();
    assert_eq!(roots.len(), 1, "{roots:?}");
    let argv = roots[0].1.split_once("muse-bin-1.4.13-test ").unwrap().1;
    let words: Vec<&str> = argv.split_whitespace().collect();
    assert_eq!(words[..3], ["exec", "--json", "--session-id"], "{argv}");
    assert!(uuid::Uuid::parse_str(words[3]).is_ok(), "{argv}");
    assert_eq!(words[4], "--output-schema", "{argv}");
    assert!(
        words[5].starts_with('/') && words[5].ends_with("/schema.json"),
        "{argv}"
    );
    assert_eq!(words[6], "--prompt-file", "{argv}");
    assert_eq!(fx.calls().lines().count(), 1, "{}", fx.calls());

    // "still alive", not merely "uncertain": the hold comes from `ps`
    // matching this invocation's marker on the recorded root, so it pins the
    // marker grammar to the real argv shape.
    let second = fx.run("met", &["--attempt-cap", "1"]);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        !second.status.success() && stderr.contains("another controller holds issue"),
        "a second run must hold while the first controller runs:\nstdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&second.stdout)
    );
    assert_eq!(fx.calls().lines().count(), 1, "{}", fx.calls());

    // A controller crash leaves the root running; the slot is still held.
    live.crash_controller();
    assert_eq!(
        fx.live_muse_roots().len(),
        1,
        "the root outlives its controller"
    );
    let third = fx.run("met", &["--attempt-cap", "1"]);
    let stderr = String::from_utf8_lossy(&third.stderr);
    assert!(
        !third.status.success() && stderr.contains("Muse implementation is still alive"),
        "a run after the crash must hold:\nstdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&third.stdout)
    );
    assert_eq!(fx.calls().lines().count(), 1, "{}", fx.calls());
}

/// `clear-dispatch` on a Muse hold: refused while the root lives, and the
/// release once it is gone, even with no supervisor record to prove it.
#[test]
fn clear_dispatch_refuses_a_live_muse_invocation_and_releases_it_once_killed() {
    let fx = Fixture::new();
    let mut live = fx.start_live(&["--attempt-cap", "1"]);
    live.crash_controller();
    let db = Database::open(&fx.db_path).unwrap();
    let issue = IssueRef::new(REPO, fx.issue);
    let held = dispatch_state::get_dispatch_state(&db, &issue)
        .unwrap()
        .expect("the crashed run's state names its invocation");
    assert_eq!(held.implementer, Implementer::Muse);
    assert!(held.invocation_artifacts.is_some(), "{held:?}");

    let refused = fx.clear();
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success()
            && stderr.contains("refusing to clear")
            && stderr.contains("its Muse implementation is alive"),
        "stdout={} stderr={stderr}",
        String::from_utf8_lossy(&refused.stdout)
    );
    assert_eq!(
        dispatch_state::get_dispatch_state(&db, &issue).unwrap(),
        Some(held.clone())
    );

    live.kill_muse();
    let cleared = fx.clear();
    assert!(
        cleared.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&cleared.stdout),
        String::from_utf8_lossy(&cleared.stderr)
    );
    let report = report(&cleared, "clear-dispatch");
    assert_eq!(report["implementer"], "muse", "{report}");
    assert!(dispatch_state::get_dispatch_state(&db, &issue)
        .unwrap()
        .is_none());
    assert_eq!(fx.calls().lines().count(), 1, "{}", fx.calls());
}

/// `clear-dispatch` on a dispatch state this binary cannot read (one a
/// newer binary wrote for a harness it does not know): released, with its
/// raw content reported scrubbed, both as text and as `--json`, whose
/// untagged shape scripts read.
#[test]
fn clear_dispatch_releases_an_unreadable_state_and_reports_it_scrubbed() {
    for json in [false, true] {
        let fx = Fixture::new();
        // A real state to overwrite: Muse's in-use refusal parks the issue.
        let parked = fx.run("inuse", &["--attempt-cap", "1"]);
        let parked = report(&parked, "inuse");
        assert_eq!(parked["terminal"]["terminal"], "session_in_use", "{parked}");
        // A credential-shaped value, built so no literal one sits in source.
        let secret = format!("{} abcdefgh{}", ["Bear", "er"].concat(), "SECRETSECRET");
        fx.overwrite_dispatch_state(&format!(
            r#"{{"implementer":"some-future-harness","auth":"{secret}"}}"#
        ));
        let db = Database::open(&fx.db_path).unwrap();
        let issue = IssueRef::new(REPO, fx.issue);
        assert!(
            dispatch_state::get_dispatch_state(&db, &issue).is_err(),
            "the state is unreadable now"
        );

        let output = fx.clear_as(json);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "json={json}: stdout={stdout} stderr={stderr}"
        );
        assert!(
            !stdout.contains("SECRETSECRET") && !stderr.contains("SECRETSECRET"),
            "json={json}: the secret was printed: stdout={stdout} stderr={stderr}"
        );
        if json {
            let cleared = report(&output, "clear-dispatch --json");
            assert_eq!(
                cleared["issue"],
                format!("{REPO}#{}", fx.issue),
                "{cleared}"
            );
            let raw = cleared["raw_content"].as_str().unwrap_or_default();
            assert!(raw.contains("some-future-harness"), "{cleared}");
            assert_eq!(cleared["raw_content_truncated"], false, "{cleared}");
            assert!(
                cleared["reason"].as_str().is_some_and(|r| !r.is_empty()),
                "{cleared}"
            );
            assert!(
                cleared.get("implementer").is_none(),
                "not the readable state's shape: {cleared}"
            );
        } else {
            assert!(
                stdout.contains("unreadable dispatch state")
                    && stdout.contains("(secrets scrubbed)")
                    && stdout.contains("some-future-harness"),
                "{stdout}"
            );
        }
        assert!(
            dispatch_state::get_dispatch_state(&db, &issue)
                .unwrap()
                .is_none(),
            "json={json}: the drawer is gone"
        );
    }
}

#[test]
fn max_turns_reaches_muse_as_max_model_steps() {
    let fx = Fixture::new();
    let output = fx.run("met", &["--attempt-cap", "2", "--max-turns", "77"]);
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fx.calls();
    assert_eq!(calls.lines().count(), 2, "{calls}");
    for line in calls.lines() {
        assert_eq!(flag_value(line, "--max-model-steps"), "77", "{line}");
        assert!(!line.contains("--max-turns"), "{line}");
    }
}

/// Muse's "in use" is read from its stderr, never rotated over, and never
/// retried: the run stops after that one dispatch and parks the issue with a
/// reason that says how to release it, and `clear-dispatch` releases it.
#[test]
fn a_muse_session_still_in_use_parks_the_issue_at_the_cli() {
    let fx = Fixture::new();
    let output = fx.run("inuse", &["--attempt-cap", "1"]);
    let report = report(&output, "inuse");
    assert_eq!(report["terminal"]["terminal"], "session_in_use", "{report}");
    let detail = report["terminal"]["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("still in use")
            && detail.contains(&format!(
                "ironmem autopilot clear-dispatch owner/repo {}",
                fx.issue
            )),
        "{report}"
    );
    assert_eq!(fx.calls().lines().count(), 1, "{}", fx.calls());
    let dispatches = report["dispatches"].as_array().unwrap();
    assert_eq!(dispatches.len(), 1, "{report}");
    assert_eq!(dispatches[0]["classification"], "infrastructure_failure");

    let db = Database::open(&fx.db_path).unwrap();
    let issue = IssueRef::new(REPO, fx.issue);
    let parked = dispatch_state::get_dispatch_state(&db, &issue)
        .unwrap()
        .unwrap();
    assert_eq!(parked.state, "paused-session-in-use");
    let cleared = fx.clear();
    assert!(
        cleared.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&cleared.stdout),
        String::from_utf8_lossy(&cleared.stderr)
    );
    assert!(dispatch_state::get_dispatch_state(&db, &issue)
        .unwrap()
        .is_none());
}

/// A session Muse refuses to resume (here, across providers) is replaced
/// after two refusals, from Muse's own stderr, and the run then succeeds.
#[test]
fn an_unresumable_muse_session_is_replaced_at_the_cli() {
    let fx = Fixture::new();
    let output = fx.run("unresumable", &["--attempt-cap", "1"]);
    let report = report(&output, "unresumable");
    assert_eq!(report["terminal"]["terminal"], "met", "{report}");
    let calls = fx.calls();
    let sessions: Vec<&str> = calls
        .lines()
        .map(|line| flag_value(line, "--session-id"))
        .collect();
    assert_eq!(sessions.len(), 3, "{calls}");
    assert_eq!(sessions[0], sessions[1], "{calls}");
    assert_ne!(sessions[1], sessions[2], "{calls}");
    let second = report["dispatches"][1]["reason"]
        .as_str()
        .unwrap_or_default();
    assert!(second.contains("fresh session"), "{report}");
}

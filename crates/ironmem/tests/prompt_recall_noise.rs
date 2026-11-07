//! Issue #375: what the `user-prompt-submit` hook injects for a fixed prompt set,
//! run through the real binary against a fixture memory store. Run with
//! `--nocapture` for the output table the PR quotes.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use ironmem::db::drawers::generate_id;
use ironmem::db::schema::Database;
use ironrace_embed::embedder::EMBED_DIM;
use serde_json::Value;

mod git_env;

const CHECKPOINT: &str = "Checkpoint acme/widgets#1 (session 380302a1): Tried: inspected worktree";
const HOOK_ROW: &str = "Hook stop ran for session 380302a1 summary";
const MIGRATION_DIARY: &str = "Chose the postgres migration plan with two phases";
const WORKSPACE_LIMIT: &str = "shop-api: the collab task limit is ten per batch";
const SHARED_RUNBOOK: &str = "The shared runbook lists the escalation steps";

/// `(content, wing, room, added_by, filed_at)`. Explicit dates so the unrelated
/// checkpoint is the newest diary row, the one the hook used to inject always.
const ROWS: &[(&str, &str, &str, &str, &str)] = &[
    (WORKSPACE_LIMIT, "shop-api", "docs", "mine", "2026-01-01"),
    (
        "other-app plan: the collab task limit moves to fifteen",
        "other-app",
        "collab-plans",
        "collab",
        "2026-01-02",
    ),
    (
        "other-app tasks: the collab task limit test",
        "other-app",
        "collab-task-lists",
        "collab",
        "2026-01-03",
    ),
    (
        "other-app checkpoint: the collab task limit reached",
        "other-app",
        "collab-checkpoints",
        "mcp",
        "2026-01-04",
    ),
    (
        "other-app draft: the collab task limit proposal",
        "other-app",
        "collab-drafts",
        "mcp",
        "2026-01-05",
    ),
    (
        "other-app note: the collab task limit is unrelated here",
        "other-app",
        "notes",
        "mine",
        "2026-01-06",
    ),
    (
        "yes means the customer accepted the charge",
        "wing-a",
        "docs",
        "mine",
        "2026-01-07",
    ),
    (
        "deploy notes for an unrelated project",
        "wing-a",
        "docs",
        "mine",
        "2026-01-08",
    ),
    (
        "Our billing webhook retry policy backs off for an hour",
        "other-app",
        "notes",
        "mine",
        "2026-01-09",
    ),
    (SHARED_RUNBOOK, "wing-a", "docs", "mine", "2026-01-10"),
    (SHARED_RUNBOOK, "wing-b", "docs", "mine", "2026-01-11"),
    (
        MIGRATION_DIARY,
        "diary",
        "diary",
        "diary",
        "2026-02-01T00:00:00Z",
    ),
    (HOOK_ROW, "diary", "diary", "hook", "2026-02-02T00:00:00Z"),
    (
        CHECKPOINT,
        "diary",
        "diary",
        "diary",
        "2026-02-03T00:00:00Z",
    ),
];

/// `(ending, wing)` of diary entries that match a prompt only past the point where
/// the injected excerpt is cut, so BM25 finds them but what would be injected has no
/// visible connection to the prompt.
const DEEP_DIARY_ROWS: &[(&str, &str)] = &[
    ("kafka partition rebalance", "diary"),
    ("invoice reconciliation", "shop-api"),
];

fn seed(path: &Path) {
    let db = Database::open(path).unwrap();
    db.migrate().unwrap();
    let zero = vec![0.0f32; EMBED_DIM];
    let insert = |content: &str, wing: &str, room: &str, added_by: &str, filed_at: &str| {
        let id = generate_id(content, wing, room);
        db.insert_drawer(&id, content, &zero, wing, room, "", added_by)
            .unwrap();
        db.with_connection(|conn| {
            conn.execute(
                "UPDATE drawers SET filed_at = ?1 WHERE id = ?2",
                rusqlite::params![filed_at, id],
            )?;
            Ok(())
        })
        .unwrap();
    };
    for (content, wing, room, added_by, filed_at) in ROWS {
        insert(content, wing, room, added_by, filed_at);
    }
    for (ending, wing) in DEEP_DIARY_ROWS {
        let content = format!("{}{ending}", "unrelated filler text ".repeat(40));
        insert(&content, wing, "diary", "diary", "2026-01-15T00:00:00Z");
    }
}

/// The hook's injected recall lines for `prompt`, run from `workspace`.
fn recall_lines(db_path: &Path, workspace: &Path, prompt: &str) -> Vec<String> {
    let payload = serde_json::json!({
        "prompt": prompt,
        "session_id": "noise",
        "cwd": workspace,
    })
    .to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ironmem"))
        .args(["hook", "user-prompt-submit", "--harness", "claude-code"])
        .env("HOME", db_path.parent().unwrap().join("home"))
        .env("IRONMEM_DB_PATH", db_path)
        .env("IRONMEM_MODEL_DIR", db_path.parent().unwrap().join("model"))
        .env("IRONMEM_EMBED_MODE", "real")
        .env("IRONMEM_MCP_MODE", "read-only")
        .env("IRONMEM_METRICS", "0")
        // Generous: this test is about content, not latency, and a hook that
        // overruns injects nothing, which would read as a clean "after".
        .env("IRONMEM_PROMPT_HOOK_BUDGET_MS", "1000")
        .env("IRONMEM_PROMPT_RECALL_HYBRID", "false")
        .env_remove("IRONMEM_WORKSPACE_ROOT")
        .env_remove("IRONMEM_DAEMON_SOCKET")
        .env_remove("IRONMEM_PROMPT_HOOK_MAX_HITS")
        .env_remove("IRONMEM_PROMPT_HOOK_MIN_SCORE")
        .env_remove("IRONMEM_PROMPT_HOOK_KG")
        .env_remove("IRONMEM_PROMPT_HOOK_DIARY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "hook failed: {stderr}");
    // "Nothing injected" must mean nothing qualified: a recall that failed, timed out
    // or was skipped would pass every negative case.
    assert!(
        !stderr.contains("prompt-hook recall:"),
        "recall did not run cleanly for {prompt:?}: {stderr}"
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    json.get("hookSpecificOutput")
        .and_then(|hook| hook.get("additionalContext"))
        .and_then(Value::as_str)
        .map(|context| {
            context
                .lines()
                .filter(|line| line.starts_with("- "))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn fixed_prompt_set_injects_only_relevant_recall() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("memory.sqlite3");
    seed(&db_path);
    // Has memory of its own in the fixture, and one with none.
    const MINED: &str = "shop-api";
    const FRESH: &str = "fresh-repo";
    for workspace in [MINED, FRESH] {
        std::fs::create_dir_all(dir.path().join(workspace)).unwrap();
    }

    // `(workspace, prompt, the content fragment of each line that must be injected)`.
    let cases: &[(&str, &str, &[&str])] = &[
        // The workspace's own row; no collab rows and no other project.
        (MINED, "the collab task limit", &[WORKSPACE_LIMIT]),
        // No drawer matches; only the diary entry that is about the prompt.
        (
            MINED,
            "why did we choose the postgres migration plan",
            &[MIGRATION_DIARY],
        ),
        // One-word replies and acknowledgements.
        (MINED, "yes", &[]),
        (MINED, "ok", &[]),
        (MINED, "continue", &[]),
        (MINED, "go ahead", &[]),
        (MINED, "thanks!", &[]),
        (MINED, "deploy", &[]),
        // Hook metadata is never recalled, even when the prompt matches it.
        (FRESH, "hook stop session summary", &[]),
        // A workspace with memory stays in it, however specific the prompt.
        (MINED, "billing webhook retry policy", &[]),
        // One with none reaches other projects, but only on a specific prompt.
        (
            FRESH,
            "billing webhook retry policy",
            &["billing webhook retry"],
        ),
        (FRESH, "billing webhook", &[]),
        // Collab rows stay out there too; the same file in two wings appears once.
        (
            FRESH,
            "the collab task limit",
            &[WORKSPACE_LIMIT, "other-app note"],
        ),
        (
            FRESH,
            "the shared runbook escalation steps",
            &[SHARED_RUNBOOK],
        ),
        // A long prompt shares a couple of words with most diary entries; that alone
        // is not a match.
        (
            MINED,
            "Please refactor the TOML config loader so environment overrides win over \
             file values, keep the defaults in one table, and add tests for precedence. \
             I inspected the worktree earlier and the loader is the only caller left.",
            &[],
        ),
        // A diary entry BM25 finds only past the excerpt's cut, in the shared diary
        // wing and in the workspace's own: the text injected would be unrelated.
        (FRESH, "kafka partition rebalance", &[]),
        (MINED, "invoice reconciliation", &[]),
    ];

    let results: Vec<Vec<String>> = cases
        .iter()
        .map(|(workspace, prompt, _)| recall_lines(&db_path, &dir.path().join(workspace), prompt))
        .collect();

    // Print every result before asserting any, so one run shows the whole table.
    for ((workspace, prompt, _), lines) in cases.iter().zip(&results) {
        eprintln!("[{workspace}] prompt: {prompt}");
        if lines.is_empty() {
            eprintln!("  (nothing injected)");
        }
        for line in lines {
            eprintln!("  {line}");
        }
    }

    for ((workspace, prompt, expected), lines) in cases.iter().zip(&results) {
        assert_eq!(
            lines.len(),
            expected.len(),
            "wrong line count for {prompt:?} in {workspace}: {lines:#?}"
        );
        for fragment in *expected {
            assert!(
                lines.iter().any(|line| line.contains(fragment)),
                "{prompt:?} is missing {fragment:?}: {lines:#?}"
            );
        }
    }
}

/// Fixture git: scrubbed, and reading no operator config.
fn git(dir: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    git_env::fixture_git_env(&mut command);
    let output = command
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git must be available");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    git(repo, &["init", "-q"]);
    git(
        repo,
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    );
}

fn write_doc(root: &Path, name: &str, content: &str) {
    let docs = root.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join(name), content).unwrap();
}

/// The real `ironmem mine`, so the wings under test are the ones mining names.
fn mine(db_path: &Path, root: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_ironmem"))
        .arg("mine")
        .arg(root)
        .env("HOME", db_path.parent().unwrap().join("home"))
        .env("IRONMEM_DB_PATH", db_path)
        .env("IRONMEM_MODEL_DIR", db_path.parent().unwrap().join("model"))
        .env("IRONMEM_EMBED_MODE", "noop")
        .env("IRONMEM_MCP_MODE", "trusted")
        .env("IRONMEM_METRICS", "0")
        .env_remove("IRONMEM_WORKSPACE_ROOT")
        .env_remove("IRONMEM_DAEMON_SOCKET")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "mine failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn linked_worktree_recalls_what_mining_stored_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("memory.sqlite3");
    let main_checkout = dir.path().join("shop-api");
    let linked = dir.path().join("feature-x");
    let other_project = dir.path().join("other-app");

    init_repo(&main_checkout);
    git(
        &main_checkout,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            linked.to_str().unwrap(),
        ],
    );
    init_repo(&other_project);

    const LINKED_DOC: &str = "The ledger rounding rules keep four decimals";
    const OTHER_DOC: &str = "Our billing webhook retry policy backs off for an hour";
    const MAIN_DOC: &str = "The pricing tiers renew on the first of the month";
    write_doc(&linked, "ledger.md", LINKED_DOC);
    // Tracked, so `docs` is a directory of the checkout and not a project inside it.
    git(&linked, &["add", "docs/ledger.md"]);
    write_doc(&other_project, "webhooks.md", OTHER_DOC);
    mine(&db_path, &linked);
    mine(&db_path, &other_project);

    // The worktree's own two-word match: below the bar for reaching other wings,
    // so it only appears if the hook searches the wing mining actually wrote.
    let lines = recall_lines(&db_path, &linked, "ledger rounding");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(
        lines[0].contains("source=\"feature-x/docs\"") && lines[0].contains(LINKED_DOC),
        "{lines:#?}"
    );

    // A subdirectory of the worktree is the same workspace.
    let lines = recall_lines(&db_path, &linked.join("docs"), "ledger rounding");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(lines[0].contains(LINKED_DOC), "{lines:#?}");

    // The worktree has memory, so another project's match stays out, however
    // specific the prompt.
    assert_eq!(
        recall_lines(&db_path, &linked, "billing webhook retry policy"),
        Vec::<String>::new()
    );

    // The repository's wing, shared by every worktree of it, still counts as
    // this workspace's memory when the worktree's own wing has no hit.
    write_doc(&main_checkout, "pricing.md", MAIN_DOC);
    mine(&db_path, &main_checkout);
    let lines = recall_lines(&db_path, &linked, "pricing tiers");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(
        lines[0].contains("source=\"shop-api/docs\"") && lines[0].contains(MAIN_DOC),
        "{lines:#?}"
    );
}

fn write_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[test]
fn non_git_workspace_recalls_every_wing_mining_gave_its_files() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("memory.sqlite3");
    let notes = dir.path().join("notes");
    let fresh = dir.path().join("fresh");
    std::fs::create_dir_all(&fresh).unwrap();

    const LEDGER: &str = "The ledger rounding rules keep four decimals";
    const NESTED: &str = "The invoice numbering scheme restarts every year";
    const ROOT_FILE: &str = "The ledger rounding overview lives at the top level";
    write_file(&notes, "docs/ledger.md", LEDGER);
    write_file(&notes, "projects/2026/invoices.md", NESTED);
    mine(&db_path, &notes);

    // Mining files these under `docs` and `projects`, and nothing under `notes`.
    let lines = recall_lines(&db_path, &notes, "ledger rounding");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(
        lines[0].contains("source=\"docs/general\"") && lines[0].contains(LEDGER),
        "{lines:#?}"
    );
    let lines = recall_lines(&db_path, &notes, "invoice numbering");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(
        lines[0].contains("source=\"projects/2026\"") && lines[0].contains(NESTED),
        "{lines:#?}"
    );

    // Those files are its memory, so another project's match stays out even though
    // nothing is filed under the workspace's own name.
    const OTHER: &str = "Our billing webhook retry policy backs off for an hour";
    write_file(&fresh, "elsewhere/webhooks.md", OTHER);
    mine(&db_path, &fresh);
    assert_eq!(
        recall_lines(&db_path, &notes, "billing webhook retry policy"),
        Vec::<String>::new()
    );

    // A root-level file fills the workspace's own wing without hiding the others,
    // for a prompt that matches only them or one that matches both.
    write_file(&notes, "overview.md", ROOT_FILE);
    mine(&db_path, &notes);
    let lines = recall_lines(&db_path, &notes, "invoice numbering scheme restarts");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(lines[0].contains(NESTED), "{lines:#?}");
    let lines = recall_lines(&db_path, &notes, "ledger rounding");
    assert_eq!(lines.len(), 2, "{lines:#?}");
    for expected in [LEDGER, ROOT_FILE] {
        assert!(
            lines.iter().any(|line| line.contains(expected)),
            "{lines:#?}"
        );
    }
}

#[test]
fn a_wing_name_shared_with_another_workspace_does_not_leak_its_files() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("memory.sqlite3");
    let notes = dir.path().join("notes");
    // Its files start with `notes`' path: only a directory boundary separates them.
    let sibling = dir.path().join("notes-archive");

    const OURS: &str = "The ledger rounding rules keep four decimals";
    const THEIRS: &str = "The pricing tiers renew on the first of the month";
    write_file(&notes, "docs/ledger.md", OURS);
    write_file(&sibling, "docs/pricing.md", THEIRS);
    mine(&db_path, &notes);
    mine(&db_path, &sibling);

    // Both are in wing `docs`, and only one is under this workspace.
    assert_eq!(
        recall_lines(&db_path, &notes, "pricing tiers"),
        Vec::<String>::new()
    );
    assert_eq!(
        recall_lines(&db_path, &notes, "pricing tiers renew month"),
        Vec::<String>::new()
    );
    let lines = recall_lines(&db_path, &notes, "ledger rounding");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(lines[0].contains(OURS), "{lines:#?}");
}

#[test]
fn another_roots_files_mined_into_the_workspaces_own_wing_stay_out() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("memory.sqlite3");
    let notes = dir.path().join("notes");
    let other = dir.path().join("other");

    const OURS: &str = "The ledger rounding rules keep four decimals";
    const THEIRS: &str = "The pricing tiers renew on the first of the month";
    write_file(&notes, "docs/ledger.md", OURS);
    // Mining `other` files this under its first directory: wing `notes`, the
    // workspace's own wing name, from a path outside the workspace.
    write_file(&other, "notes/pricing.md", THEIRS);
    mine(&db_path, &notes);
    mine(&db_path, &other);

    assert_eq!(
        recall_lines(&db_path, &notes, "pricing tiers"),
        Vec::<String>::new()
    );
    assert_eq!(
        recall_lines(&db_path, &notes, "pricing tiers renew month"),
        Vec::<String>::new()
    );
    let lines = recall_lines(&db_path, &notes, "ledger rounding");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(lines[0].contains(OURS), "{lines:#?}");
}

#[test]
fn git_subdirectory_mine_is_recalled_from_anywhere_in_the_repository() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("memory.sqlite3");
    let repo = dir.path().join("shop-api");
    init_repo(&repo);
    let crates = repo.join("crates");

    const TOP: &str = "The ledger rounding rules keep four decimals";
    const NESTED: &str = "The invoice numbering scheme restarts every year";
    write_file(&crates, "ledger.md", TOP);
    write_file(&crates, "core/invoices.md", NESTED);
    // Tracked, so `crates` is a directory of the checkout and not a project inside it.
    git(&repo, &["add", "crates"]);
    // What the stop hook does when a session runs in a subdirectory: no `.git` there,
    // so mining uses its non-Git rules, and nothing lands in wing `shop-api`.
    mine(&db_path, &crates);

    for workspace in [&repo, &crates] {
        let lines = recall_lines(&db_path, workspace, "ledger rounding");
        assert_eq!(lines.len(), 1, "from {workspace:?}: {lines:#?}");
        assert!(
            lines[0].contains("source=\"crates/general\"") && lines[0].contains(TOP),
            "{lines:#?}"
        );
        let lines = recall_lines(&db_path, workspace, "invoice numbering");
        assert_eq!(lines.len(), 1, "from {workspace:?}: {lines:#?}");
        assert!(
            lines[0].contains("source=\"core/general\"") && lines[0].contains(NESTED),
            "{lines:#?}"
        );
    }

    // Another checkout's subdirectory mine shares those wing names and stays out.
    let other = dir.path().join("other-app");
    init_repo(&other);
    const THEIRS: &str = "The pricing tiers renew on the first of the month";
    write_file(&other.join("crates"), "pricing.md", THEIRS);
    mine(&db_path, &other.join("crates"));
    assert_eq!(
        recall_lines(&db_path, &repo, "pricing tiers"),
        Vec::<String>::new()
    );
}

#[test]
fn main_checkout_subdirectory_mine_counts_as_the_linked_worktrees_repository() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("memory.sqlite3");
    let main_checkout = dir.path().join("shop-api");
    let linked = dir.path().join("feature-x");
    init_repo(&main_checkout);
    git(
        &main_checkout,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            linked.to_str().unwrap(),
        ],
    );

    const MAIN_DOC: &str = "The pricing tiers renew on the first of the month";
    write_file(&main_checkout.join("crates"), "pricing.md", MAIN_DOC);
    mine(&db_path, &main_checkout.join("crates"));

    let lines = recall_lines(&db_path, &linked, "pricing tiers");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(
        lines[0].contains("source=\"crates/general\"") && lines[0].contains(MAIN_DOC),
        "{lines:#?}"
    );
}

#[test]
fn non_git_projects_inside_a_git_parent_keep_their_memory_apart() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("memory.sqlite3");
    // A dotfiles-style work tree that tracks none of the projects under it
    // (`status.showUntrackedFiles=no`, rather than a `.gitignore`, so mining still
    // reads them).
    let home = dir.path().join("home");
    init_repo(&home);
    let acme = home.join("clients").join("acme-notes");
    let globex = home.join("clients").join("globex-notes");

    const OURS: &str = "Acme contract renewal terms run for two years";
    const THEIRS: &str = "Globex contract renewal terms run for one year";
    write_file(&acme, "contracts/acme.md", OURS);
    write_file(&globex, "contracts/globex.md", THEIRS);
    mine(&db_path, &acme);
    mine(&db_path, &globex);

    let lines = recall_lines(&db_path, &acme, "contract renewal terms");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(lines[0].contains(OURS), "{lines:#?}");
    assert_eq!(
        recall_lines(&db_path, &acme, "globex contract renewal terms"),
        Vec::<String>::new()
    );
}

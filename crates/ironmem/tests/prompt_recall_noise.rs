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

fn seed(path: &Path) {
    let db = Database::open(path).unwrap();
    db.migrate().unwrap();
    let zero = vec![0.0f32; EMBED_DIM];
    for (content, wing, room, added_by, filed_at) in ROWS {
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
    assert!(
        output.status.success(),
        "hook failed: {}",
        String::from_utf8_lossy(&output.stderr)
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

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use ironmem::db::schema::Database;
use ironrace_embed::embedder::EMBED_DIM;
use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ironmem")
}

fn seed_db_file_bulk(path: &Path, n: usize) {
    let db = Database::open(path).unwrap();
    db.migrate().unwrap();
    let zero = vec![0.0f32; EMBED_DIM];
    db.with_connection(|c| Ok(c.execute_batch("BEGIN")?))
        .unwrap();
    for i in 0..n {
        let content = format!("drawer {i} token alpha beta gamma context entry number {i}");
        let id = format!("bench-{i:05}");
        db.insert_drawer(&id, &content, &zero, "bench", "general", "test", "test")
            .unwrap();
    }
    db.with_connection(|c| Ok(c.execute_batch("COMMIT")?))
        .unwrap();
}

fn run_prompt_hook(db_path: &Path, model_dir: &Path, prompt: &str) -> (Value, u128) {
    let payload = serde_json::json!({
        "prompt": prompt,
        "session_id": "timing"
    })
    .to_string();
    let start = Instant::now();
    let mut child = Command::new(bin())
        .arg("hook")
        .arg("user-prompt-submit")
        .arg("--harness")
        .arg("claude-code")
        .env("IRONMEM_DB_PATH", db_path)
        .env("IRONMEM_MODEL_DIR", model_dir)
        .env("IRONMEM_EMBED_MODE", "real")
        .env("IRONMEM_MCP_MODE", "read-only")
        .env("IRONMEM_METRICS", "0")
        .env("IRONMEM_PROMPT_HOOK_BUDGET_MS", "150")
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
    let elapsed = start.elapsed().as_millis();
    assert!(
        output.status.success(),
        "hook failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = serde_json::from_slice(&output.stdout).unwrap();
    (json, elapsed)
}

#[test]
fn user_prompt_submit_binary_p95_under_budget_on_10k_drawers() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("m.sqlite3");
    let model_dir = dir.path().join("missing-model");
    seed_db_file_bulk(&db_path, 10_000);

    let (hit, _) = run_prompt_hook(&db_path, &model_dir, "drawer token alpha beta");
    assert!(
        hit.get("hookSpecificOutput").is_some(),
        "relevant prompt should inject"
    );

    let (miss, _) = run_prompt_hook(&db_path, &model_dir, "zzqqxx nonexistent qwerty");
    assert!(
        miss.get("hookSpecificOutput").is_none(),
        "unrelated prompt should emit nothing"
    );

    let n = 20;
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        let prompt = format!("drawer token alpha number {i}");
        let (json, elapsed) = run_prompt_hook(&db_path, &model_dir, &prompt);
        assert!(
            json.get("hookSpecificOutput").is_some(),
            "timed relevant prompt should inject, not silently time out"
        );
        samples.push(elapsed as u64);
    }
    samples.sort_unstable();
    let p95 = samples[((n as f64 * 0.95) as usize).saturating_sub(1)];
    assert!(
        p95 <= 150,
        "binary p95 {p95}ms exceeds 150ms budget; samples={samples:?}"
    );
}

use std::fs;
use std::path::Path;
use abeval::codex_tokens::{attribute_codex_tokens, TimeWindow};
use chrono::{DateTime, Utc};

fn win(start: &str, end: &str) -> TimeWindow {
    TimeWindow {
        start: DateTime::parse_from_rfc3339(start).unwrap().with_timezone(&Utc),
        end: DateTime::parse_from_rfc3339(end).unwrap().with_timezone(&Utc),
    }
}

fn rollout(cwd: &str, ts: &str, input: u32, cached: u32, output: u32) -> String {
    format!(
        "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{cwd}\",\"timestamp\":\"{ts}\"}}}}\n\
         {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":\
         {{\"input_tokens\":{input},\"cached_input_tokens\":{cached},\"output_tokens\":{output},\
         \"reasoning_output_tokens\":0,\"total_tokens\":{}}}}}}}}}\n",
        input + output
    )
}

fn write_rollout(root: &Path, name: &str, body: &str) {
    let day = root.join("2026").join("06").join("17");
    fs::create_dir_all(&day).unwrap();
    fs::write(day.join(name), body).unwrap();
}

#[test]
fn sums_only_matching_cwd_within_window() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let wt = dir.path().join("wt-task1");
    fs::create_dir_all(&wt).unwrap();
    let wt_str = wt.to_str().unwrap();

    // Matching session (right cwd, in window): 100/40/10 -> total 110.
    write_rollout(&root, "rollout-a.jsonl", &rollout(wt_str, "2026-06-17T05:43:08.955Z", 100, 40, 10));
    // Wrong cwd: excluded.
    write_rollout(&root, "rollout-b.jsonl", &rollout("/somewhere/else", "2026-06-17T05:43:08.955Z", 999, 0, 999));
    // Right cwd but OUTSIDE the window (next day): excluded.
    let day2 = root.join("2026").join("06").join("18");
    fs::create_dir_all(&day2).unwrap();
    fs::write(day2.join("rollout-c.jsonl"), rollout(wt_str, "2026-06-18T05:43:08.955Z", 500, 0, 500)).unwrap();

    let usage = attribute_codex_tokens(&root, &wt, win("2026-06-17T00:00:00Z", "2026-06-17T23:59:59Z")).unwrap();
    assert_eq!(usage.input_tokens, 60); // 100 - 40
    assert_eq!(usage.cache_read_input_tokens, 40);
    assert_eq!(usage.output_tokens, 10);
    assert_eq!(usage.total(), 110);
}

#[test]
fn zero_match_returns_zero_usage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    fs::create_dir_all(&root).unwrap();
    let wt = dir.path().join("wt-none");
    fs::create_dir_all(&wt).unwrap();
    let usage = attribute_codex_tokens(&root, &wt, win("2026-06-17T00:00:00Z", "2026-06-17T23:59:59Z")).unwrap();
    assert_eq!(usage.total(), 0);
}

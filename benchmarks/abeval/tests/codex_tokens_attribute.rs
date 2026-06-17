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

/// TEST 2 — multiple matching sessions with the same worktree cwd are summed.
/// Guards against a last()/short-circuit regression: the existing single-match
/// test would not catch it, but this test writes two rollouts for the same cwd
/// and asserts that both contribute to the accumulator.
#[test]
fn multiple_matching_sessions_are_summed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let wt = dir.path().join("wt-multi");
    std::fs::create_dir_all(&wt).unwrap();
    let wt_str = wt.to_str().unwrap();

    // First rollout: input=100, cached=40, output=10  → net input=60, total=110
    write_rollout(&root, "rollout-x.jsonl", &rollout(wt_str, "2026-06-17T05:00:00Z", 100, 40, 10));
    // Second rollout (same cwd, same day): input=200, cached=50, output=20 → net input=150, total=220
    write_rollout(&root, "rollout-y.jsonl", &rollout(wt_str, "2026-06-17T06:00:00Z", 200, 50, 20));

    let usage = attribute_codex_tokens(&root, &wt, win("2026-06-17T00:00:00Z", "2026-06-17T23:59:59Z")).unwrap();
    // Corrected mapping sums both: net inputs = (100-40) + (200-50) = 60 + 150 = 210
    assert_eq!(usage.input_tokens, (100 - 40) + (200 - 50));
    assert_eq!(usage.cache_read_input_tokens, 40 + 50);
    assert_eq!(usage.output_tokens, 10 + 20);
    assert_eq!(usage.total(), 110 + 220);
}

/// TEST 3 — window boundary inclusivity.
/// A rollout timestamped EXACTLY at window.start must be included (≤ is
/// inclusive). A rollout timestamped one second BEFORE window.start must be
/// excluded. Uses a distinct cwd per rollout so only one can match.
#[test]
fn window_boundary_at_start_is_inclusive() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let wt_in = dir.path().join("wt-in");
    let wt_out = dir.path().join("wt-out");
    std::fs::create_dir_all(&wt_in).unwrap();
    std::fs::create_dir_all(&wt_out).unwrap();
    let wt_in_str = wt_in.to_str().unwrap();
    let wt_out_str = wt_out.to_str().unwrap();

    // Exactly at window.start — must be INCLUDED.
    write_rollout(&root, "rollout-at.jsonl",   &rollout(wt_in_str,  "2026-06-17T08:00:00Z", 300, 0, 30));
    // One second BEFORE window.start — must be EXCLUDED.
    write_rollout(&root, "rollout-before.jsonl", &rollout(wt_out_str, "2026-06-17T07:59:59Z", 999, 0, 999));

    let window = win("2026-06-17T08:00:00Z", "2026-06-17T23:59:59Z");
    let usage = attribute_codex_tokens(&root, &wt_in, window).unwrap();
    // Only the at-boundary rollout (wt_in) should contribute.
    assert_eq!(usage.input_tokens, 300);
    assert_eq!(usage.output_tokens, 30);
    assert_eq!(usage.total(), 330);

    // Confirm the before-boundary rollout is excluded.
    let before_usage = attribute_codex_tokens(&root, &wt_out, window).unwrap();
    assert_eq!(before_usage.total(), 0, "rollout before window.start must be excluded");
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

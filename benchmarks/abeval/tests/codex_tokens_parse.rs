use abeval::codex_tokens::parse_rollout;

/// A real-shaped rollout: session_meta + two token_count events + a noise line.
/// The FINAL token_count is the cumulative one that must be used.
const ROLLOUT: &str = r#"{"timestamp":"2026-06-17T05:43:21.565Z","type":"session_meta","payload":{"id":"abc","timestamp":"2026-06-17T05:43:08.955Z","cwd":"/tmp/wt/task1","cli_version":"0.140.0"}}
{"timestamp":"2026-06-17T05:44:00.000Z","type":"response_item","payload":{"type":"message"}}
{"timestamp":"2026-06-17T05:45:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"reasoning_output_tokens":4,"total_tokens":110}}}}
{"timestamp":"2026-06-17T05:48:53.779Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":358817,"cached_input_tokens":241920,"output_tokens":8420,"reasoning_output_tokens":2150,"total_tokens":367237}}}}
"#;

#[test]
fn parses_cwd_and_final_cumulative_usage() {
    let parsed = parse_rollout(ROLLOUT).unwrap().expect("a usable session");
    assert_eq!(parsed.cwd, "/tmp/wt/task1");
    // Corrected mapping: input excludes cached; output already includes reasoning.
    assert_eq!(parsed.usage.input_tokens, 358817 - 241920); // 116897
    assert_eq!(parsed.usage.cache_read_input_tokens, 241920);
    assert_eq!(parsed.usage.output_tokens, 8420);
    assert_eq!(parsed.usage.cache_creation_input_tokens, 0);
    // The whole point: total() equals Codex's own total_tokens (no double-count).
    assert_eq!(parsed.usage.total(), 367237);
    // started_at comes from session_meta.payload.timestamp.
    assert_eq!(parsed.started_at.to_rfc3339(), "2026-06-17T05:43:08.955+00:00");
}

#[test]
fn rollout_without_token_count_is_none_not_error() {
    let only_meta = r#"{"type":"session_meta","payload":{"cwd":"/tmp/x","timestamp":"2026-06-17T05:43:08.955Z"}}
"#;
    assert!(parse_rollout(only_meta).unwrap().is_none());
}

#[test]
fn rollout_without_session_meta_is_an_error() {
    let no_meta = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":2,"reasoning_output_tokens":0,"total_tokens":12}}}}
"#;
    assert!(parse_rollout(no_meta).is_err());
}

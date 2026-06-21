use abeval::collab_driver::{parse_session_id, ModelTier};
use abeval::collab_live::{
    claude_worker_argv, codex_config, codex_exec_argv, collab_outcome_for, format_worker_failure,
    worker_text_and_usage,
};
use std::path::Path;

#[test]
fn worker_failure_surfaces_stdout_tail_when_stderr_empty() {
    // For `claude -p` the actionable cause (the result event / synthetic error,
    // e.g. a session-limit notice) is printed to STDOUT, not stderr. A failure
    // message built from exit code + stderr alone is useless: it reads
    // "exited Some(1) — stderr:" with nothing after it. The stdout tail must be
    // surfaced so the operator (and Gap 2's classifier) can see the real cause.
    let stdout = "lots of earlier output\nClaude usage limit reached. Resets at 9pm.";
    let msg = format_worker_failure("claude worker", Some(1), "/tmp/wt", "", stdout);
    assert!(msg.contains("Some(1)"), "exit code must be present: {msg}");
    assert!(msg.contains("/tmp/wt"), "location must be present: {msg}");
    assert!(
        msg.contains("Claude usage limit reached"),
        "the stdout tail (the real cause) must be surfaced: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("stdout"),
        "the tail must be clearly labeled as stdout: {msg}"
    );
}

#[test]
fn worker_failure_bounds_the_stdout_tail() {
    // A multi-megabyte transcript must not be dumped wholesale into the error —
    // only a bounded tail (the end, where the terminal result/error lives) is
    // included, and the head is dropped.
    let head = "HEAD_MARKER_SHOULD_BE_DROPPED";
    let big = format!("{head}{}TAIL_MARKER", "x".repeat(8192));
    let msg = format_worker_failure("claude worker", Some(1), "/tmp/wt", "", &big);
    assert!(
        msg.contains("TAIL_MARKER"),
        "the END of stdout (terminal result/error) must be kept: {msg}"
    );
    assert!(
        !msg.contains(head),
        "the head of an oversized transcript must be dropped, not dumped: {msg}"
    );
}

#[test]
fn collab_outcome_mapping_respects_disposition() {
    use abeval::collab_driver::{RunDisposition, PHASE_CODING_COMPLETE, PHASE_CODING_FAILED};

    assert_eq!(
        collab_outcome_for(RunDisposition::Terminal, PHASE_CODING_COMPLETE),
        "completed"
    );
    assert_eq!(
        collab_outcome_for(RunDisposition::Terminal, PHASE_CODING_FAILED),
        "failed"
    );
    assert_eq!(
        collab_outcome_for(RunDisposition::ExcludedRetryable, "WorkerAborted"),
        "excluded"
    );
}

#[test]
fn codex_exec_argv_is_no_shell_and_isolated() {
    let (prog, args) = codex_exec_argv(Path::new("/tmp/wt"), "join sess-1");
    assert_eq!(prog, "codex");
    // -s danger-full-access, -C <worktree>, and the prompt as a single positional.
    assert!(args.windows(2).any(|w| w == ["-s", "danger-full-access"]));
    assert!(args.windows(2).any(|w| w == ["-C", "/tmp/wt"]));
    assert!(args.iter().any(|a| a == "join sess-1"));
    // `--` must immediately precede the prompt: collab.md starts with `---`
    // frontmatter and would otherwise be parsed as a flag.
    assert!(args.windows(2).any(|w| w == ["--", "join sess-1"]));
    // never a shell.
    assert!(!args.iter().any(|a| a == "-c" || a.contains("sh ")));
}

#[test]
fn claude_worker_argv_carries_stream_json_and_mcp_config() {
    let (prog, args) = claude_worker_argv(r#"{"mcpServers":{}}"#, ModelTier::Opus);
    assert_eq!(prog, "claude");
    // stream-json (not the single envelope) so subagent token usage is summed;
    // stream-json itself requires --verbose.
    assert!(args
        .windows(2)
        .any(|w| w == ["--output-format", "stream-json"]));
    assert!(args.iter().any(|a| a == "--verbose"));
    assert!(args.iter().any(|a| a == "--mcp-config"));
    assert!(args.iter().any(|a| a == "-p"));
    // The prompt is pushed last by the caller, so argv must end with `-p --`:
    // worker templates start with `---` frontmatter and would otherwise be parsed
    // as an option ("unknown option '---'").
    assert_eq!(args.last().map(String::as_str), Some("--"));
    assert!(args.windows(2).any(|w| w == ["-p", "--"]));
}

#[test]
fn claude_worker_argv_pins_the_model_tier() {
    // The headless driver must pin `--model` per turn: the turn-template `model:`
    // frontmatter is inert under `claude -p` (only the interactive orchestrator's
    // Agent(model=) honors it). Opus for planning/review, Sonnet for mechanical/
    // implementation turns (memory project_abeval_campaign_model_tiering).
    let (_, opus) = claude_worker_argv(r#"{"mcpServers":{}}"#, ModelTier::Opus);
    assert!(
        opus.windows(2).any(|w| w == ["--model", "opus"]),
        "opus tier must inject `--model opus`: {opus:?}"
    );
    let (_, sonnet) = claude_worker_argv(r#"{"mcpServers":{}}"#, ModelTier::Sonnet);
    assert!(
        sonnet.windows(2).any(|w| w == ["--model", "sonnet"]),
        "sonnet tier must inject `--model sonnet`: {sonnet:?}"
    );
    // The `--model <flag>` pair must precede the trailing `-p --` (the prompt is the
    // last positional), so the model flag is never swallowed as the prompt.
    assert_eq!(sonnet.last().map(String::as_str), Some("--"));
}

#[test]
fn worker_text_extracts_result_and_sums_usage_from_stream_json() {
    // The worker runs with --output-format stream-json --verbose, so the raw
    // stdout is JSONL: assistant events (carrying per-message usage) plus a
    // terminal `result` event. The ABEVAL_SESSION_ID= sentinel lives in the
    // result event's `result` field, NOT in the raw bytes. Usage is summed across
    // every assistant message (parent + any subagent), so a subagent turn is
    // counted — the single-envelope `json` mode would have missed it.
    let transcript = concat!(
        r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
        "\n",
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"id":"msg_parent","usage":{"input_tokens":12,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        "\n",
        r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":{"id":"msg_subagent","usage":{"input_tokens":100,"output_tokens":40,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        "\n",
        r#"{"type":"result","is_error":false,"result":"ABEVAL_SESSION_ID=d234877e-f538-4925-a66b-75c1a2380c74","usage":{"input_tokens":12,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#,
    );

    // Raw transcript: sentinel is NOT line-matchable (JSONL wrappers).
    assert!(parse_session_id(transcript).is_err());

    // Extracted text: sentinel parses; usage is the PARENT + SUBAGENT sum, not the
    // terminal envelope's parent-only top-level usage.
    let wt = worker_text_and_usage(transcript);
    assert!(
        !wt.usage_unparseable,
        "a parseable transcript is not flagged"
    );
    assert_eq!(
        parse_session_id(&wt.text).unwrap(),
        "d234877e-f538-4925-a66b-75c1a2380c74"
    );
    assert_eq!(
        wt.usage.input_tokens,
        12 + 100,
        "subagent input tokens summed in"
    );
    assert_eq!(
        wt.usage.output_tokens,
        3 + 40,
        "subagent output tokens summed in"
    );
}

#[test]
fn worker_text_preserves_terminal_is_error_flag() {
    let transcript = concat!(
        r#"{"type":"assistant","message":{"id":"msg","usage":{"input_tokens":10,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        "\n",
        r#"{"type":"result","is_error":true,"result":"Claude usage limit reached. Resets at 9pm.","usage":{}}"#,
    );

    let wt = worker_text_and_usage(transcript);
    assert!(wt.is_error, "terminal is_error must reach the driver");
    assert_eq!(wt.text, "Claude usage limit reached. Resets at 9pm.");
    assert_eq!(wt.usage.total(), 12);
}

#[test]
fn worker_text_falls_back_to_raw_on_unparseable_envelope() {
    let wt = worker_text_and_usage("not json at all");
    assert_eq!(wt.text, "not json at all");
    assert_eq!(wt.usage.input_tokens, 0);
    assert_eq!(wt.usage.output_tokens, 0);
    // The zero usage here is a FALLBACK, not a measurement — must be flagged so the
    // driver can exclude a completed run that hit it.
    assert!(
        wt.usage_unparseable,
        "an unparseable transcript must flag usage_unparseable"
    );
}

#[test]
fn codex_config_pins_ironmem_mcp_and_avoids_unparseable_app_keys() {
    let toml = codex_config(Path::new("/out/t1/collab.db"));
    // Per memory feedback_codex_app_config_rewrite: keep config minimal so the
    // older CLI can parse it — no service_tier / relative-agent-paths.
    assert!(!toml.contains("service_tier"));
    assert!(!toml.contains("relative-agent-paths"));
    // The ironmem MCP must be configured so Codex actually has the collab tools,
    // pinned to THIS task's DB in trusted (write-enabled) mode.
    // The model must be one the ChatGPT/subscription account supports; gpt-5-codex
    // / gpt-5 return HTTP 400 and no-op the turn.
    assert!(toml.contains(r#"model = "gpt-5.5""#));
    assert!(!toml.contains("gpt-5-codex"));
    assert!(toml.contains("[mcp_servers.ironmem]"));
    assert!(toml.contains(r#"command = "ironmem""#));
    assert!(toml.contains(r#"IRONMEM_DB_PATH = "/out/t1/collab.db""#));
    assert!(toml.contains(r#"IRONMEM_MCP_MODE = "trusted""#));
}

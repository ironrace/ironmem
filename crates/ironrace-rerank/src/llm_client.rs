//! Subprocess client abstraction for LLM rerank.
//!
//! Trait `LlmClient` is the seam: production uses `ClaudeCliClient` (real
//! `claude -p` subprocess), tests use `MockLlmClient` (deterministic fake).

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::response::{LlmResponse, Usage};

/// Single-call interface to an LLM. Synchronous, blocking.
pub trait LlmClient: Send + Sync {
    /// Send `prompt` to the LLM, return the assistant text plus usage metadata.
    /// Errors should NOT leak raw stderr to user-facing layers; sanitize first.
    fn call(&self, prompt: &str) -> Result<LlmResponse>;
}

/// Real client: shells out to the local `claude` CLI in non-interactive mode.
///
/// Auth uses the user's existing Claude Code subscription (no API key,
/// no per-call cost). Trade-off: subprocess startup overhead ~1-3s per call.
///
/// Invocation:
/// ```text
/// claude --model <model> --output-format json --no-session-persistence \
///        --tools "" --disable-slash-commands -p <prompt>
/// ```
pub struct ClaudeCliClient {
    pub(crate) binary: String,
    pub(crate) model: String,
    pub(crate) timeout: Duration,
}

impl ClaudeCliClient {
    pub fn new(model: impl Into<String>, timeout: Duration) -> Self {
        Self {
            binary: "claude".to_string(),
            model: model.into(),
            timeout,
        }
    }
}

/// Parse `claude -p --output-format json` stdout into an `LlmResponse`.
/// `model_fallback` is the client's configured model (used when the envelope
/// omits `model`); `prompt_chars` is the serialized prompt length for chars/4
/// estimation and the recorded `chars` basis.
fn parse_cli_stdout(stdout: &str, model_fallback: &str, prompt_chars: usize) -> LlmResponse {
    let parsed: Option<serde_json::Value> = serde_json::from_str(stdout).ok();

    // text ← `result` field, else the raw stdout (handles non-JSON output).
    let text = parsed
        .as_ref()
        .and_then(|v| v.get("result"))
        .and_then(|r| r.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| stdout.to_string());

    let model = parsed
        .as_ref()
        .and_then(|v| v.get("model"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| model_fallback.to_string());

    // A `usage` object with at least one nonzero token field counts as real.
    let real_usage: Option<Usage> = parsed
        .as_ref()
        .and_then(|v| v.get("usage"))
        .and_then(|u| serde_json::from_value::<Usage>(u.clone()).ok())
        .filter(|u| {
            u.input_tokens != 0
                || u.output_tokens != 0
                || u.cache_creation_input_tokens != 0
                || u.cache_read_input_tokens != 0
        });

    match real_usage {
        Some(usage) => {
            let cost_usd = parsed
                .as_ref()
                .and_then(|v| v.get("total_cost_usd"))
                .and_then(|c| c.as_f64());
            LlmResponse {
                text,
                usage,
                cost_usd,
                model,
                estimated: false,
                prompt_chars,
            }
        }
        None => {
            // chars/4 fallback (ceil). input from prompt, output from text.
            let output_chars = text.chars().count();
            let usage = Usage {
                input_tokens: ceil_div4(prompt_chars),
                output_tokens: ceil_div4(output_chars),
                ..Usage::default()
            };
            LlmResponse {
                text,
                usage,
                cost_usd: None,
                model,
                estimated: true,
                prompt_chars,
            }
        }
    }
}

/// ceil(n / 4), truncated to u32 (prompt sizes never approach u32::MAX).
fn ceil_div4(n: usize) -> u32 {
    n.div_ceil(4) as u32
}

impl LlmClient for ClaudeCliClient {
    fn call(&self, prompt: &str) -> Result<LlmResponse> {
        let started = Instant::now();
        let mut child = Command::new(&self.binary)
            .arg("--model")
            .arg(&self.model)
            .arg("--output-format")
            .arg("json")
            .arg("--no-session-persistence")
            .arg("--tools")
            .arg("")
            .arg("--disable-slash-commands")
            .arg("-p")
            .arg(prompt)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning {} (is `claude` on PATH?)", self.binary))?;

        // Poll-based wall-clock timeout. Crude but std-only.
        let deadline = started + self.timeout;
        loop {
            match child.try_wait()? {
                Some(_status) => break,
                None => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        bail!("claude CLI timed out after {:?}", self.timeout);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }

        let output = child
            .wait_with_output()
            .context("collecting claude output")?;
        if !output.status.success() {
            // Sanitize stderr — log raw at trace level for debugging, but bubble
            // a generic message up.
            let raw_stderr = String::from_utf8_lossy(&output.stderr);
            tracing::trace!(stderr = %raw_stderr, "claude CLI nonzero exit");
            bail!("claude CLI exited with status {}", output.status);
        }
        let stdout = String::from_utf8(output.stdout)
            .map_err(|e| anyhow!("claude stdout not UTF-8: {e}"))?;
        Ok(parse_cli_stdout(
            &stdout,
            &self.model,
            prompt.chars().count(),
        ))
    }
}

/// Direct Anthropic Messages API client. Bypasses the `claude` CLI to avoid
/// subprocess startup cost (~1-3s/call). Requires an API key.
///
/// Auth resolution: caller is responsible for fetching the key (typically from
/// `ANTHROPIC_API_KEY` with `IRONMEM_ANTHROPIC_API_KEY` as a scoped fallback);
/// we just hold whatever string is passed in.
///
/// Responses are parsed directly into `LlmResponse`, preserving the provider's
/// usage block for downstream token accounting.
pub struct AnthropicApiClient {
    pub(crate) api_key: String,
    pub(crate) model: String,
    pub(crate) max_tokens: u32,
    pub(crate) timeout: Duration,
    /// Defaults to `https://api.anthropic.com`. Test seam.
    pub(crate) base_url: String,
}

impl AnthropicApiClient {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>, timeout: Duration) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            max_tokens: 8,
            timeout,
            base_url: "https://api.anthropic.com".to_string(),
        }
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

/// Build the JSON request body for Anthropic Messages API.
///
/// `temperature` is pinned to `0.0` so probe-to-probe results are
/// reproducible. Without this we'd be sampling at the API default (1.0)
/// and small recall deltas (1-2pp on a 50q slice) would be indistinguishable
/// from sampling noise — that bit us during early eval rounds.
fn build_anthropic_body(model: &str, max_tokens: u32, prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "temperature": 0.0,
        "messages": [{"role": "user", "content": prompt}],
    })
}

/// Parse an Anthropic Messages API response into an `LlmResponse`. The API
/// reports a `usage` block, so missing or malformed usage is treated as an
/// invalid response rather than a measured zero-token call. The API carries no
/// dollar cost, so `cost_usd=None`. `prompt_chars` is the serialized prompt
/// length passed from `call`.
fn parse_anthropic_response(
    api_response: &serde_json::Value,
    model_fallback: &str,
    prompt_chars: usize,
) -> Result<LlmResponse> {
    let text = api_response
        .get("content")
        .and_then(|c| c.get(0))
        .and_then(|c0| c0.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("anthropic response missing content[0].text"))?
        .to_string();

    let usage = api_response
        .get("usage")
        .ok_or_else(|| anyhow!("anthropic response missing usage"))?;
    let usage = serde_json::from_value::<Usage>(usage.clone())
        .context("anthropic response has invalid usage")?;

    let model = api_response
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| model_fallback.to_string());

    Ok(LlmResponse {
        text,
        usage,
        cost_usd: None,
        model,
        estimated: false,
        prompt_chars,
    })
}

impl LlmClient for AnthropicApiClient {
    fn call(&self, prompt: &str) -> Result<LlmResponse> {
        let body = build_anthropic_body(&self.model, self.max_tokens, prompt);
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));

        // Mirror mempalace's retry policy: 3 attempts, 3s sleep between, only
        // for transient transport errors (DNS, connect, read timeouts). HTTP
        // 4xx/5xx responses are NOT retried — they indicate a config issue
        // (bad key, bad model name) that won't resolve by retrying.
        let agent = ureq::AgentBuilder::new().timeout(self.timeout).build();
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..3 {
            let result = agent
                .post(&url)
                .set("x-api-key", &self.api_key)
                .set("anthropic-version", "2023-06-01")
                .set("content-type", "application/json")
                .send_json(body.clone());

            match result {
                Ok(resp) => {
                    let parsed: serde_json::Value = resp
                        .into_json()
                        .context("decoding Anthropic response JSON")?;
                    return parse_anthropic_response(&parsed, &self.model, prompt.chars().count());
                }
                Err(ureq::Error::Status(code, resp)) => {
                    // Don't retry config errors. We deliberately do NOT log
                    // the response body: 401/403 responses from Anthropic
                    // can echo a partial API-key suffix or an org id, and
                    // even trace-level logs may be persisted by diagnostics
                    // pipelines. Discard the body without inspection.
                    drop(resp);
                    tracing::trace!(status = code, "anthropic API non-2xx (body suppressed)");
                    bail!("anthropic API returned HTTP {code}");
                }
                Err(ureq::Error::Transport(t)) => {
                    tracing::trace!(error = %t, attempt, "anthropic API transport error");
                    last_err = Some(anyhow!("transport error: {t}"));
                    if attempt < 2 {
                        std::thread::sleep(Duration::from_secs(3));
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("anthropic API: 3 transport failures")))
    }
}

/// Test-only client. Returns a pre-canned response (or Err) on every `call`.
pub struct MockLlmClient {
    pub(crate) response: Result<LlmResponse>,
}

impl MockLlmClient {
    /// Ergonomic text-only constructor: wraps `text` in an `LlmResponse` with
    /// zeroed usage, `estimated=true`, empty `model`, `prompt_chars=0`.
    pub fn ok(response: impl Into<String>) -> Self {
        Self {
            response: Ok(LlmResponse {
                text: response.into(),
                usage: Usage::default(),
                cost_usd: None,
                model: String::new(),
                estimated: true,
                prompt_chars: 0,
            }),
        }
    }
    /// Full-control constructor for usage-asserting tests.
    pub fn ok_response(response: LlmResponse) -> Self {
        Self {
            response: Ok(response),
        }
    }
    /// Error-path fixture: every `call` returns `Err(message)`.
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            response: Err(anyhow!(message.into())),
        }
    }
}

impl LlmClient for MockLlmClient {
    fn call(&self, _prompt: &str) -> Result<LlmResponse> {
        // anyhow::Error isn't Clone, so rebuild on the error path.
        match &self.response {
            Ok(r) => Ok(r.clone()),
            Err(e) => bail!("{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parse_with_usage_is_measured() {
        let stdout = r#"{"type":"result","result":"5","model":"claude-haiku-4-5",
            "total_cost_usd":0.0012,
            "usage":{"input_tokens":120,"output_tokens":3,
                     "cache_creation_input_tokens":0,"cache_read_input_tokens":40}}"#;
        let r = parse_cli_stdout(stdout, "fallback-model", 200);
        assert_eq!(r.text, "5");
        assert_eq!(r.model, "claude-haiku-4-5");
        assert_eq!(r.usage.input_tokens, 120);
        assert_eq!(r.usage.output_tokens, 3);
        assert_eq!(r.usage.cache_read_input_tokens, 40);
        assert_eq!(r.cost_usd, Some(0.0012));
        assert!(!r.estimated);
        assert_eq!(r.prompt_chars, 200);
    }

    #[test]
    fn cli_parse_without_usage_is_estimated() {
        // No `usage` key → chars/4 fallback. prompt_chars=40 → input=ceil(40/4)=10.
        let stdout = r#"{"type":"result","result":"hello world"}"#; // 11 chars → output=ceil(11/4)=3
        let r = parse_cli_stdout(stdout, "fallback-model", 40);
        assert_eq!(r.text, "hello world");
        assert_eq!(r.model, "fallback-model");
        assert!(r.estimated);
        assert_eq!(r.usage.input_tokens, 10);
        assert_eq!(r.usage.output_tokens, 3);
        assert_eq!(r.cost_usd, None);
        assert_eq!(r.prompt_chars, 40);
    }

    #[test]
    fn cli_parse_non_json_falls_back_to_raw_text() {
        let r = parse_cli_stdout("3", "fallback-model", 12);
        assert_eq!(r.text, "3");
        assert!(r.estimated);
        assert_eq!(r.model, "fallback-model");
    }

    #[test]
    fn anthropic_body_shape() {
        let body = build_anthropic_body("claude-haiku-4-5", 8, "hi");
        assert_eq!(body["model"], "claude-haiku-4-5");
        assert_eq!(body["max_tokens"], 8);
        // Pinned to 0 for reproducible eval — see build_anthropic_body docstring.
        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[test]
    fn anthropic_parse_extracts_text_usage_model() {
        let api = serde_json::json!({
            "id": "msg_abc", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "5"}],
            "model": "claude-haiku-4-5", "stop_reason": "end_turn",
            "usage": {"input_tokens": 200, "output_tokens": 2,
                      "cache_creation_input_tokens": 0, "cache_read_input_tokens": 10}
        });
        let r = parse_anthropic_response(&api, "fallback", 80).unwrap();
        assert_eq!(r.text, "5");
        assert_eq!(r.model, "claude-haiku-4-5");
        assert_eq!(r.usage.input_tokens, 200);
        assert_eq!(r.usage.output_tokens, 2);
        assert_eq!(r.usage.cache_read_input_tokens, 10);
        assert!(!r.estimated);
        assert_eq!(r.cost_usd, None);
        assert_eq!(r.prompt_chars, 80);
    }

    #[test]
    fn anthropic_parse_missing_content_errors() {
        let api = serde_json::json!({"id": "msg_abc"});
        assert!(parse_anthropic_response(&api, "fallback", 0).is_err());
    }

    #[test]
    fn anthropic_parse_empty_content_array_errors() {
        let api = serde_json::json!({"content": []});
        assert!(parse_anthropic_response(&api, "fallback", 0).is_err());
    }

    #[test]
    fn anthropic_parse_missing_usage_errors() {
        let api = serde_json::json!({
            "content": [{"type": "text", "text": "5"}],
            "model": "claude-haiku-4-5"
        });
        assert!(parse_anthropic_response(&api, "fallback", 0).is_err());
    }

    #[test]
    fn anthropic_client_builder_sets_fields() {
        let c = AnthropicApiClient::new("sk-ant-xxx", "claude-haiku-4-5", Duration::from_secs(5))
            .with_max_tokens(16)
            .with_base_url("http://localhost:9999");
        assert_eq!(c.api_key, "sk-ant-xxx");
        assert_eq!(c.model, "claude-haiku-4-5");
        assert_eq!(c.max_tokens, 16);
        assert_eq!(c.base_url, "http://localhost:9999");
    }
}

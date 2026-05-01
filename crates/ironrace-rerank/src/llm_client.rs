//! Subprocess client abstraction for LLM rerank.
//!
//! Trait `LlmClient` is the seam: production uses `ClaudeCliClient` (real
//! `claude -p` subprocess), tests use `MockLlmClient` (deterministic fake).

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

/// Single-call interface to an LLM. Synchronous, blocking.
pub trait LlmClient: Send + Sync {
    /// Send `prompt` to the LLM, return the assistant's text response.
    /// Errors should NOT leak raw stderr to user-facing layers; sanitize first.
    fn call(&self, prompt: &str) -> Result<String>;
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

impl LlmClient for ClaudeCliClient {
    fn call(&self, prompt: &str) -> Result<String> {
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
        String::from_utf8(output.stdout).map_err(|e| anyhow!("claude stdout not UTF-8: {e}"))
    }
}

/// Direct Anthropic Messages API client. Bypasses the `claude` CLI to avoid
/// subprocess startup cost (~1-3s/call). Requires an API key.
///
/// Auth resolution: caller is responsible for fetching the key (typically from
/// `ANTHROPIC_API_KEY` with `IRONMEM_ANTHROPIC_API_KEY` as a scoped fallback);
/// we just hold whatever string is passed in.
///
/// Response is wrapped into the same `{"result": "<text>"}` envelope produced
/// by `claude -p --output-format json` so callers can share one parser.
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

/// Extract the assistant text from an Anthropic Messages API response and
/// re-emit it in the `{"result": "<text>"}` envelope used by `claude -p`,
/// so a single parser handles both backends.
fn wrap_anthropic_response(api_response: &serde_json::Value) -> Result<String> {
    let text = api_response
        .get("content")
        .and_then(|c| c.get(0))
        .and_then(|c0| c0.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("anthropic response missing content[0].text"))?;
    Ok(serde_json::json!({"result": text}).to_string())
}

impl LlmClient for AnthropicApiClient {
    fn call(&self, prompt: &str) -> Result<String> {
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
                    return wrap_anthropic_response(&parsed);
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
    pub(crate) response: Result<String>,
}

impl MockLlmClient {
    pub fn ok(response: impl Into<String>) -> Self {
        Self {
            response: Ok(response.into()),
        }
    }
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            response: Err(anyhow!(message.into())),
        }
    }
}

impl LlmClient for MockLlmClient {
    fn call(&self, _prompt: &str) -> Result<String> {
        // Mirror the response by cloning the error chain (anyhow::Error isn't Clone).
        match &self.response {
            Ok(s) => Ok(s.clone()),
            Err(e) => bail!("{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn wrap_anthropic_response_extracts_text() {
        let api = serde_json::json!({
            "id": "msg_abc",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "5"}],
            "model": "claude-haiku-4-5",
            "stop_reason": "end_turn",
        });
        let wrapped = wrap_anthropic_response(&api).unwrap();
        let v: serde_json::Value = serde_json::from_str(&wrapped).unwrap();
        // Re-emitted in the {"result": "<text>"} envelope produced by claude -p,
        // so the existing rerank parser handles it without branching.
        assert_eq!(v["result"], "5");
    }

    #[test]
    fn wrap_anthropic_response_missing_content_errors() {
        let api = serde_json::json!({"id": "msg_abc"});
        assert!(wrap_anthropic_response(&api).is_err());
    }

    #[test]
    fn wrap_anthropic_response_empty_content_array_errors() {
        let api = serde_json::json!({"content": []});
        assert!(wrap_anthropic_response(&api).is_err());
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

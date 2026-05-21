//! Anthropic Messages API client. Retry + cache_control + parse-error addendum.

use crate::constants::*;
use crate::prompt::{ContentBlock, PARSE_RETRY_ADDENDUM};
use anyhow::{Context, Result};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
}

impl Usage {
    pub fn add_assign(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Decision {
    pub id: String,
    pub decision: String, // "valid" | "stale" | "needs_revalidation"
}

#[derive(Debug, Clone)]
pub struct BatchResponse {
    pub decisions: Vec<Decision>,
    pub usage: Usage,
    pub request_id: String,
    pub wall_ms: u64,
}

/// Returned by [`AnthropicClient::score_batch`] when the model's response
/// text fails to parse as `Vec<Decision>` on both the original attempt
/// and the addendum-retry attempt. Carries the raw second-attempt text
/// and request id so the runner can persist a diagnostic sidecar entry
/// and skip the batch instead of aborting the whole run.
#[derive(Debug, thiserror::Error)]
#[error("response parse failed after addendum retry: {err_msg}")]
pub struct ParseFailureError {
    pub raw_text: String,
    pub request_id: String,
    pub err_msg: String,
}

pub struct AnthropicClient {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl AnthropicClient {
    pub fn from_env() -> Result<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .or_else(|_| std::env::var("IRONMEM_ANTHROPIC_API_KEY"))
            .context("ANTHROPIC_API_KEY (or IRONMEM_ANTHROPIC_API_KEY) must be set")?;
        Ok(Self::with_base_url("https://api.anthropic.com".into(), key))
    }

    pub fn with_base_url(base_url: String, api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            api_key,
        }
    }

    /// Dispatch one batch of prompt blocks.
    ///
    /// Two independent retry axes:
    ///   - `transient_attempt` — up to two retries for 5xx/429/network
    ///     errors. Bounded so a flapping upstream cannot exhaust the
    ///     runtime budget.
    ///   - `parse_retried` — at most ONE retry that appends
    ///     [`PARSE_RETRY_ADDENDUM`] after a malformed (non-JSON / wrong-
    ///     shape) model response. This axis is independent of the
    ///     transient axis: a parse retry does not consume a transient
    ///     slot, and a transient retry does not consume the parse-retry
    ///     slot.
    pub async fn score_batch(&self, blocks: Vec<ContentBlock>) -> Result<BatchResponse> {
        let started = std::time::Instant::now();
        let mut attempt_blocks = blocks;
        let mut transient_attempt: usize = 0;
        let mut parse_retried = false;
        let mut cumulative_usage = Usage::default();
        const MAX_TRANSIENT_RETRIES: usize = 2;

        loop {
            let body = build_request_body(&attempt_blocks);
            let resp = self
                .client
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    if transient_attempt < MAX_TRANSIENT_RETRIES {
                        let backoff = backoff_for(transient_attempt);
                        transient_attempt += 1;
                        tracing::warn!(
                            "transient network error: {e}; retrying after {:?}",
                            backoff
                        );
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    return Err(e.into());
                }
            };

            let status = resp.status();
            if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                if transient_attempt < MAX_TRANSIENT_RETRIES {
                    let backoff = backoff_for(transient_attempt);
                    transient_attempt += 1;
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                anyhow::bail!(
                    "API {} after retries: {}",
                    status,
                    resp.text().await.unwrap_or_default()
                );
            }

            let request_id = resp
                .headers()
                .get("request-id")
                .or_else(|| resp.headers().get("anthropic-request-id"))
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unknown")
                .to_string();
            let payload: serde_json::Value = resp.json().await?;

            let usage: Usage = serde_json::from_value(payload["usage"].clone()).unwrap_or_default();
            cumulative_usage.add_assign(&usage);
            let text = payload["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let parse_input = strip_markdown_fence(&text);
            match serde_json::from_str::<Vec<Decision>>(parse_input) {
                Ok(decisions) => {
                    return Ok(BatchResponse {
                        decisions,
                        usage: cumulative_usage,
                        request_id,
                        wall_ms: started.elapsed().as_millis() as u64,
                    });
                }
                Err(_) if !parse_retried => {
                    parse_retried = true;
                    attempt_blocks.push(ContentBlock {
                        text: PARSE_RETRY_ADDENDUM.to_string(),
                        cache_control: None,
                    });
                    continue;
                }
                Err(e) => {
                    return Err(anyhow::Error::from(ParseFailureError {
                        raw_text: text,
                        request_id,
                        err_msg: e.to_string(),
                    }));
                }
            }
        }
    }
}

fn backoff_for(attempt: usize) -> Duration {
    let base_ms = match attempt {
        0 => 250,
        1 => 1000,
        _ => 1000,
    };
    let jitter = rand::thread_rng().gen_range(0..=base_ms / 2);
    Duration::from_millis(base_ms + jitter)
}

#[derive(Serialize)]
struct ApiRequestBody<'a> {
    model: &'a str,
    temperature: f32,
    max_tokens: u32,
    messages: Vec<UserMessage<'a>>,
}

#[derive(Serialize)]
struct UserMessage<'a> {
    role: &'a str,
    content: Vec<ApiContentBlock<'a>>,
}

#[derive(Serialize)]
struct ApiContentBlock<'a> {
    #[serde(rename = "type")]
    block_type: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl<'a>>,
}

#[derive(Serialize)]
struct CacheControl<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
}

/// Strip a single leading ```` ```json ```` or bare ```` ``` ```` fence and
/// the matching trailing ```` ``` ```` from a model response, if present.
///
/// Defensive pre-parse fixup for the documented Anthropic behavior of
/// occasionally wrapping JSON responses in a markdown code-fence despite
/// the prompt asking for bare JSON. Only the OUTERMOST fence pair is
/// stripped — mid-content backticks are left alone — and stripping leaves
/// trailing/leading whitespace handled too. If no recognizable fence is
/// present, the input is returned unchanged so the deeper addendum-retry
/// path remains in charge of true malformation.
pub(crate) fn strip_markdown_fence(s: &str) -> &str {
    let trimmed = s.trim();
    // Find the opening fence (```json or bare ```).
    let after_open = if let Some(rest) = trimmed.strip_prefix("```json") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest
    } else {
        return s;
    };
    // Require a newline directly after the opener — guards against
    // accidentally peeling the leading characters of a non-fenced string
    // that happens to start with three backticks followed by content.
    let after_newline = match after_open.strip_prefix('\n') {
        Some(rest) => rest,
        None => return s,
    };
    // Require a trailing ``` (allow trailing whitespace/newline after it).
    let trimmed_end = after_newline.trim_end();
    let body = match trimmed_end.strip_suffix("```") {
        Some(rest) => rest,
        None => return s,
    };
    body.trim()
}

fn build_request_body<'a>(blocks: &'a [ContentBlock]) -> ApiRequestBody<'a> {
    let content: Vec<_> = blocks
        .iter()
        .map(|b| ApiContentBlock {
            block_type: "text",
            text: &b.text,
            cache_control: b.cache_control.map(|k| CacheControl { kind: k }),
        })
        .collect();
    ApiRequestBody {
        model: MODEL_ID,
        temperature: TEMPERATURE,
        max_tokens: MAX_TOKENS,
        messages: vec![UserMessage {
            role: "user",
            content,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_fence_bare_json_is_unchanged() {
        let s = r#"[{"id":"x","decision":"valid"}]"#;
        assert_eq!(strip_markdown_fence(s), s);
    }

    #[test]
    fn strip_fence_with_language_tag() {
        let s = "```json\n[{\"id\":\"x\",\"decision\":\"valid\"}]\n```";
        assert_eq!(
            strip_markdown_fence(s),
            r#"[{"id":"x","decision":"valid"}]"#
        );
    }

    #[test]
    fn strip_fence_without_language_tag() {
        let s = "```\n[{\"id\":\"x\",\"decision\":\"valid\"}]\n```";
        assert_eq!(
            strip_markdown_fence(s),
            r#"[{"id":"x","decision":"valid"}]"#
        );
    }

    #[test]
    fn strip_fence_with_trailing_whitespace() {
        let s = "```json\n[{\"id\":\"x\",\"decision\":\"valid\"}]\n```\n";
        assert_eq!(
            strip_markdown_fence(s),
            r#"[{"id":"x","decision":"valid"}]"#
        );
    }

    #[test]
    fn strip_fence_with_leading_whitespace() {
        let s = "  ```json\n[{\"id\":\"x\",\"decision\":\"valid\"}]\n```";
        assert_eq!(
            strip_markdown_fence(s),
            r#"[{"id":"x","decision":"valid"}]"#
        );
    }

    #[test]
    fn strip_fence_real_sample_from_parse_failures() {
        // Reconstructed from parse_failures.jsonl line 2
        // (flask-heldout-2026-05-20-canary canary run).
        let raw = "```json\n[\n  {\"id\": \"Field::src.flask.scaffold.Scaffold.name::src/flask/scaffold.py::77\", \"decision\": \"valid\"}\n]\n```";
        let stripped = strip_markdown_fence(raw);
        let parsed: Vec<Decision> =
            serde_json::from_str(stripped).expect("real sample should parse after fence-stripping");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].decision, "valid");
    }

    #[test]
    fn strip_fence_passthrough_when_inner_is_malformed() {
        // Stripping must not mask a deeper malformation: the addendum-
        // retry path is what handles non-JSON.
        let s = "```json\nnot really JSON\n```";
        let stripped = strip_markdown_fence(s);
        // Stripping happens (we got the body), but parsing still fails.
        assert_eq!(stripped, "not really JSON");
        assert!(serde_json::from_str::<Vec<Decision>>(stripped).is_err());
    }

    #[test]
    fn strip_fence_no_strip_on_mid_content_backticks() {
        // A string with ``` in the middle (no leading fence) is unchanged.
        let s = r#"[{"id":"x```y","decision":"valid"}]"#;
        assert_eq!(strip_markdown_fence(s), s);
    }

    #[test]
    fn strip_fence_no_strip_without_newline_after_opener() {
        // Three backticks followed directly by content (no newline) is
        // not a recognized fence — return unchanged.
        let s = "```not a fence```";
        assert_eq!(strip_markdown_fence(s), s);
    }

    #[test]
    fn strip_fence_no_strip_with_open_but_no_close() {
        // Opening fence but no closing fence — leave alone so the
        // addendum-retry path gets a chance to see the raw response.
        let s = "```json\n[{\"id\":\"x\",\"decision\":\"valid\"}]";
        assert_eq!(strip_markdown_fence(s), s);
    }
}

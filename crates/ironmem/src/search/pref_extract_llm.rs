//! LLM-backed implementation of `ironrace_pref_extract::PreferenceExtractor`.
//!
//! The regex extractor produces fragments like `"upgrade my camera flash"` —
//! useful when the question reuses the user's first-person phrasing, but the
//! LongMemEval preference questions almost always reuse *topic vocabulary*
//! ("photography accessories"), not the user's verbs. The LLM extractor asks
//! a small model to summarize the conversation in topic-noun form, producing
//! synth content that embeds closer to question phrasings.

use std::sync::Arc;
use std::time::Duration;

use ironrace_pref_extract::PreferenceExtractor;
use ironrace_rerank::LlmClient;
use serde_json::Value;

const PROMPT_TEMPLATE: &str = "Below are user turns from a conversation. \
Write a 1-2 sentence summary using the kinds of nouns and topics someone \
would use to ASK A QUESTION about this conversation later. Focus on subject \
matter, products, brands, activities, and concerns mentioned. Use natural \
noun phrases. Output only the summary, no preamble, no numbering.

Conversation:
{TEXT}

Summary:";

/// Extractor that calls a single-shot LLM to summarize the conversation in
/// question-vocabulary form. Failure modes (timeout, non-zero exit) return
/// an empty `Vec`, falling back to "no synth doc for this drawer."
pub struct LlmPreferenceExtractor {
    client: Arc<dyn LlmClient>,
}

impl LlmPreferenceExtractor {
    pub fn new(client: Arc<dyn LlmClient>) -> Self {
        Self { client }
    }
}

impl PreferenceExtractor for LlmPreferenceExtractor {
    fn extract(&self, text: &str) -> Vec<String> {
        let prompt = PROMPT_TEMPLATE.replace("{TEXT}", text);
        let raw = match self.client.call(&prompt) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "pref_extract LLM call failed");
                return Vec::new();
            }
        };
        let assistant_text = extract_assistant_text(&raw).unwrap_or(raw);
        let cleaned = assistant_text.trim();
        if cleaned.is_empty() {
            return Vec::new();
        }
        // Return as a single phrase. `synthesize_doc` joins by `". "`; with one
        // entry the join is a no-op so the synth body is exactly this paragraph.
        vec![cleaned.to_string()]
    }
}

/// Pull the assistant text out of the `claude -p --output-format json` envelope:
///   `{"type": "result", "result": "<assistant text>", ...}`
/// The `AnthropicApiClient` re-emits this same envelope via `wrap_anthropic_response`,
/// so both backends share one parser. Falls back to `None` when the input isn't
/// that envelope; caller then uses the raw stdout directly.
fn extract_assistant_text(raw: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw).ok()?;
    if let Some(Value::String(s)) = v.get("result") {
        return Some(s.clone());
    }
    None
}

/// Build a `ClaudeCliClient`-backed extractor with the given model and timeout.
/// Convenience constructor for production wiring.
pub fn cli_extractor(model: impl Into<String>, timeout: Duration) -> LlmPreferenceExtractor {
    let client: Arc<dyn LlmClient> =
        Arc::new(ironrace_rerank::ClaudeCliClient::new(model, timeout));
    LlmPreferenceExtractor::new(client)
}

/// Build an `AnthropicApiClient`-backed extractor (one in-process HTTPS call
/// per `extract`, no subprocess fan-out). Reuses the canonical
/// `ironrace_rerank::AnthropicApiClient` so we have one HTTP client across
/// rerank and pref-extract.
///
/// Panics if neither `ANTHROPIC_API_KEY` nor `IRONMEM_ANTHROPIC_API_KEY` is
/// set — matches `App::ensure_reranker_loaded` for consistency. Misconfig
/// surfaces immediately rather than silently degrading.
pub fn api_extractor(
    model: impl Into<String>,
    max_tokens: u32,
    timeout: Duration,
) -> LlmPreferenceExtractor {
    let key = crate::search::tunables::anthropic_api_key().unwrap_or_else(|| {
        panic!(
            "IRONMEM_PREF_LLM_BACKEND=api requires ANTHROPIC_API_KEY or \
             IRONMEM_ANTHROPIC_API_KEY to be set"
        );
    });
    let client =
        ironrace_rerank::AnthropicApiClient::new(key, model, timeout).with_max_tokens(max_tokens);
    LlmPreferenceExtractor::new(Arc::new(client))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrace_rerank::MockLlmClient;

    #[test]
    fn extracts_summary_from_envelope_response() {
        let mock = MockLlmClient::ok(
            r#"{"type":"result","result":"User discussed Sony A7R IV photography setup, Godox V1 flash, Gitzo tripod, and camera bag accessories."}"#,
        );
        let ex = LlmPreferenceExtractor::new(Arc::new(mock));
        let phrases = ex.extract("I'm upgrading my Sony A7R IV...");
        assert_eq!(phrases.len(), 1);
        assert!(phrases[0].contains("photography"));
        assert!(phrases[0].contains("Godox"));
    }

    #[test]
    fn extracts_summary_from_raw_text_when_not_json() {
        let mock = MockLlmClient::ok(
            "User discussed kitchen organization, slow cooker recipes, and meal prep ideas.",
        );
        let ex = LlmPreferenceExtractor::new(Arc::new(mock));
        let phrases = ex.extract("I've been cooking...");
        assert_eq!(phrases.len(), 1);
        assert!(phrases[0].contains("slow cooker"));
    }

    #[test]
    fn returns_empty_on_llm_error() {
        let mock = MockLlmClient::err("subprocess failed");
        let ex = LlmPreferenceExtractor::new(Arc::new(mock));
        assert!(ex.extract("anything").is_empty());
    }

    #[test]
    fn returns_empty_on_blank_response() {
        let mock = MockLlmClient::ok(r#"{"type":"result","result":"   "}"#);
        let ex = LlmPreferenceExtractor::new(Arc::new(mock));
        assert!(ex.extract("anything").is_empty());
    }
}

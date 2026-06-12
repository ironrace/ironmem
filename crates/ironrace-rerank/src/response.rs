//! Structured result of an `LlmClient::call`. Carries the assistant text plus
//! token-accounting metadata so callers can persist usage to `token_usage`.
//!
//! `Usage` is lifted verbatim (field-for-field) from the provbench baseline
//! (`benchmarks/provbench/baseline/src/client.rs`) so the two token shapes stay
//! identical; `add_assign` keeps the inherent-method form for git-blame parity.

use serde::Deserialize;

/// Anthropic-shaped token counts. All fields default to 0 so a partial or
/// missing `usage` block deserializes cleanly.
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
    /// Saturating field-wise accumulate. Kept as an inherent method (not
    /// `std::ops::AddAssign`) to match the baseline source it was lifted from.
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

/// One LLM call's result: assistant `text` plus accounting metadata.
///
/// `prompt_chars` is the char count of the prompt text passed to `call()`
/// (set at the client boundary, where the real prompt exists). This is the
/// user prompt string itself, not the JSON request envelope the API client
/// wraps it in.
/// `estimated` is true when token counts were derived from a chars/4 heuristic
/// rather than a provider-reported `usage` block.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub text: String,
    pub usage: Usage,
    pub cost_usd: Option<f64>,
    pub model: String,
    pub estimated: bool,
    pub prompt_chars: usize,
}

impl LlmResponse {
    /// Total chars involved in the call: prompt chars + assistant text chars.
    /// Recorded into `token_usage.chars` as the estimation/diagnostic basis.
    pub fn chars(&self) -> usize {
        self.prompt_chars + self.text.chars().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_assign_sums_all_fields() {
        let mut a = Usage {
            input_tokens: 10,
            cache_creation_input_tokens: 1,
            cache_read_input_tokens: 2,
            output_tokens: 3,
        };
        let b = Usage {
            input_tokens: 5,
            cache_creation_input_tokens: 4,
            cache_read_input_tokens: 6,
            output_tokens: 7,
        };
        a.add_assign(&b);
        assert_eq!(a.input_tokens, 15);
        assert_eq!(a.cache_creation_input_tokens, 5);
        assert_eq!(a.cache_read_input_tokens, 8);
        assert_eq!(a.output_tokens, 10);
    }

    #[test]
    fn add_assign_saturates() {
        let mut a = Usage {
            input_tokens: u32::MAX,
            ..Usage::default()
        };
        a.add_assign(&Usage {
            input_tokens: 1,
            ..Usage::default()
        });
        assert_eq!(a.input_tokens, u32::MAX);
    }

    #[test]
    fn chars_is_prompt_plus_text() {
        let r = LlmResponse {
            text: "abcd".to_string(),
            usage: Usage::default(),
            cost_usd: None,
            model: String::new(),
            estimated: true,
            prompt_chars: 6,
        };
        assert_eq!(r.chars(), 10);
    }
}

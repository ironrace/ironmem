//! Select the implementation harness independently of its model.
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::MemoryError;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Implementer {
    #[default]
    Claude,
    Codex,
}

impl FromStr for Implementer {
    type Err = MemoryError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            _ => Err(MemoryError::Validation(format!(
                "unknown implementer '{value}'; expected 'claude' or 'codex'"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::Implementer;

    #[test]
    fn implementer_defaults_to_claude_and_round_trips_snake_case() {
        assert_eq!(Implementer::default(), Implementer::Claude);
        assert_eq!(
            Implementer::from_str("CLAUDE").unwrap(),
            Implementer::Claude
        );
        assert_eq!(Implementer::from_str("CodEx").unwrap(), Implementer::Codex);
        assert_eq!(
            serde_json::to_string(&Implementer::Codex).unwrap(),
            "\"codex\""
        );
        assert_eq!(
            serde_json::from_str::<Implementer>("\"claude\"").unwrap(),
            Implementer::Claude
        );
    }

    #[test]
    fn unknown_implementer_names_both_valid_values() {
        let err = Implementer::from_str("gemini").unwrap_err().to_string();
        assert!(err.contains("expected 'claude' or 'codex'"), "got: {err}");
    }
}

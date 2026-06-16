//! Arms (METRICS_SPEC §11.2) and deterministic assignment.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arm {
    Ironmem,
    Superpowers,
}

impl Arm {
    /// Exact §11.2 label string. Pinned by test.
    pub fn label(self) -> &'static str {
        match self {
            Arm::Ironmem => "ironmem",
            Arm::Superpowers => "superpowers",
        }
    }
}

/// Parse the `--arms` selector into an ordered, deduplicated arm list.
pub fn parse_arms_selector(selector: &str) -> Result<Vec<Arm>> {
    match selector {
        "both" => Ok(vec![Arm::Ironmem, Arm::Superpowers]),
        "ironmem" => Ok(vec![Arm::Ironmem]),
        "superpowers" => Ok(vec![Arm::Superpowers]),
        other => bail!("unknown arms selector {other:?} (expected both|ironmem|superpowers)"),
    }
}

/// Deterministic per-task arm assignment. `_task_id` is reserved for future
/// seeded assignment; today the selector alone fixes the arms (§11.2 default:
/// both arms per task).
pub fn assign_arms(_task_id: &str, selector: &str) -> Result<Vec<Arm>> {
    parse_arms_selector(selector)
}

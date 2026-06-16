//! Execution abstraction: `Usage` (verbatim from provbench baseline), the
//! `ArmExecutor` trait, a deterministic `DryRunExecutor`, and the inert
//! `LiveExecutor` (plumbing only — fully wired/guarded in runner.rs Task 5).

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::arms::Arm;
use crate::corpus::Task;

/// Token accounting — the four §2.1 components. Shape reused verbatim from
/// `benchmarks/provbench/baseline/src/client.rs`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Saturating field-wise accumulation.
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

    /// §2.1 tokens_to_done = input + output + cache_creation + cache_read.
    pub fn total(&self) -> u64 {
        self.input_tokens as u64
            + self.output_tokens as u64
            + self.cache_creation_input_tokens as u64
            + self.cache_read_input_tokens as u64
    }
}

/// Outcome of running one task in one arm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmOutcome {
    pub arm: Arm,
    pub usage: Usage,
    /// `"completed"` for dry-run synthesis; live outcomes are recorded by the
    /// future live path / normalized metric input.
    pub outcome: String,
    pub transcript: String,
}

pub trait ArmExecutor {
    fn execute(&self, task: &Task, arm: Arm) -> Result<ArmOutcome>;
}

/// Deterministic, network-free executor used for the committed smoke path.
pub struct DryRunExecutor;

impl ArmExecutor for DryRunExecutor {
    fn execute(&self, task: &Task, arm: Arm) -> Result<ArmOutcome> {
        // Deterministic synthesized usage derived from stable task/arm bytes.
        let seed = task.id.len() as u32 + arm.label().len() as u32;
        let usage = Usage {
            input_tokens: 1000 + seed,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            output_tokens: 200 + seed,
        };
        Ok(ArmOutcome {
            arm,
            usage,
            outcome: "completed".to_string(),
            transcript: format!("[dry-run] {} :: {}", arm.label(), task.id),
        })
    }
}

/// Inert live executor — its command template is built and guarded in
/// runner.rs (Task 5). It never spawns in this PR's tests.
pub struct LiveExecutor;

impl LiveExecutor {
    /// Build the (inert) command template for an arm. Returns the program and
    /// args that WOULD be spawned. This PR never executes it.
    ///
    /// - `ironmem` arm: starts/joins a `/collab` flow for the task.
    /// - `superpowers` arm: runs the task prompt with superpowers skills ONLY
    ///   (C1: NO `/collab`, NO semantic search/KG/drawer writes, NO ironmem
    ///   server-side state in the working context). Any task_tag/reporting
    ///   instrumentation is measurement-only and kept out of the working path.
    pub fn command_template(&self, task: &Task, arm: Arm) -> (String, Vec<String>) {
        match arm {
            Arm::Ironmem => (
                "claude".to_string(),
                vec![
                    "/collab".to_string(),
                    "start".to_string(),
                    task.prompt.clone(),
                ],
            ),
            Arm::Superpowers => (
                "claude".to_string(),
                // superpowers-alone: plain prompt, no /collab, no ironmem state.
                vec![task.prompt.clone()],
            ),
        }
    }
}

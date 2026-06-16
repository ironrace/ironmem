//! Frozen constants for the abeval harness (METRICS_SPEC §11).

/// Minimum completed tasks per arm before any headline delta (§11.3).
pub const MIN_TASKS_PER_ARM: usize = 8;

/// Inclusive corpus size bounds (§11.1).
pub const CORPUS_MIN: usize = 8;
pub const CORPUS_MAX: usize = 12;

/// Default corpus path relative to the crate root.
pub const DEFAULT_CORPUS_PATH: &str = "corpus/tasks.jsonl";

/// Env var that, when set to a truthy approval string, opts in to paid runs.
pub const APPROVAL_ENV: &str = "ABEVAL_PAID_RUN_APPROVED";

/// Allowed `source` reference prefixes (§2.2 invariant 4).
pub const SOURCE_PREFIXES: [&str; 3] = ["issue:", "pr:", "backlog:"];

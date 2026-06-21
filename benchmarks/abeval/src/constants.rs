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

/// Exact text required in an approval file before a paid live run can proceed.
pub const APPROVAL_FILE_SENTINEL: &str = "I approve paid A/B runs";

/// Allowed `source` reference prefixes — the real-reference shape that encodes
/// the §11.1 "genuine backlog, not synthetic" corpus requirement (README
/// invariant 4).
pub const SOURCE_PREFIXES: [&str; 3] = ["issue:", "pr:", "backlog:"];

/// Outcome string values, centralized so the mint site (executor) and the decode
/// sites (runner orchestration, report aggregation) cannot drift independently.
/// A typo would otherwise compile and silently make rows non-headline-eligible.
/// `COMPLETED`/`FAILED` are agent-level (process exit + envelope); `MERGED` is
/// the §12 done-proxy (agent-completed AND gates-green).
pub const OUTCOME_COMPLETED: &str = "completed";
pub const OUTCOME_FAILED: &str = "failed";
pub const OUTCOME_MERGED: &str = "merged";

/// A run aborted by an EXTERNAL account-wide condition (Claude session/rate
/// limit), surfaced via `collab_driver::RunDisposition::ExcludedRetryable`.
/// Distinct from `FAILED`: an excluded run is NOT a task the arm
/// attempted-and-failed — it is dropped from the corpus row set (so it never
/// dilutes the merged-rate denominator) and must be re-run. Its partial token
/// spend is still persisted (sidecar) for auditability.
pub const OUTCOME_EXCLUDED: &str = "excluded";

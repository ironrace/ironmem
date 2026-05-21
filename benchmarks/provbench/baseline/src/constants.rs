//! SPEC immutables for the Phase 0c LLM-as-invalidator baseline.
//!
//! Token prices and caps are pinned to the §6.2 / §15 snapshot dated
//! 2026-05-09. Edits to any value in this file must be matched by a
//! SPEC §11 change-log entry and a new freeze hash.

// --- Anthropic API pin (SPEC §6.2) ---
pub const MODEL_ID: &str = "claude-sonnet-4-6";
pub const MODEL_SNAPSHOT_DATE: &str = "2026-05-09";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

pub const TEMPERATURE: f32 = 0.0;
pub const MAX_TOKENS: u32 = 4096;
pub const MAX_FACTS_PER_BATCH: usize = 32;

/// Maximum retries on transient errors (SPEC §6.2).
pub const MAX_TRANSIENT_RETRIES: u32 = 2;

/// Default per-minute input-token throttle cap. Set well below
/// Anthropic's 450k ITPM org-cap on `claude-sonnet-4-6` to leave
/// headroom for the parse-retry doublings. Off entirely when the
/// CLI flag is set to `0`. Robustness knob only — does not touch
/// the §10 frozen surface (prompt/scoring/labeler).
pub const DEFAULT_MAX_INPUT_TOKENS_PER_MINUTE: usize = 300_000;

// --- Token prices (SPEC §6.2 / §15 snapshot 2026-05-09) ---

/// $3.00 per 1M input tokens (uncached).
pub const PRICE_INPUT_UNCACHED_USD_PER_MTOK: f64 = 3.00;
/// Cache write: 1.25× the input price per Anthropic prompt-caching pricing.
pub const PRICE_INPUT_CACHE_WRITE_USD_PER_MTOK: f64 = 3.75;
/// Cache read: 0.10× the input price.
pub const PRICE_INPUT_CACHE_READ_USD_PER_MTOK: f64 = 0.30;
pub const PRICE_OUTPUT_USD_PER_MTOK: f64 = 15.00;

// --- Budget caps ---

/// SPEC §6.2 / §15 ceiling — hard upper bound, never exceedable.
pub const SPEC_BUDGET_USD: f64 = 250.00;
/// Operational guardrail (this crate's default). Overridable via `--budget-usd`.
pub const DEFAULT_OPERATIONAL_BUDGET_USD: f64 = 25.00;

// --- Deterministic sampling ---

/// Default sample seed (same constant the labeler uses for spotcheck draws).
pub const DEFAULT_SEED: u64 = 0xC0DE_BABE_DEAD_BEEF;

/// Default per-stratum sample targets (pre-registered for the §9.2 baseline run).
/// `usize::MAX` is the sentinel meaning "take the entire stratum".
pub const TARGET_VALID: usize = 2000;
pub const TARGET_STALE_CHANGED: usize = 2000;
pub const TARGET_STALE_DELETED: usize = 2000;
pub const TARGET_STALE_RENAMED: usize = usize::MAX;
pub const TARGET_NEEDS_REVALIDATION: usize = 2000;

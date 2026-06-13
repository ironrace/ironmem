//! Runtime-overridable search tuning knobs.
//!
//! Most constants here read an `IRONMEM_*` environment variable on first use
//! (via `OnceLock`); the knowledge-graph caps and the metrics knobs read fresh
//! on every call (see their sections). When the variable is absent or
//! unparseable the compile-time default is used, so production behaviour is
//! unchanged with no env overrides.
//!
//! This centralises all tuning parameters in one place and lets the E2 parameter
//! sweep drive experiments purely through environment variables without recompiling.

use std::sync::OnceLock;

// ── helpers ───────────────────────────────────────────────────────────────────

fn env_f32(name: &str, default: f32) -> f32 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name).as_deref() {
        Ok("1" | "true" | "yes") => true,
        Ok("0" | "false" | "no") => false,
        _ => default,
    }
}

// ── pipeline constants ────────────────────────────────────────────────────────

/// Maximum HNSW candidates to overfetch before re-ranking.
pub fn max_overfetch() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| env_usize("IRONMEM_MAX_OVERFETCH", 150))
}

/// RRF k constant (Cormack et al. 2009 default: 60).
pub fn rrf_k() -> f32 {
    static V: OnceLock<f32> = OnceLock::new();
    *V.get_or_init(|| env_f32("IRONMEM_RRF_K", 60.0))
}

/// Minimum token count to fire the content-word query variant.
pub fn content_word_variant_min_tokens() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| env_usize("IRONMEM_CONTENT_WORD_MIN_TOKENS", 13))
}

/// BM25 hit count below which BM25's RRF weight is scaled down.
pub fn bm25_sparse_threshold() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| env_usize("IRONMEM_BM25_SPARSE_THRESHOLD", 5))
}

// ── knowledge-graph fan-out caps ───────────────────────────────────────────────
// Read fresh on every call (no OnceLock): the kg_query MCP tool overrides the
// limit per-request, and integration tests flip these via process-global env
// vars. Call frequency is ≤ once per kg_query / once per kg_boost, so a fresh
// env read is negligible next to the SQLite probes they bound.

/// Maximum currently-valid triples `query_entity_current` returns. Bounds both
/// the triples serialized into `kg_query` MCP responses and the 1-hop fan-out
/// the KG-boost loop walks. Default 50. A `0` override is treated as "use the
/// default" rather than silently truncating every query to nothing.
pub fn kg_query_limit() -> usize {
    nonzero_or("IRONMEM_KG_QUERY_LIMIT", 50)
}

/// Maximum distinct related entities the KG boost fans out to (1-hop) across all
/// mentioned entities. Bounds the entity lookups a high-degree hub would
/// otherwise trigger unbounded. Default 32. A `0` override is treated as "use
/// the default" rather than silently disabling the boost.
pub fn kg_boost_fanout() -> usize {
    nonzero_or("IRONMEM_KG_BOOST_FANOUT", 32)
}

/// Like `env_usize`, but a parsed `0` falls back to `default` — these caps must
/// stay ≥ 1 or they silently disable the feature they bound.
fn nonzero_or(name: &str, default: usize) -> usize {
    match env_usize(name, default) {
        0 => default,
        n => n,
    }
}

// ── rerank weights ────────────────────────────────────────────────────────────

/// Predicate-keyword shrinkage weight (max fraction of distance removed).
pub fn kw_weight() -> f32 {
    static V: OnceLock<f32> = OnceLock::new();
    *V.get_or_init(|| env_f32("IRONMEM_KW_WEIGHT", 0.50))
}

/// Quoted-phrase shrinkage weight.
pub fn quoted_weight() -> f32 {
    static V: OnceLock<f32> = OnceLock::new();
    *V.get_or_init(|| env_f32("IRONMEM_QUOTED_WEIGHT", 0.60))
}

/// Person-name shrinkage weight (kept weak; speaker names recur every session).
pub fn name_weight() -> f32 {
    static V: OnceLock<f32> = OnceLock::new();
    *V.get_or_init(|| env_f32("IRONMEM_NAME_WEIGHT", 0.20))
}

/// Fraction of candidates a token must appear in to be considered corpus-ubiquitous
/// and excluded from overlap scoring (IDF-style dampener).
pub fn high_df_threshold() -> f32 {
    static V: OnceLock<f32> = OnceLock::new();
    *V.get_or_init(|| env_f32("IRONMEM_HIGH_DF_THRESHOLD", 0.80))
}

// ── E3: pseudo-relevance feedback (off by default) ───────────────────────────

pub fn prf_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| env_bool("IRONMEM_PRF_ENABLED", false))
}

/// Number of top candidates used as PRF foreground.
/// Set to 15 to cover preference misses at ranks 6-22 (E1 diagnostic).
pub fn prf_top_k() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| env_usize("IRONMEM_PRF_TOP_K", 15))
}

pub fn prf_terms() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| env_usize("IRONMEM_PRF_TERMS", 4))
}

/// Minimum candidate count required to fire PRF. Corpora smaller than this
/// have too few background docs for meaningful ICF scoring, producing noisy
/// expansion terms that hurt precision. LongMemEval's ~50-session haystack
/// stays below this floor; production corpora with 100+ drawers trigger it.
pub fn prf_min_corpus() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| env_usize("IRONMEM_PRF_MIN_CORPUS", 100))
}

// ── E4: recency boost (off by default) ───────────────────────────────────────

pub fn recency_boost_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| env_bool("IRONMEM_RECENCY_BOOST", false))
}

pub fn recency_half_life_days() -> f32 {
    static V: OnceLock<f32> = OnceLock::new();
    *V.get_or_init(|| env_f32("IRONMEM_RECENCY_HALF_LIFE_DAYS", 30.0))
}

// ── rerank tunables ──────────────────────────────────────────────────────────

/// `IRONMEM_RERANK=llm_haiku` enables the LLM rerank stage.
/// `IRONMEM_RERANK=cross_encoder` is also accepted for legacy compatibility
/// (now routes through the same `LlmReranker` plumbing).
/// Strict string-enum: any other value (including "1", "true") leaves it OFF.
pub fn rerank_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        matches!(
            std::env::var("IRONMEM_RERANK").as_deref(),
            Ok("cross_encoder") | Ok("llm_haiku")
        )
    })
}

/// How many top candidates feed the cross-encoder. Default 20.
pub fn rerank_top_k() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| env_usize("IRONMEM_RERANK_TOP_K", 20))
}

/// Shrinkage rerank (existing step 8) is on by default. Set
/// `IRONMEM_SHRINKAGE_RERANK=0` to disable for eval comparisons.
/// Production default unchanged.
pub fn shrinkage_rerank_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| env_bool("IRONMEM_SHRINKAGE_RERANK", true))
}

/// Model alias passed to `claude --model`. Default `"claude-haiku-4-5"`.
pub fn llm_rerank_model() -> String {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        std::env::var("IRONMEM_LLM_RERANK_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".to_string())
    })
    .clone()
}

/// Wall-clock timeout for one `claude -p` subprocess call in milliseconds.
/// Default 5000 ms. Cap at 60_000 to avoid pathological hangs.
pub fn llm_rerank_timeout_ms() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    *V.get_or_init(|| env_usize("IRONMEM_LLM_RERANK_TIMEOUT_MS", 5000).min(60_000) as u64)
}

/// LLM rerank backend. `"cli"` (default) uses the local `claude` CLI and the
/// user's subscription auth — free per call but ~1-3s of subprocess startup.
/// `"api"` POSTs directly to `api.anthropic.com/v1/messages` — faster but
/// bills the API key. Any other value falls back to `"cli"`.
///
/// Process-lifetime constant after first read. OnceLock-cached because no
/// current test flips this per-call. Switch to per-call read pattern if future
/// tests need to exercise both backends in the same test binary.
pub fn llm_rerank_backend() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        std::env::var("IRONMEM_LLM_RERANK_BACKEND").unwrap_or_else(|_| "cli".to_string())
    })
    .as_str()
}

/// `max_tokens` for the Anthropic Messages API call. Default 8 — matches
/// mempalace's pinned value. The pick-one prompt asks for a bare integer,
/// and at temperature=0 Haiku emits one directly without preamble. Bumping
/// this only helps if the model is allowed to ramble first (it isn't, here).
/// Ignored by the CLI backend (the CLI does not expose this knob).
pub fn llm_rerank_max_tokens() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| env_usize("IRONMEM_LLM_RERANK_MAX_TOKENS", 8) as u32)
}

/// Resolve the Anthropic API key for any in-process Anthropic Messages API
/// call (rerank or pref-extract). Order:
///   1. `ANTHROPIC_API_KEY` (the standard convention).
///   2. `IRONMEM_ANTHROPIC_API_KEY` (scoped fallback for users who want to
///      keep the standard var unset so their `claude` CLI keeps using
///      subscription auth).
///
/// Returns `None` if neither is set; the caller is responsible for hard-failing
/// when `IRONMEM_LLM_RERANK_BACKEND=api` AND the key is missing.
pub fn anthropic_api_key() -> Option<String> {
    static V: OnceLock<Option<String>> = OnceLock::new();
    V.get_or_init(|| {
        std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .or_else(|| std::env::var("IRONMEM_ANTHROPIC_API_KEY").ok())
            .filter(|s| !s.is_empty())
    })
    .clone()
}

// ── E5: preference enrichment (off by default) ───────────────────────────────

/// `IRONMEM_PREF_ENRICH=1` enables the synthetic-preference-doc enrichment
/// at ingest time and the search-pipeline collapse step that hides the
/// synthetic from results. Default OFF; the LongMemEval bench flips it on
/// to measure the recall lift on `single-session-preference` questions.
pub fn pref_enrich_enabled() -> bool {
    // Not OnceLock-cached: the integration tests need to flip it per-test.
    // Runtime cost is one env-var read per add_drawer / search call, which is
    // negligible vs an embed or HNSW probe.
    matches!(
        std::env::var("IRONMEM_PREF_ENRICH").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// Selects which `PreferenceExtractor` implementation `build_synthetic` uses.
/// `regex` (default): the V4 regex set ported from mempalace. Free, fast, but
/// only catches first-person fragments. `llm`: a `claude -p` subprocess that
/// summarizes the conversation in question-vocabulary form. Per-ingest LLM
/// cost; rich enough to bridge the vocabulary gap that regex misses.
///
/// Process-lifetime constant after first read. Unlike `pref_enrich_enabled`,
/// this is OnceLock-cached because no current test flips it per-call. If
/// future integration tests need to flip extractor types per-test, switch
/// to the per-call read pattern.
pub fn pref_extractor() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        std::env::var("IRONMEM_PREF_EXTRACTOR").unwrap_or_else(|_| "regex".to_string())
    })
    .as_str()
}

/// Model alias for the LLM preference extractor. Default `"claude-haiku-4-5"`.
///
/// Process-lifetime constant after first read. OnceLock-cached because no
/// current test flips this per-call. Switch to per-call read pattern if future
/// tests need different model aliases per-test.
pub fn pref_llm_model() -> String {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        std::env::var("IRONMEM_PREF_LLM_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".to_string())
    })
    .clone()
}

/// Wall-clock timeout for one preference-extractor LLM call, in milliseconds.
/// Default 15_000 ms (extraction is a longer prompt than rerank, so we allow
/// a bigger budget). Capped at 60_000 ms.
pub fn pref_llm_timeout_ms() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    *V.get_or_init(|| env_usize("IRONMEM_PREF_LLM_TIMEOUT_MS", 15_000).min(60_000) as u64)
}

/// Transport for the LLM preference extractor. `cli` (default) shells out to
/// `claude -p`; `api` POSTs directly to `api.anthropic.com/v1/messages`. The
/// API path avoids the heavy claude-code subprocess fan-out (~13s overhead
/// per call) at the cost of a billable API key.
///
/// Process-lifetime constant after first read. OnceLock-cached because no
/// current test flips this per-call. Switch to per-call read pattern if future
/// tests need to exercise both backends in the same test binary.
pub fn pref_llm_backend() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        std::env::var("IRONMEM_PREF_LLM_BACKEND").unwrap_or_else(|_| "cli".to_string())
    })
    .as_str()
}

/// `max_tokens` for the API backend. Default 200 — enough for the
/// 1-2 sentence summary prompt with margin. Ignored by the CLI backend.
pub fn pref_llm_max_tokens() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| env_usize("IRONMEM_PREF_LLM_MAX_TOKENS", 200) as u32)
}

// ── shrinkage matcher mode ──────────────────────────────────────────────────

/// `IRONMEM_SHRINKAGE_WORD_BOUNDARY=0` reverts the shrinkage rerank's
/// keyword/name matcher to legacy substring behavior. Default ON.
///
/// The legacy path (`String::contains`) causes false-positive boosts: a
/// predicate keyword like "suggest" substring-matches drawer text
/// "suggestions"; "current" matches "currently". Word-boundary matching
/// with a small set of English suffix tolerances (s|es|ed|ing|ion|ions)
/// fixes the substring confusion without losing inflected-form recall.
///
/// Not OnceLock-cached: the integration tests need to flip this per-test.
pub fn shrinkage_word_boundary_enabled() -> bool {
    !matches!(
        std::env::var("IRONMEM_SHRINKAGE_WORD_BOUNDARY").as_deref(),
        Ok("0") | Ok("false")
    )
}

// ── metrics tunables (§5/§8) ────────────────────────────────────────────────
// Unlike the search knobs above, these read the env FRESH on every call (no
// OnceLock). They gate per-response / per-hook metrics writes that tests flip
// via process-global env vars; a cached first-read would make IRONMEM_METRICS
// and IRONMEM_CONTEXT_WINDOW untestable in a shared test binary. Call frequency
// is ≤ once per MCP response / hook event, so a fresh env read is cheap.

/// Context-window size used as the occupancy denominator (METRICS_SPEC §8.1).
/// Default 200000; non-positive or unparseable overrides fall back to default.
pub fn context_window() -> i64 {
    match std::env::var("IRONMEM_CONTEXT_WINDOW")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
    {
        Some(v) if v > 0 => v,
        _ => 200_000,
    }
}

/// Global metrics kill switch (METRICS_SPEC §8.3). Default enabled; `0`,
/// `false`, `no`, `off` disable.
pub fn metrics_enabled() -> bool {
    !matches!(
        std::env::var("IRONMEM_METRICS").as_deref(),
        Ok("0" | "false" | "no" | "off")
    )
}

/// Serializes tests that mutate the `IRONMEM_KG_*` env vars. These tunables
/// read the env fresh on every call, so any test flipping them (here or in
/// `search::pipeline`) must hold this lock to avoid clobbering a concurrent
/// test reading the default.
#[cfg(test)]
pub(crate) static KG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    // Env vars are process-global; serialize against the crate-wide metrics
    // env lock so tests in other modules that flip `IRONMEM_METRICS` (e.g.
    // `mcp::server`) cannot clobber these.
    use crate::metrics::METRICS_ENV_LOCK;

    #[test]
    fn context_window_defaults_to_200000() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_CONTEXT_WINDOW");
        assert_eq!(context_window(), 200_000);
    }

    #[test]
    fn context_window_reads_override() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("IRONMEM_CONTEXT_WINDOW", "1000000");
        assert_eq!(context_window(), 1_000_000);
        std::env::remove_var("IRONMEM_CONTEXT_WINDOW");
    }

    #[test]
    fn context_window_rejects_nonpositive_and_garbage() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("IRONMEM_CONTEXT_WINDOW", "0");
        assert_eq!(context_window(), 200_000);
        std::env::set_var("IRONMEM_CONTEXT_WINDOW", "-5");
        assert_eq!(context_window(), 200_000);
        std::env::set_var("IRONMEM_CONTEXT_WINDOW", "abc");
        assert_eq!(context_window(), 200_000);
        std::env::remove_var("IRONMEM_CONTEXT_WINDOW");
    }

    #[test]
    fn kg_query_limit_defaults_to_50_and_reads_override() {
        let _g = KG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_KG_QUERY_LIMIT");
        assert_eq!(kg_query_limit(), 50);
        std::env::set_var("IRONMEM_KG_QUERY_LIMIT", "7");
        assert_eq!(kg_query_limit(), 7);
        std::env::remove_var("IRONMEM_KG_QUERY_LIMIT");
    }

    #[test]
    fn kg_boost_fanout_defaults_to_32_and_reads_override() {
        let _g = KG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_KG_BOOST_FANOUT");
        assert_eq!(kg_boost_fanout(), 32);
        std::env::set_var("IRONMEM_KG_BOOST_FANOUT", "4");
        assert_eq!(kg_boost_fanout(), 4);
        std::env::remove_var("IRONMEM_KG_BOOST_FANOUT");
    }

    #[test]
    fn kg_caps_treat_zero_as_default() {
        let _g = KG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A `0` override must not silently disable the feature each cap bounds.
        std::env::set_var("IRONMEM_KG_QUERY_LIMIT", "0");
        std::env::set_var("IRONMEM_KG_BOOST_FANOUT", "0");
        assert_eq!(kg_query_limit(), 50);
        assert_eq!(kg_boost_fanout(), 32);
        std::env::remove_var("IRONMEM_KG_QUERY_LIMIT");
        std::env::remove_var("IRONMEM_KG_BOOST_FANOUT");
    }

    #[test]
    fn metrics_enabled_defaults_true_and_honors_kill_switch() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        assert!(metrics_enabled());
        for off in ["0", "false", "no", "off"] {
            std::env::set_var("IRONMEM_METRICS", off);
            assert!(!metrics_enabled(), "{off} should disable");
        }
        std::env::set_var("IRONMEM_METRICS", "1");
        assert!(metrics_enabled());
        std::env::remove_var("IRONMEM_METRICS");
    }
}

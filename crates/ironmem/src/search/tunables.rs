//! Runtime-overridable search tuning knobs.
//!
//! Every constant here reads an `IRONMEM_*` environment variable on first use
//! (via `OnceLock`). When the variable is absent or unparseable the compile-time
//! default is used, so production behaviour is unchanged with no env overrides.
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

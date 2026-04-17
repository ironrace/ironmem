//! Search pipeline: sanitize → embed → HNSW (multi-query) → BM25 → RRF → KG boost → shrinkage rerank → rank.
//!
//! Hybrid retrieval strategy:
//!   1. Embed the cleaned query (content-word variant only for long queries)
//!   2. Run HNSW ANN search for each query variant, union the ranked lists
//!   3. Run BM25 full-text search via SQLite FTS5 (porter-stemmed tokens)
//!   4. Merge HNSW and BM25 via weighted RRF (BM25 down-weighted when sparse)
//!   5. Apply KG-entity score boosts
//!   6. Apply lexical shrinkage rerank (mempalace hybrid-v5 port)
//!   7. Sort deterministically, truncate to requested limit

use std::collections::HashMap;

use crate::db::{knowledge_graph::KnowledgeGraph, ScoredDrawer, SearchFilters};
use crate::error::MemoryError;
use crate::mcp::app::App;

use super::rerank::{extract_signals, shrinkage_rerank, RerankSignals};
use super::sanitizer::{extract_content_words, sanitize_query, SanitizeResult};

/// Maximum HNSW candidates to overfetch before re-ranking.
const MAX_OVERFETCH: usize = 150;

/// RRF k constant. 60 is the widely-accepted default (Cormack et al. 2009).
const RRF_K: f32 = 60.0;

/// Queries with this many tokens or fewer skip the content-word variant.
/// The variant strips question words (when/where/who) which loses question-type
/// signal; on short LoCoMo-style questions it adds noise without benefit.
const CONTENT_WORD_VARIANT_MIN_TOKENS: usize = 13;

/// BM25 hit count below which we down-weight BM25's RRF contribution.
/// With fewer than this many hits the lexical results are likely noisy.
const BM25_SPARSE_THRESHOLD: usize = 5;

/// Full search result including sanitizer metadata.
pub struct SearchResult {
    pub results: Vec<ScoredDrawer>,
    pub sanitizer_info: SanitizeResult,
    pub total_candidates: usize,
    pub rerank_signals: RerankSignals,
    pub bm25_hit_count: usize,
    pub content_word_variant_fired: bool,
}

/// Execute the full hybrid search pipeline.
pub fn search(
    app: &App,
    query: &str,
    filters: &SearchFilters,
) -> Result<SearchResult, MemoryError> {
    let limit = if filters.limit == 0 {
        10
    } else {
        filters.limit
    };

    // Step 0: Ensure index is up-to-date (lazy rebuild after writes)
    app.ensure_index_fresh()?;

    // Step 1: Always-on query sanitization
    let sanitized = sanitize_query(query);

    if sanitized.clean_query.is_empty() {
        return Ok(SearchResult {
            results: Vec::new(),
            sanitizer_info: sanitized,
            total_candidates: 0,
            rerank_signals: RerankSignals::default(),
            bm25_hit_count: 0,
            content_word_variant_fired: false,
        });
    }

    // Step 2: Embed primary query; content-word variant only for long queries.
    // Short queries (≤ CONTENT_WORD_VARIANT_MIN_TOKENS tokens) skip the variant
    // because stripping question words (when/where/who) loses question-type signal.
    let token_count = sanitized.clean_query.split_whitespace().count();
    let use_content_variant = token_count > CONTENT_WORD_VARIANT_MIN_TOKENS;

    let (primary_vec, maybe_content_vec) = {
        let mut emb = app
            .embedder
            .write()
            .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;

        let primary = emb
            .embed_one(&sanitized.clean_query)
            .map_err(MemoryError::Embed)?;

        let content = if use_content_variant {
            extract_content_words(&sanitized.clean_query)
                .map(|cw| emb.embed_one(&cw).map_err(MemoryError::Embed))
                .transpose()?
        } else {
            None
        };

        (primary, content)
    };

    let content_word_variant_fired = maybe_content_vec.is_some();

    // Step 3: HNSW search — overfetch at 5× limit, clamped to MAX_OVERFETCH.
    let overfetch = limit.saturating_mul(5).clamp(30, MAX_OVERFETCH);

    let state = app
        .index_state
        .read()
        .map_err(|e| MemoryError::Lock(format!("IndexState lock poisoned: {e}")))?;

    let primary_hnsw = state.index.search(&primary_vec, overfetch);
    let total_candidates = primary_hnsw.len();

    let hnsw_results = if let Some(cv) = maybe_content_vec {
        let content_hnsw = state.index.search(&cv, overfetch);
        union_hnsw(primary_hnsw, content_hnsw, overfetch)
    } else {
        primary_hnsw
    };

    let hnsw_ids: Vec<String> = hnsw_results
        .iter()
        .filter_map(|(idx, _)| state.id_map.get(*idx).cloned())
        .collect();

    drop(state);

    // Step 4: BM25 full-text search.
    let bm25_pairs = app.db.bm25_search(
        &sanitized.clean_query,
        overfetch,
        filters.wing.as_deref(),
        filters.room.as_deref(),
    )?;
    let bm25_hit_count = bm25_pairs.len();
    let bm25_ids: Vec<String> = bm25_pairs.into_iter().map(|(id, _)| id).collect();

    // Step 5: Weighted RRF — down-weight BM25 when results are sparse to keep
    // HNSW authoritative rather than letting a few noisy BM25 hits dominate.
    let bm25_weight = if bm25_ids.is_empty() {
        0.0
    } else if bm25_ids.len() < BM25_SPARSE_THRESHOLD {
        bm25_ids.len() as f32 / BM25_SPARSE_THRESHOLD as f32
    } else {
        1.0
    };

    let merged_ids = if bm25_weight == 0.0 {
        hnsw_ids.clone()
    } else {
        rrf_merge_weighted(&hnsw_ids, &bm25_ids, RRF_K, bm25_weight)
    };

    // Step 6: Fetch drawer metadata with filters.
    let candidate_id_refs: Vec<&str> = merged_ids.iter().map(|s| s.as_str()).collect();
    let drawers = app.db.get_drawers_by_ids_filtered(
        &candidate_id_refs,
        filters.wing.as_deref(),
        filters.room.as_deref(),
    )?;

    let rrf_scores = rrf_scores_map_weighted(&hnsw_ids, &bm25_ids, RRF_K, bm25_weight);
    let mut scored: Vec<ScoredDrawer> = merged_ids
        .iter()
        .filter_map(|id| {
            drawers.get(id).map(|drawer| {
                let score = rrf_scores.get(id.as_str()).copied().unwrap_or(0.0);
                ScoredDrawer {
                    drawer: drawer.clone(),
                    score,
                }
            })
        })
        .collect();

    // Step 7: KG score boosts (inert when entities table is empty)
    let kg = KnowledgeGraph::new(&app.db);
    kg_boost(&mut scored, &sanitized.clean_query, &kg)?;

    // Step 8: Lexical shrinkage rerank (mempalace hybrid-v5 port)
    let rerank_signals = extract_signals(&sanitized.clean_query);
    shrinkage_rerank(&mut scored, &rerank_signals);

    // Step 9: Deterministic sort — score desc, then drawer_id asc as tiebreak
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.drawer.id.cmp(&b.drawer.id))
    });
    scored.truncate(limit);

    tracing::debug!(
        bm25_hit_count,
        content_word_variant_fired,
        bm25_weight,
        names = ?rerank_signals.names,
        predicate_kws = ?rerank_signals.predicate_kws,
        quoted_phrases = ?rerank_signals.quoted_phrases,
        "search_pipeline_telemetry"
    );

    Ok(SearchResult {
        results: scored,
        sanitizer_info: sanitized,
        total_candidates,
        rerank_signals,
        bm25_hit_count,
        content_word_variant_fired,
    })
}

/// Union two HNSW result lists by max score, deduplicating by index position.
/// Merged list is sorted by score desc, then index asc (deterministic), capped at `cap`.
fn union_hnsw(
    primary: Vec<(usize, f32)>,
    secondary: Vec<(usize, f32)>,
    cap: usize,
) -> Vec<(usize, f32)> {
    let mut seen: HashMap<usize, f32> = HashMap::with_capacity(primary.len() + secondary.len());
    for (idx, score) in primary.iter().chain(secondary.iter()) {
        seen.entry(*idx)
            .and_modify(|s| *s = s.max(*score))
            .or_insert(*score);
    }
    let mut merged: Vec<(usize, f32)> = seen.into_iter().collect();
    merged.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    merged.truncate(cap);
    merged
}

/// Weighted RRF merge. `bm25_weight ∈ [0, 1]` scales list_b's contribution.
fn rrf_merge_weighted(
    list_a: &[String],
    list_b: &[String],
    k: f32,
    bm25_weight: f32,
) -> Vec<String> {
    let scores = rrf_scores_map_weighted(list_a, list_b, k, bm25_weight);
    let mut ranked: Vec<(&str, f32)> = scores.iter().map(|(id, &s)| (*id, s)).collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });
    ranked.into_iter().map(|(id, _)| id.to_string()).collect()
}

/// Compute weighted RRF scores. list_a (HNSW) always has weight 1.0;
/// list_b (BM25) contribution is scaled by `bm25_weight`.
fn rrf_scores_map_weighted<'a>(
    list_a: &'a [String],
    list_b: &'a [String],
    k: f32,
    bm25_weight: f32,
) -> HashMap<&'a str, f32> {
    let mut scores: HashMap<&str, f32> = HashMap::new();
    for (rank, id) in list_a.iter().enumerate() {
        *scores.entry(id.as_str()).or_default() += 1.0 / (k + rank as f32 + 1.0);
    }
    for (rank, id) in list_b.iter().enumerate() {
        *scores.entry(id.as_str()).or_default() += bm25_weight * (1.0 / (k + rank as f32 + 1.0));
    }
    scores
}

/// Boost search scores using knowledge graph entity relationships.
///
/// 1. Find entity mentions in the query
/// 2. For each mentioned entity, get 1-hop related entities
/// 3. Boost results that mention these entities
fn kg_boost(
    candidates: &mut [ScoredDrawer],
    query: &str,
    kg: &KnowledgeGraph,
) -> Result<(), MemoryError> {
    use std::collections::HashSet;

    let mentioned = kg.find_entities_in_text(query)?;

    if mentioned.is_empty() {
        return Ok(());
    }

    // Collect related entity names (1-hop)
    let mut related_names: HashSet<String> = HashSet::new();
    let mut direct_names: HashSet<String> = HashSet::new();

    for entity in &mentioned {
        direct_names.insert(entity.name.to_lowercase());

        if let Ok(triples) = kg.query_entity_current(&entity.id) {
            for triple in triples {
                if let Ok(Some(e)) = kg.get_entity(&triple.subject) {
                    related_names.insert(e.name.to_lowercase());
                }
                if let Ok(Some(e)) = kg.get_entity(&triple.object) {
                    related_names.insert(e.name.to_lowercase());
                }
            }
        }
    }

    // Remove direct names from related (avoid double-boosting)
    for name in &direct_names {
        related_names.remove(name);
    }

    // Apply boosts
    for candidate in candidates.iter_mut() {
        let content_lower = candidate.drawer.content.to_lowercase();

        for name in &direct_names {
            if content_lower.contains(name) {
                candidate.score *= 1.15; // 15% boost for direct entity match
            }
        }

        for name in &related_names {
            if content_lower.contains(name) {
                candidate.score *= 1.05; // 5% boost for related entity
            }
        }
    }

    Ok(())
}

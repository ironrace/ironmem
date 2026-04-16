//! Search pipeline: sanitize → embed → HNSW (multi-query) → BM25 → RRF → KG boost → rank.
//!
//! Hybrid retrieval strategy:
//!   1. Embed the cleaned query (and optionally a content-word variant)
//!   2. Run HNSW ANN search for each query variant, union the ranked lists
//!   3. Run BM25 full-text search via SQLite FTS5 (exact keyword matching)
//!   4. Merge HNSW and BM25 ranked lists via Reciprocal Rank Fusion (RRF)
//!   5. Apply KG-entity score boosts to re-ranked candidates
//!   6. Truncate to requested limit

use std::collections::HashMap;

use crate::db::{knowledge_graph::KnowledgeGraph, ScoredDrawer, SearchFilters};
use crate::error::MemoryError;
use crate::mcp::app::App;

use super::sanitizer::{extract_content_words, sanitize_query, SanitizeResult};

/// Maximum HNSW candidates to overfetch before re-ranking.
/// Higher cap gives BM25 merge more material to work with.
const MAX_OVERFETCH: usize = 150;

/// RRF k constant. 60 is the widely-accepted default (Cormack et al. 2009).
const RRF_K: f32 = 60.0;

/// Full search result including sanitizer metadata.
pub struct SearchResult {
    pub results: Vec<ScoredDrawer>,
    pub sanitizer_info: SanitizeResult,
    pub total_candidates: usize,
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
        });
    }

    // Step 2: Embed primary query and optional content-word variant
    let (primary_vec, maybe_content_vec) = {
        let mut emb = app
            .embedder
            .write()
            .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;

        let primary = emb
            .embed_one(&sanitized.clean_query)
            .map_err(MemoryError::Embed)?;

        // Content-word variant: embed stop-word-filtered query to emphasise
        // proper nouns and domain terms. Only if meaningfully different.
        let content = extract_content_words(&sanitized.clean_query)
            .map(|cw| emb.embed_one(&cw).map_err(MemoryError::Embed))
            .transpose()?;

        (primary, content)
    };

    // Step 3: HNSW search — primary query, plus content-word variant if available.
    // Fetch at least 5× the requested limit, clamped to MAX_OVERFETCH.
    let overfetch = limit.saturating_mul(5).clamp(30, MAX_OVERFETCH);

    let state = app
        .index_state
        .read()
        .map_err(|e| MemoryError::Lock(format!("IndexState lock poisoned: {e}")))?;

    let primary_hnsw = state.index.search(&primary_vec, overfetch);
    let total_candidates = primary_hnsw.len();

    // Union with content-word variant results (deduped by index position).
    let hnsw_results = if let Some(cv) = maybe_content_vec {
        let content_hnsw = state.index.search(&cv, overfetch);
        union_hnsw(primary_hnsw, content_hnsw, overfetch)
    } else {
        primary_hnsw
    };

    // Map HNSW index positions → drawer IDs (preserving rank order).
    let hnsw_ids: Vec<String> = hnsw_results
        .iter()
        .filter_map(|(idx, _)| state.id_map.get(*idx).cloned())
        .collect();

    drop(state); // release read lock before DB I/O

    // Step 4: BM25 full-text search via SQLite FTS5 (graceful fallback on error).
    let bm25_pairs = app.db.bm25_search(
        &sanitized.clean_query,
        overfetch,
        filters.wing.as_deref(),
        filters.room.as_deref(),
    )?;
    let bm25_ids: Vec<String> = bm25_pairs.into_iter().map(|(id, _)| id).collect();

    // Step 5: Reciprocal Rank Fusion — merge HNSW and BM25 ranked lists.
    let merged_ids = if bm25_ids.is_empty() {
        // No FTS results (table not yet populated or no matches) — fall back to HNSW.
        hnsw_ids.clone()
    } else {
        rrf_merge(&hnsw_ids, &bm25_ids, RRF_K)
    };

    // Step 6: Fetch drawer metadata, apply metadata filters.
    let candidate_id_refs: Vec<&str> = merged_ids.iter().map(|s| s.as_str()).collect();
    let drawers = app.db.get_drawers_by_ids_filtered(
        &candidate_id_refs,
        filters.wing.as_deref(),
        filters.room.as_deref(),
    )?;

    // Build ScoredDrawers with RRF score.
    let rrf_scores = rrf_scores_map(&hnsw_ids, &bm25_ids, RRF_K);
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

    // Step 7: KG score adjustment from entity relationships
    let kg = KnowledgeGraph::new(&app.db);
    kg_boost(&mut scored, &sanitized.clean_query, &kg)?;

    // Step 8: Re-sort by boosted score and truncate
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit);

    Ok(SearchResult {
        results: scored,
        sanitizer_info: sanitized,
        total_candidates,
    })
}

/// Union two HNSW result lists by score, deduplicating by index position.
/// Primary results are preferred on tie; the merged list is re-sorted by score
/// and capped at `cap`.
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
    merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    merged.truncate(cap);
    merged
}

/// Merge two ranked lists using Reciprocal Rank Fusion.
/// Returns a new ranking where items from both lists that share the same ID
/// receive additive RRF scores.
fn rrf_merge(list_a: &[String], list_b: &[String], k: f32) -> Vec<String> {
    let scores = rrf_scores_map(list_a, list_b, k);
    let mut ranked: Vec<(&str, f32)> = scores.iter().map(|(id, &s)| (*id, s)).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.into_iter().map(|(id, _)| id.to_string()).collect()
}

/// Compute RRF scores for all items across both ranked lists.
fn rrf_scores_map<'a>(list_a: &'a [String], list_b: &'a [String], k: f32) -> HashMap<&'a str, f32> {
    let mut scores: HashMap<&str, f32> = HashMap::new();
    for (rank, id) in list_a.iter().enumerate() {
        *scores.entry(id.as_str()).or_default() += 1.0 / (k + rank as f32 + 1.0);
    }
    for (rank, id) in list_b.iter().enumerate() {
        *scores.entry(id.as_str()).or_default() += 1.0 / (k + rank as f32 + 1.0);
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

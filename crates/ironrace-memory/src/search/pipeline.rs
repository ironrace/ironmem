//! Search pipeline: sanitize → embed → HNSW → metadata filter → KG boost → rank.
//!
//! Knowledge-graph relationships can adjust retrieval ranking after vector search.

use std::collections::HashSet;

use crate::db::{knowledge_graph::KnowledgeGraph, ScoredDrawer, SearchFilters};
use crate::error::MemoryError;
use crate::mcp::app::App;

use super::sanitizer::{sanitize_query, SanitizeResult};

const MAX_OVERFETCH: usize = 75;

/// Full search result including sanitizer metadata.
pub struct SearchResult {
    pub results: Vec<ScoredDrawer>,
    pub sanitizer_info: SanitizeResult,
    pub total_candidates: usize,
}

/// Execute the full search pipeline.
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

    // Step 2: Embed the clean query
    let query_vec = {
        let mut emb = app
            .embedder
            .write()
            .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;
        emb.embed_one(&sanitized.clean_query)
            .map_err(MemoryError::Embed)?
    };

    // Step 3+4: HNSW search + id_map lookup under a single read lock (no TOCTOU)
    // Fetch at least 5x the requested limit, with a floor of 30 candidates
    // so short-content documents (where needle is diluted by other context)
    // aren't dropped before the relevance re-rank step.
    let overfetch = limit.saturating_mul(5).clamp(30, MAX_OVERFETCH);
    let state = app
        .index_state
        .read()
        .map_err(|e| MemoryError::Lock(format!("IndexState lock poisoned: {e}")))?;

    let hnsw_results = state.index.search(&query_vec, overfetch);
    let total_candidates = hnsw_results.len();

    let candidate_ids: Vec<&str> = hnsw_results
        .iter()
        .filter_map(|(idx, _)| state.id_map.get(*idx).map(|id| id.as_str()))
        .collect();

    let drawers = app.db.get_drawers_by_ids_filtered(
        &candidate_ids,
        filters.wing.as_deref(),
        filters.room.as_deref(),
    )?;

    // Build scored results with metadata filtering
    let mut scored = Vec::new();
    for (idx, score) in &hnsw_results {
        if let Some(id) = state.id_map.get(*idx) {
            if let Some(drawer) = drawers.get(id) {
                scored.push(ScoredDrawer {
                    drawer: drawer.clone(),
                    score: *score,
                });
            }
        }
    }

    // Step 5: KG score adjustment from entity relationships
    let kg = KnowledgeGraph::new(&app.db);
    kg_boost(&mut scored, &sanitized.clean_query, &kg)?;

    // Step 6: Re-sort by boosted score and truncate
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
                // Get names for related entities
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

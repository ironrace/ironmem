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
use super::tunables;

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
    let use_content_variant = token_count > tunables::content_word_variant_min_tokens();

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
    let overfetch = limit.saturating_mul(5).clamp(30, tunables::max_overfetch());

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
    let sparse_threshold = tunables::bm25_sparse_threshold();
    let bm25_weight = if bm25_ids.is_empty() {
        0.0
    } else if bm25_ids.len() < sparse_threshold {
        bm25_ids.len() as f32 / sparse_threshold as f32
    } else {
        1.0
    };

    let rrf_k = tunables::rrf_k();
    let mut merged_ids = if bm25_weight == 0.0 {
        hnsw_ids.clone()
    } else {
        rrf_merge_weighted(&hnsw_ids, &bm25_ids, rrf_k, bm25_weight)
    };

    // Step 5b: Pseudo-relevance feedback (E3) — expand the query with discriminative
    // terms from the top-K initial candidates, then re-search and 4-way RRF merge.
    // Gated on IRONMEM_PRF_ENABLED (default off). prf_top_k defaults to 15 so that
    // preference-question gold sessions (E1: ranks 6-22) appear in the foreground.
    let mut prf_expanded_hnsw_ids: Option<Vec<String>> = None;
    let mut prf_expanded_bm25_ids: Option<Vec<String>> = None;

    if tunables::prf_enabled()
        && merged_ids.len() >= tunables::prf_top_k()
        && merged_ids.len() >= tunables::prf_min_corpus()
    {
        let fg_k = tunables::prf_top_k();
        let n_terms = tunables::prf_terms();

        // Fetch foreground (top-K) content for term extraction.
        let fg_refs: Vec<&str> = merged_ids[..fg_k].iter().map(|s| s.as_str()).collect();
        let bg_refs: Vec<&str> = merged_ids[fg_k..].iter().map(|s| s.as_str()).collect();
        let fg_drawers = app.db.get_drawers_by_ids(&fg_refs)?;
        let bg_drawers = if bg_refs.is_empty() {
            std::collections::HashMap::new()
        } else {
            app.db.get_drawers_by_ids(&bg_refs)?
        };

        // Build query term set to exclude from expansion.
        let query_lower = sanitized.clean_query.to_lowercase();
        let query_terms: std::collections::HashSet<String> = super::rerank::KW_RE
            .find_iter(&query_lower)
            .map(|m| m.as_str().to_string())
            .filter(|w| !super::rerank::KW_STOP.contains(w.as_str()))
            .collect();

        // fg_tf: term → number of foreground docs containing it.
        let fg_size = fg_drawers.len();
        let bg_size = bg_drawers.len();
        let mut fg_tf: HashMap<String, usize> = HashMap::new();
        for drawer in fg_drawers.values() {
            let doc_lower = drawer.content.to_lowercase();
            let doc_terms: std::collections::HashSet<String> = super::rerank::KW_RE
                .find_iter(&doc_lower)
                .map(|m| m.as_str().to_string())
                .filter(|w| !super::rerank::KW_STOP.contains(w.as_str()))
                .collect();
            for term in doc_terms {
                if !query_terms.contains(&term) {
                    *fg_tf.entry(term).or_default() += 1;
                }
            }
        }

        // bg_df: term → number of background docs containing it.
        let mut bg_df: HashMap<String, usize> = HashMap::new();
        for drawer in bg_drawers.values() {
            let doc_lower = drawer.content.to_lowercase();
            let doc_terms: std::collections::HashSet<String> = super::rerank::KW_RE
                .find_iter(&doc_lower)
                .map(|m| m.as_str().to_string())
                .filter(|w| !super::rerank::KW_STOP.contains(w.as_str()))
                .collect();
            for term in doc_terms {
                *bg_df.entry(term).or_default() += 1;
            }
        }

        // Score: TF-ICF — frequent in foreground, rare in background.
        // Require term in ≥2 foreground docs to suppress single-doc noise.
        let mut term_scores: Vec<(String, f32)> = fg_tf
            .into_iter()
            .filter(|(_, count)| *count >= 2)
            .map(|(term, fg_count)| {
                let bg_count = bg_df.get(&term).copied().unwrap_or(0);
                let icf = ((bg_size as f32 + 1.0) / (bg_count as f32 + 1.0))
                    .ln()
                    .max(0.0);
                let score = (fg_count as f32 / fg_size as f32) * icf;
                (term, score)
            })
            .collect();
        term_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let expansion: Vec<String> = term_scores
            .into_iter()
            .take(n_terms)
            .map(|(t, _)| t)
            .collect();

        if !expansion.is_empty() {
            let expanded_query = format!("{} {}", sanitized.clean_query, expansion.join(" "));
            tracing::debug!(expansion = ?expansion, "prf_query_expansion");

            // Re-embed and re-search with expanded query.
            let expanded_vec = {
                let mut emb = app
                    .embedder
                    .write()
                    .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;
                emb.embed_one(&expanded_query).map_err(MemoryError::Embed)?
            };

            let state = app
                .index_state
                .read()
                .map_err(|e| MemoryError::Lock(format!("IndexState lock poisoned: {e}")))?;
            let exp_hnsw_raw = state.index.search(&expanded_vec, overfetch);
            let exp_hnsw_ids: Vec<String> = exp_hnsw_raw
                .iter()
                .filter_map(|(idx, _)| state.id_map.get(*idx).cloned())
                .collect();
            drop(state);

            let exp_bm25_pairs = app.db.bm25_search(
                &expanded_query,
                overfetch,
                filters.wing.as_deref(),
                filters.room.as_deref(),
            )?;
            let exp_bm25_ids: Vec<String> = exp_bm25_pairs.into_iter().map(|(id, _)| id).collect();

            // 4-way RRF: original lists weight 1.0, expanded lists weight 0.5.
            let lists: [&Vec<String>; 4] = [&hnsw_ids, &bm25_ids, &exp_hnsw_ids, &exp_bm25_ids];
            let weights: [f32; 4] = [1.0, bm25_weight, 0.5, 0.5 * bm25_weight];
            merged_ids = rrf_merge_nway(&lists, &weights, rrf_k);
            prf_expanded_hnsw_ids = Some(exp_hnsw_ids);
            prf_expanded_bm25_ids = Some(exp_bm25_ids);
        }
    }

    // Step 6: Fetch drawer metadata with filters.
    let candidate_id_refs: Vec<&str> = merged_ids.iter().map(|s| s.as_str()).collect();
    let drawers = app.db.get_drawers_by_ids_filtered(
        &candidate_id_refs,
        filters.wing.as_deref(),
        filters.room.as_deref(),
    )?;

    // Compute RRF scores — 4-way if PRF fired, 2-way otherwise.
    let rrf_scores = match (&prf_expanded_hnsw_ids, &prf_expanded_bm25_ids) {
        (Some(exp_hnsw), Some(exp_bm25)) => {
            let lists: [&Vec<String>; 4] = [&hnsw_ids, &bm25_ids, exp_hnsw, exp_bm25];
            let weights: [f32; 4] = [1.0, bm25_weight, 0.5, 0.5 * bm25_weight];
            rrf_scores_map_nway(&lists, &weights, rrf_k)
        }
        _ => rrf_scores_map_weighted(&hnsw_ids, &bm25_ids, rrf_k, bm25_weight),
    };
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

/// N-way weighted RRF merge. Each list has a corresponding weight.
fn rrf_merge_nway(lists: &[&Vec<String>], weights: &[f32], k: f32) -> Vec<String> {
    let scores = rrf_scores_map_nway(lists, weights, k);
    let mut ranked: Vec<(&str, f32)> = scores.iter().map(|(id, &s)| (*id, s)).collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });
    ranked.into_iter().map(|(id, _)| id.to_string()).collect()
}

/// Compute N-way weighted RRF scores from parallel lists and weights.
fn rrf_scores_map_nway<'a>(
    lists: &[&'a Vec<String>],
    weights: &[f32],
    k: f32,
) -> HashMap<&'a str, f32> {
    let mut scores: HashMap<&str, f32> = HashMap::new();
    for (list, &weight) in lists.iter().zip(weights.iter()) {
        for (rank, id) in list.iter().enumerate() {
            *scores.entry(id.as_str()).or_default() += weight / (k + rank as f32 + 1.0);
        }
    }
    scores
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

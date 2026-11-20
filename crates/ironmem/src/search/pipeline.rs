//! Search pipeline: sanitize → embed → vector search (multi-query) → BM25 → RRF → KG boost
//!     → collapse synthetic preference siblings → shrinkage rerank → LLM rerank → rank.
//!
//! Hybrid retrieval strategy:
//!   1. Embed the cleaned query (content-word variant only for long queries)
//!   2. Run HNSW ANN search for each query variant (an exact scan of the
//!      scope instead for a wing/room-scoped search), union the ranked lists
//!   3. Run BM25 full-text search via SQLite FTS5 (porter-stemmed tokens)
//!   4. Merge vector and BM25 results via weighted RRF (BM25 down-weighted when sparse)
//!   5. Apply KG-entity score boosts
//!   6. Apply lexical shrinkage rerank (mempalace hybrid-v5 port)
//!   7. Sort deterministically, truncate to requested limit

use std::collections::HashMap;

use crate::db::{
    drawers::{Drawer, PREF_SENTINEL},
    knowledge_graph::{fold_mention, folded_text_mentions, EntityName, KnowledgeGraph},
    ScoredDrawer, SearchFilters,
};
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
///
/// The legs read SQLite in separate statements, with no read transaction
/// around them, so a write another process commits mid-search can be seen by
/// one leg and not another; the final filtered fetch decides which rows are
/// returned. (A read transaction would have to span the embedder call and the
/// optional LLM rerank.)
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

    // Step 0: the embedder must be ready before anything is embedded. The
    // HNSW index is rebuilt after writes only where it is read (the unscoped
    // vector leg, `vector_candidates`): a scoped search never reads it.
    app.ensure_embedder_ready()?;

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

    // Step 3: vector search (HNSW, or an exact scan of a wing/room scope) —
    // overfetch at 5× limit, clamped to MAX_OVERFETCH.
    let overfetch = limit.saturating_mul(5).clamp(30, tunables::max_overfetch());

    // A scoped search that may run PRF keeps the rows its scan scored, so
    // the PRF re-query (step 5b) ranks them without reading the scope again.
    let prf_on = tunables::prf_enabled() || app.force_prf;
    let (vector_ids, scope_kept) = if is_scoped(filters) {
        scoped_scan(
            app,
            &primary_vec,
            maybe_content_vec.as_deref(),
            overfetch,
            filters,
            prf_on,
        )?
    } else {
        let ids = vector_candidates(
            app,
            &primary_vec,
            maybe_content_vec.as_deref(),
            overfetch,
            filters,
        )?;
        (ids, None)
    };
    let total_candidates = vector_ids.len();

    // Step 4: BM25 full-text search.
    let bm25_pairs = app.db.bm25_search_with_history(
        &sanitized.clean_query,
        overfetch,
        filters.wing.as_deref(),
        filters.room.as_deref(),
        filters.include_superseded,
    )?;
    let bm25_hit_count = bm25_pairs.len();
    let bm25_ids: Vec<String> = bm25_pairs.into_iter().map(|(id, _)| id).collect();

    // Step 5: Weighted RRF — down-weight BM25 when results are sparse to keep
    // the vector leg authoritative rather than letting a few noisy BM25 hits dominate.
    let bm25_weight = sparse_bm25_weight(bm25_ids.len(), tunables::bm25_sparse_threshold());

    let rrf_k = tunables::rrf_k();
    let mut merged_ids = if bm25_weight == 0.0 {
        vector_ids.clone()
    } else {
        rrf_merge_weighted(&vector_ids, &bm25_ids, rrf_k, bm25_weight)
    };

    // Step 5b: Pseudo-relevance feedback (E3) — expand the query with discriminative
    // terms from the top-K initial candidates, then re-search and 4-way RRF merge.
    // Gated on IRONMEM_PRF_ENABLED (default off; `force_prf` is the test-only
    // seam, as `force_rerank` is for step 9). prf_top_k defaults to 15 so that
    // preference-question gold sessions (E1: ranks 6-22) appear in the foreground.
    let mut prf_expanded_vector_ids: Option<Vec<String>> = None;
    let mut prf_expanded_bm25_ids: Option<Vec<String>> = None;
    let mut prf_drawer_cache: HashMap<String, Drawer> = HashMap::new();

    if prf_on
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
        prf_drawer_cache.extend(
            fg_drawers
                .iter()
                .map(|(id, drawer)| (id.clone(), drawer.clone())),
        );
        prf_drawer_cache.extend(
            bg_drawers
                .iter()
                .map(|(id, drawer)| (id.clone(), drawer.clone())),
        );

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

            let exp_vector_ids = match &scope_kept {
                Some(kept) => rank_kept_candidates(kept, &expanded_vec, overfetch, filters),
                None => vector_candidates(app, &expanded_vec, None, overfetch, filters)?,
            };

            let exp_bm25_pairs = app.db.bm25_search_with_history(
                &expanded_query,
                overfetch,
                filters.wing.as_deref(),
                filters.room.as_deref(),
                filters.include_superseded,
            )?;
            let exp_bm25_ids: Vec<String> = exp_bm25_pairs.into_iter().map(|(id, _)| id).collect();

            // 4-way RRF: original lists weight 1.0, expanded lists weight 0.5.
            let lists: [&Vec<String>; 4] = [&vector_ids, &bm25_ids, &exp_vector_ids, &exp_bm25_ids];
            let weights: [f32; 4] = [1.0, bm25_weight, 0.5, 0.5 * bm25_weight];
            merged_ids = rrf_merge_nway(&lists, &weights, rrf_k);
            prf_expanded_vector_ids = Some(exp_vector_ids);
            prf_expanded_bm25_ids = Some(exp_bm25_ids);
        }
    }

    // Step 6: Fetch drawer metadata with filters. When PRF fetched foreground /
    // background drawers, carry those forward to avoid repeating the same lookup.
    // PRF fetched the cached drawers by id with no filter, so they are checked
    // against `drawer_matches_filters` here; the uncached ids are filtered in
    // SQL by wing, room and supersession.
    let mut drawers: HashMap<String, Drawer> = HashMap::new();
    let mut missing_candidate_ids: Vec<&str> = Vec::new();
    for id in &merged_ids {
        match prf_drawer_cache.get(id) {
            Some(drawer) if drawer_matches_filters(drawer, filters) => {
                drawers.insert(id.clone(), drawer.clone());
            }
            _ => missing_candidate_ids.push(id.as_str()),
        }
    }
    let fetched = app.db.get_drawers_by_ids_filtered(
        &missing_candidate_ids,
        filters.wing.as_deref(),
        filters.room.as_deref(),
        filters.include_superseded,
    )?;
    drawers.extend(fetched);

    // Compute RRF scores — 4-way if PRF fired, 2-way otherwise.
    let rrf_scores = match (&prf_expanded_vector_ids, &prf_expanded_bm25_ids) {
        (Some(exp_vector), Some(exp_bm25)) => {
            let lists: [&Vec<String>; 4] = [&vector_ids, &bm25_ids, exp_vector, exp_bm25];
            let weights: [f32; 4] = [1.0, bm25_weight, 0.5, 0.5 * bm25_weight];
            rrf_scores_map_nway(&lists, &weights, rrf_k)
        }
        _ => rrf_scores_map_weighted(&vector_ids, &bm25_ids, rrf_k, bm25_weight),
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

    // Step 7.5: collapse synthetic preference siblings into their parent rows.
    // Cheap when no synthetic hit is in `scored` (single partition pass + early
    // return). Always-on by structural check; the only way a synth hit reaches
    // here is if pref-enrichment was enabled at ingest time.
    collapse_synthetic_into_parents(app, &mut scored, filters)?;

    // Step 8: Lexical shrinkage rerank (mempalace hybrid-v5 port)
    // Default ON; disable with IRONMEM_SHRINKAGE_RERANK=0 for eval comparisons.
    let rerank_signals = extract_signals(&sanitized.clean_query);
    if tunables::shrinkage_rerank_enabled() {
        shrinkage_rerank(&mut scored, &rerank_signals);
    }

    // Step 9: Optional LLM rerank. `force_rerank` is a test-only seam that runs
    // the stage even when the OnceLock-cached `IRONMEM_RERANK` gate is unset.
    if tunables::rerank_enabled() || app.force_rerank {
        app.ensure_reranker_loaded();
        if let Some(scorer) = app.reranker.read().unwrap().clone() {
            let llm_response = crate::search::llm_rerank::cross_encoder_rerank(
                &scorer,
                &sanitized.clean_query,
                &mut scored,
            );
            if let Some(resp) = llm_response {
                let ctx = crate::metrics::MetricsContext::resolve(app, None);
                let row = crate::db::metrics::new_token_usage_from_llm(
                    "llm_rerank",
                    &resp,
                    chrono::Utc::now().to_rfc3339(),
                )
                .with_context(&ctx);
                if let Err(e) = app.db.insert_token_usage(&row) {
                    tracing::warn!(
                        error = %e,
                        source = %row.source,
                        model = ?row.model,
                        "llm_rerank token_usage insert failed"
                    );
                }
            }
        }
    }

    // Step 10: Deterministic sort — score desc, then drawer_id asc as tiebreak.
    //
    // When the rerank stage ran, skip the score-based resort. Reranker scores
    // (e.g. LLM logits or cross-encoder logits) are not commensurable with
    // upstream shrinkage/RRF scores (~[0, 1]); a mixed sort would let the
    // un-reranked tail's positive shrinkage scores float above rerank-promoted
    // items. `llm_rerank::cross_encoder_rerank` already sorts [..rerank_top_k]
    // correctly, and the tail [rerank_top_k..] retains its pre-rerank order,
    // so the current ordering is already what we want.
    if !(tunables::rerank_enabled() || app.force_rerank) {
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.drawer.id.cmp(&b.drawer.id))
        });
    }
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

/// True when the caller restricted the search to a wing or room.
fn is_scoped(filters: &SearchFilters) -> bool {
    filters.wing.is_some() || filters.room.is_some()
}

/// Vector candidates for one query (plus the optional content-word variant),
/// as drawer ids in rank order.
///
/// Unscoped searches take the top `overfetch` across the HNSW index (rebuilt
/// first if a write left it stale) and drop superseded rows. A wing/room-scoped search does not use the index: it
/// scores every drawer in the scope exactly (`scoped_vector_candidates`). The
/// index has no scope filter, and its global top-k can hold none of the
/// scope's drawers, so a search of the index would return nothing for a
/// scope whose drawers exist.
fn vector_candidates(
    app: &App,
    primary: &[f32],
    content: Option<&[f32]>,
    overfetch: usize,
    filters: &SearchFilters,
) -> Result<Vec<String>, MemoryError> {
    if is_scoped(filters) {
        return scoped_vector_candidates(app, primary, content, overfetch, filters);
    }
    // Lazy rebuild after writes, here where the index is read.
    app.ensure_index_fresh()?;
    let ids = search_index_ids(app, primary, content, overfetch)?;
    filter_superseded_candidate_ids(app, ids, filters.include_superseded)
}

/// Search the index for the top `k` of the primary vector (unioned with the
/// content-word variant when present). Holds the `index_state` read lock only
/// for the search and the id mapping.
fn search_index_ids(
    app: &App,
    primary: &[f32],
    content: Option<&[f32]>,
    k: usize,
) -> Result<Vec<String>, MemoryError> {
    let state = app
        .index_state
        .read()
        .map_err(|e| MemoryError::Lock(format!("IndexState lock poisoned: {e}")))?;

    let primary_hnsw = state.index.search(primary, k);
    let hnsw_results = match content {
        Some(cv) => union_hnsw(primary_hnsw, state.index.search(cv, k), k),
        None => primary_hnsw,
    };
    Ok(hnsw_results
        .iter()
        .filter_map(|(idx, _)| state.id_map.get(*idx).cloned())
        .collect())
}

/// Vector candidates for a wing/room-scoped search, scored exactly.
///
/// One `SELECT` (after the column probe `scope_vectors` makes, as the other
/// drawer reads do) streams the scope's drawers, in no particular order; the
/// SQL already leaves out collab messages and, unless history is asked for,
/// superseded rows. A drawer is kept when `row_matches_filters` keeps it (the
/// same rules as the final filter) and its embedding decodes, has the query's
/// length and has a non-zero, finite norm. Each row is scored by cosine
/// similarity with the query and with the content-word variant in the same
/// pass, and each query vector holds only its best `overfetch` rows
/// ([`TopK`]); the two rankings are merged with `union_hnsw`, as the unscoped
/// path merges them. Returns at most `overfetch` ids, highest score first,
/// ties broken by drawer id: exactly what scoring every row and sorting them
/// all would return.
///
/// The cost is linear in the scope and is paid on every scoped search: every
/// row the SQL keeps is read, decoded and scored, with no cap, cache or index
/// (an 18k-drawer room is ~56MB of blob reads). Memory stays at `overfetch`
/// rows per query vector, plus one decoded row at a time. Indexing the scope
/// instead is not exact: an HNSW search widened to reach the scope can still
/// miss drawers its graph does not connect.
///
/// An all-zero embedding (Autopilot and collab rows) is skipped silently: it
/// is deliberate and has no direction to score. Every other skipped row (an
/// undecodable blob, another length than the query, a norm that is not
/// usable, a cosine that is not finite) is malformed; they are counted and
/// reported in one `warn` that names the scope.
fn scoped_vector_candidates(
    app: &App,
    primary: &[f32],
    content: Option<&[f32]>,
    overfetch: usize,
    filters: &SearchFilters,
) -> Result<Vec<String>, MemoryError> {
    scoped_scan(app, primary, content, overfetch, filters, false).map(|(ids, _)| ids)
}

/// The scan behind [`scoped_vector_candidates`]. With `keep`, it also returns
/// every row it decoded and could classify, so the PRF re-query can rank the
/// scope with [`rank_kept_candidates`] instead of reading it again; that holds
/// the scope's decoded vectors in memory until the search ends, which is why
/// only a search that may run PRF asks for it. The kept rows are `None` when
/// no query vector could be scored and the scope was not read.
fn scoped_scan(
    app: &App,
    primary: &[f32],
    content: Option<&[f32]>,
    overfetch: usize,
    filters: &SearchFilters,
    keep: bool,
) -> Result<(Vec<String>, Option<Vec<ScopeCandidate>>), MemoryError> {
    use crate::db::drawers::EmbeddingNorm;

    let mut queries: Vec<ScopedQuery<'_>> = std::iter::once(primary)
        .chain(content)
        .filter_map(|query| ScopedQuery::new(query, overfetch))
        .collect();
    if queries.is_empty() {
        return Ok((Vec::new(), None));
    }

    let mut malformed = 0usize;
    let mut kept = keep.then(Vec::new);
    app.db.scope_vectors(
        filters.wing.as_deref(),
        filters.room.as_deref(),
        filters.include_superseded,
        |row| {
            if !row_matches_filters(&row.wing, &row.room, row.superseded_by.is_some(), filters) {
                return;
            }
            let Some(embedding) = row.embedding() else {
                malformed += 1;
                return;
            };
            if embedding.len() != primary.len() {
                malformed += 1;
                return;
            }
            let norm = match crate::db::drawers::embedding_norm(&embedding) {
                EmbeddingNorm::Usable(norm) => norm,
                EmbeddingNorm::Zero => return,
                EmbeddingNorm::Unusable => {
                    malformed += 1;
                    return;
                }
            };
            // A row either query cannot score counts once, however many
            // queries drop it. (`&=` offers the row to every query.)
            let mut scorable = true;
            for query in &mut queries {
                scorable &= query.offer(&row.id, &embedding, norm);
            }
            if !scorable {
                malformed += 1;
            }
            if let Some(kept) = kept.as_mut() {
                kept.push(ScopeCandidate {
                    id: row.id,
                    embedding,
                    norm,
                });
            }
        },
    )?;

    let mut rankings = queries.into_iter().map(ScopedQuery::finish);
    let first = rankings.next().unwrap_or_default();
    let top = match rankings.next() {
        Some(second) => union_hnsw(first, second, overfetch),
        None => first,
    };
    warn_malformed(malformed, filters);
    Ok((top.into_iter().map(|(id, _)| id).collect(), kept))
}

/// Rank the rows a scoped scan kept (see [`scoped_scan`]) against `query`,
/// PRF's expanded query, as the scan itself would rank them, without reading
/// the scope again.
fn rank_kept_candidates(
    candidates: &[ScopeCandidate],
    query: &[f32],
    overfetch: usize,
    filters: &SearchFilters,
) -> Vec<String> {
    let Some(scoring) = query_scoring(query) else {
        return Vec::new();
    };
    let mut unscorable = vec![false; candidates.len()];
    let top = exact_top_k(candidates, query, scoring, overfetch, &mut unscorable);
    warn_malformed(
        unscorable.iter().filter(|dropped| **dropped).count(),
        filters,
    );
    top.into_iter().map(|(id, _)| id).collect()
}

/// The scoped leg's one warning for the rows it skipped as malformed.
fn warn_malformed(malformed: usize, filters: &SearchFilters) {
    if malformed > 0 {
        tracing::warn!(
            malformed,
            wing = filters.wing.as_deref(),
            room = filters.room.as_deref(),
            "scoped search skipped drawers whose stored embedding is malformed"
        );
    }
}

/// An in-scope drawer the scoped vector leg can score: its id, its decoded
/// embedding and that embedding's (non-zero, finite) norm.
struct ScopeCandidate {
    id: String,
    embedding: Vec<f32>,
    norm: f32,
}

/// How the scoped leg scores rows against one query vector, decided once
/// per query by [`query_scoring`].
#[derive(Debug, Clone, Copy, PartialEq)]
enum QueryScoring {
    /// Cosine similarity, with the query's (finite, non-zero) norm.
    Cosine(f32),
    /// An all-zero query (the noop embedder's): every row scores 0, so the
    /// id tie-break alone decides. Issue #430 tracks giving it no vector
    /// candidates instead.
    AllZero,
}

/// Classify `query` by its norm with the rule that classifies stored rows
/// (`crate::db::drawers::embedding_norm`). A query with no usable norm that
/// is not all-zero (a NaN or infinite component, or a norm that overflows or
/// underflows) has no direction to rank by: it gets `None`, after a warning,
/// and so no vector candidates.
fn query_scoring(query: &[f32]) -> Option<QueryScoring> {
    use crate::db::drawers::EmbeddingNorm;

    match crate::db::drawers::embedding_norm(query) {
        EmbeddingNorm::Usable(norm) => Some(QueryScoring::Cosine(norm)),
        EmbeddingNorm::Zero => Some(QueryScoring::AllZero),
        EmbeddingNorm::Unusable => {
            tracing::warn!(
                "query embedding has no usable norm (a non-finite value, or a norm that \
                 overflows or underflows); its vector candidates are skipped"
            );
            None
        }
    }
}

/// One query vector's ranking of the scope: each row is offered once, and
/// only the best `k` are held.
struct ScopedQuery<'q> {
    vector: &'q [f32],
    scoring: QueryScoring,
    top: TopK,
}

impl<'q> ScopedQuery<'q> {
    /// `None` for a query [`query_scoring`] gives no scoring.
    fn new(vector: &'q [f32], k: usize) -> Option<Self> {
        query_scoring(vector).map(|scoring| Self {
            vector,
            scoring,
            top: TopK::new(k),
        })
    }

    /// Score one row and rank it. `false` when its score is not finite, so it
    /// is not ranked. A row of another length than the query is not scored
    /// and is not counted here: the scan has already dropped, and counted,
    /// rows of another length than the primary query. An all-zero query
    /// ([`QueryScoring::AllZero`]) scores every row 0, so ids decide.
    fn offer(&mut self, id: &str, embedding: &[f32], norm: f32) -> bool {
        if embedding.len() != self.vector.len() {
            return true;
        }
        let score = match self.scoring {
            QueryScoring::Cosine(query_norm) => {
                match crate::db::drawers::cosine(self.vector, query_norm, embedding, norm) {
                    Some(score) => score,
                    None => return false,
                }
            }
            QueryScoring::AllZero => 0.0,
        };
        self.top.offer(id, score);
        true
    }

    fn finish(self) -> Vec<(String, f32)> {
        self.top.into_sorted()
    }
}

/// The top `k` of `candidates` by `scoring` against `query`, as (id, score),
/// highest first, ties broken by drawer id whatever order `candidates` is
/// in, ranked as [`ScopedQuery::offer`] ranks a streamed row. A candidate
/// whose score is not finite is skipped and marked in `unscorable` (indexed
/// by position, the same length as `candidates`); marks are only ever set,
/// so one slice can collect them over several queries.
fn exact_top_k(
    candidates: &[ScopeCandidate],
    query: &[f32],
    scoring: QueryScoring,
    k: usize,
    unscorable: &mut [bool],
) -> Vec<(String, f32)> {
    let mut ranking = ScopedQuery {
        vector: query,
        scoring,
        top: TopK::new(k),
    };
    for (pos, candidate) in candidates.iter().enumerate() {
        if !ranking.offer(&candidate.id, &candidate.embedding, candidate.norm) {
            unscorable[pos] = true;
        }
    }
    ranking.finish()
}

/// The best `k` (id, score) pairs offered, by score descending (`total_cmp`)
/// then id ascending: what sorting every pair that way and cutting at `k`
/// keeps, ties included, whatever order they arrive in. Holds at most `k`
/// pairs, and copies an id only when its pair is kept.
struct TopK {
    k: usize,
    /// Worst kept pair on top (see [`Ranked`]'s order), so it is the one
    /// evicted.
    heap: std::collections::BinaryHeap<Ranked>,
}

impl TopK {
    fn new(k: usize) -> Self {
        Self {
            k,
            heap: std::collections::BinaryHeap::new(),
        }
    }

    fn offer(&mut self, id: &str, score: f32) {
        if self.k == 0 {
            return;
        }
        if self.heap.len() == self.k {
            let worst = self.heap.peek().expect("a full heap of k > 0 pairs");
            let better = match score.total_cmp(&worst.score) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Equal => id < worst.id.as_str(),
                std::cmp::Ordering::Less => false,
            };
            if !better {
                return;
            }
            self.heap.pop();
        }
        self.heap.push(Ranked {
            score,
            id: id.to_string(),
        });
    }

    /// The kept pairs, best first.
    fn into_sorted(self) -> Vec<(String, f32)> {
        self.heap
            .into_sorted_vec()
            .into_iter()
            .map(|ranked| (ranked.id, ranked.score))
            .collect()
    }
}

/// A [`TopK`] entry, ordered worst-last: a lower score is greater, and among
/// equal scores a later id is. The heap's maximum is then the pair to evict,
/// and its ascending order is best first.
struct Ranked {
    score: f32,
    id: String,
}

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for Ranked {}

fn drawer_matches_filters(drawer: &Drawer, filters: &SearchFilters) -> bool {
    row_matches_filters(
        &drawer.wing,
        &drawer.room,
        drawer.superseded_by.is_some(),
        filters,
    )
}

/// The rules `drawer_matches_filters` applies, for a row given by its
/// columns: a collab message never matches, a superseded row matches only
/// when `include_superseded` is set, and a set wing or room must equal the
/// row's. The scoped vector leg, which has no `Drawer` to hand, calls this
/// too, so the two cannot drift apart.
fn row_matches_filters(wing: &str, room: &str, superseded: bool, filters: &SearchFilters) -> bool {
    room != crate::db::drawers::COLLAB_MESSAGE_ROOM
        && (filters.include_superseded || !superseded)
        && filters.wing.as_deref().is_none_or(|w| wing == w)
        && filters.room.as_deref().is_none_or(|r| room == r)
}

/// Drop retained historical rows before they can influence RRF or PRF. This
/// deliberately leaves scope and collab-message filtering to the existing
/// final stage, preserving their established semantics.
fn filter_superseded_candidate_ids(
    app: &App,
    candidate_ids: Vec<String>,
    include_superseded: bool,
) -> Result<Vec<String>, MemoryError> {
    if include_superseded || candidate_ids.is_empty() {
        return Ok(candidate_ids);
    }

    let refs: Vec<&str> = candidate_ids.iter().map(String::as_str).collect();
    let current = app
        .db
        .get_drawers_by_ids_filtered(&refs, None, None, false)?;
    Ok(candidate_ids
        .into_iter()
        .filter(|id| current.contains_key(id))
        .collect())
}

/// Union two vector result lists by max score, deduplicating by key: an
/// HNSW index position, or the scoped exact scan's drawer id.
/// Merged list is sorted by score desc, then key asc (deterministic), capped at `cap`.
fn union_hnsw<K: Clone + Eq + std::hash::Hash + Ord>(
    primary: Vec<(K, f32)>,
    secondary: Vec<(K, f32)>,
    cap: usize,
) -> Vec<(K, f32)> {
    let mut seen: HashMap<K, f32> = HashMap::with_capacity(primary.len() + secondary.len());
    for (key, score) in primary.into_iter().chain(secondary) {
        seen.entry(key)
            .and_modify(|s| *s = s.max(score))
            .or_insert(score);
    }
    let mut merged: Vec<(K, f32)> = seen.into_iter().collect();
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

/// BM25's RRF weight, down-scaled when the result set is sparse (fewer than
/// `sparse_threshold` hits) so a handful of noisy lexical matches can't
/// dominate a stronger dense/vector signal; at or above the threshold BM25
/// gets full weight. Shared by the main search pipeline and the prompt-hook's
/// hybrid vector fusion — both merge a dense id list with a BM25 id list via
/// [`rrf_merge_weighted`] and must down-weight sparse BM25 identically.
pub(crate) fn sparse_bm25_weight(bm25_id_count: usize, sparse_threshold: usize) -> f32 {
    if bm25_id_count == 0 {
        0.0
    } else if bm25_id_count < sparse_threshold {
        bm25_id_count as f32 / sparse_threshold as f32
    } else {
        1.0
    }
}

/// Weighted RRF merge. `bm25_weight ∈ [0, 1]` scales list_b's contribution.
pub(crate) fn rrf_merge_weighted(
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

/// Compute weighted RRF scores. list_a (vector) always has weight 1.0;
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

/// Step 7.5: collapse synthetic preference siblings into their parent rows.
///
/// A synthetic drawer carries `source_file = "pref:<parent_drawer_id>"`. After
/// scoring, we want the parent to absorb the synth's score (if higher) and the
/// synth to disappear from the candidate list. If the parent is missing from
/// `candidates` (because it didn't make the vector top-N) but the synth did, fetch
/// the parent by id and surface it with the synth's score; drop the synth.
/// The fetch honors `include_superseded`, so a surviving synthetic sibling
/// cannot restore an otherwise-hidden historical parent during normal search.
/// If the parent has been deleted from the DB, drop the synth quietly.
///
/// This runs *before* the rerank stages so all downstream scoring sees only
/// real drawers and so RRF/KG scores remain commensurable.
pub fn collapse_synthetic_into_parents(
    app: &App,
    candidates: &mut Vec<ScoredDrawer>,
    filters: &SearchFilters,
) -> Result<(), MemoryError> {
    // Partition: (synth, real). Both keep insertion order to keep the ordering
    // step downstream deterministic.
    let mut synths: Vec<ScoredDrawer> = Vec::new();
    let mut reals: Vec<ScoredDrawer> = Vec::with_capacity(candidates.len());
    for sd in candidates.drain(..) {
        if sd.drawer.source_file.starts_with(PREF_SENTINEL) {
            synths.push(sd);
        } else {
            reals.push(sd);
        }
    }

    if synths.is_empty() {
        *candidates = reals;
        return Ok(());
    }

    // Build a parent_id → real-index map for O(1) lookup.
    let mut by_id: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, r) in reals.iter().enumerate() {
        by_id.insert(r.drawer.id.clone(), i);
    }

    // First pass: promote scores for parents already in `reals`. Defer
    // orphan-parent fetches to a single batched DB call.
    let mut orphan_scores: std::collections::HashMap<String, f32> =
        std::collections::HashMap::new();

    for s in synths {
        let parent_id = &s.drawer.source_file[PREF_SENTINEL.len()..];
        if let Some(&idx) = by_id.get(parent_id) {
            if s.score > reals[idx].score {
                reals[idx].score = s.score;
            }
        } else {
            // Track the highest synth score per orphan parent.
            // HashMap.entry guarantees uniqueness — no per-element dedup needed.
            let cur = orphan_scores
                .entry(parent_id.to_string())
                .or_insert(s.score);
            if s.score > *cur {
                *cur = s.score;
            }
        }
    }

    // Derive the orphan id list from the HashMap keys (already unique).
    let orphan_parent_ids: Vec<String> = orphan_scores.keys().cloned().collect();

    if !orphan_parent_ids.is_empty() {
        let id_refs: Vec<&str> = orphan_parent_ids.iter().map(|s| s.as_str()).collect();
        let fetched =
            app.db
                .get_drawers_by_ids_filtered(&id_refs, None, None, filters.include_superseded)?;
        for pid in &orphan_parent_ids {
            if let Some(parent) = fetched.get(pid) {
                let score = orphan_scores.get(pid).copied().unwrap_or(0.0);
                reals.push(ScoredDrawer {
                    drawer: parent.clone(),
                    score,
                });
            }
            // else: parent deleted between index and query — drop quietly.
        }
    }

    *candidates = reals;
    Ok(())
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

    // Collect related entity names (1-hop), bounded by the fan-out cap so a
    // high-degree hub can't trigger unbounded entity lookups. The cap counts
    // genuine related entities (the mentioned entity itself is skipped).
    let query_limit = tunables::kg_query_limit();
    let fanout_cap = tunables::kg_boost_fanout();
    // Names are held folded (`fold_mention`), the form the drawer text is
    // compared in below, so two spellings of one name boost once.
    let mut related_names: HashSet<String> = HashSet::new();
    let mut direct_names: HashSet<String> = HashSet::new();
    let mut skipped = 0usize;
    let mut first_skipped: Option<String> = None;

    'outer: for entity in &mentioned {
        direct_names.insert(fold_mention(&entity.name));

        let triples = kg.query_entity_current(&entity.id, query_limit)?;
        for triple in triples {
            for related_id in [&triple.subject, &triple.object] {
                match kg.entity_name(related_id)? {
                    Some(EntityName::Text(name)) => {
                        let name = fold_mention(&name);
                        if !direct_names.contains(&name) {
                            related_names.insert(name);
                        }
                    }
                    // A row whose name is not text (a BLOB an outside import
                    // wrote) has no name to look for. It is skipped; failing
                    // here would fail every search that reaches it.
                    Some(EntityName::NotText) => {
                        skipped += 1;
                        first_skipped.get_or_insert_with(|| related_id.clone());
                    }
                    None => {}
                }
            }
            if related_names.len() >= fanout_cap {
                break 'outer;
            }
        }
    }
    if let Some(id) = first_skipped {
        tracing::warn!(
            "KG boost skipped {skipped} related entity lookup(s) whose name is not text; \
             the first has id {id:?}"
        );
    }

    // Remove direct names from related (avoid double-boosting)
    for name in &direct_names {
        related_names.remove(name);
    }

    // Apply boosts. `folded_text_mentions` decides whether a drawer mentions
    // a name: one with punctuation in it only as a whole word (a query naming
    // "c++" is no reason to boost a drawer about "abc++"), a plain one as a
    // substring, as before #378.
    for candidate in candidates.iter_mut() {
        let content = fold_mention(&candidate.drawer.content);

        for name in &direct_names {
            if folded_text_mentions(&content, name) {
                candidate.score *= 1.15; // 15% boost for direct entity match
            }
        }

        for name in &related_names {
            if folded_text_mentions(&content, name) {
                candidate.score *= 1.05; // 5% boost for related entity
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::worktree::testing::with_captured_log;
    use crate::db::drawers::Drawer;
    use crate::db::schema::Database;
    use crate::search::tunables::KG_ENV_LOCK;

    fn candidate(id: &str, content: &str) -> ScoredDrawer {
        ScoredDrawer {
            drawer: Drawer {
                id: id.to_string(),
                content: content.to_string(),
                wing: "w".into(),
                room: "r".into(),
                source_file: "f".into(),
                added_by: "t".into(),
                filed_at: "2026-01-01T00:00:00Z".into(),
                date: "2026-01-01".into(),
                superseded_by: None,
            },
            score: 1.0,
        }
    }

    #[test]
    fn kg_boost_caps_related_entity_fanout() {
        let _g = KG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_KG_QUERY_LIMIT");
        std::env::set_var("IRONMEM_KG_BOOST_FANOUT", "8");

        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        // High-degree hub: 20 distinct related entities, each its own triple.
        for i in 0..20 {
            kg.add_triple(
                "Hub",
                "person",
                "knows",
                &format!("friend{i:02}"),
                "person",
                None,
                1.0,
                None,
            )
            .unwrap();
        }

        // One candidate per related entity, each mentioning exactly that friend.
        let mut candidates: Vec<ScoredDrawer> = (0..20)
            .map(|i| candidate(&format!("c{i:02}"), &format!("note about friend{i:02}")))
            .collect();

        kg_boost(&mut candidates, "Hub", &kg).unwrap();

        // Without a cap all 20 related entities would boost their candidate;
        // the fan-out cap of 8 must limit it to exactly 8.
        let boosted = candidates.iter().filter(|c| c.score > 1.0).count();
        assert_eq!(
            boosted, 8,
            "fan-out cap of 8 should boost exactly 8 related entities"
        );

        std::env::remove_var("IRONMEM_KG_BOOST_FANOUT");
    }

    /// The ids of the candidates whose score was raised above the 1.0 they
    /// start at, in candidate order.
    fn boosted(candidates: &[ScoredDrawer]) -> Vec<&str> {
        candidates
            .iter()
            .filter(|c| c.score > 1.0)
            .map(|c| c.drawer.id.as_str())
            .collect()
    }

    /// A name that contains punctuation is matchable since #378, and a drawer
    /// mentions it only as a whole word: "c++" in the query is no reason to
    /// boost a drawer about "abc++". A plain name keeps the substring match it
    /// always had, so an inflected form ("cats") still counts, at the known
    /// cost of also boosting a drawer about a "category".
    #[test]
    fn kg_boost_bounds_a_punctuated_name_and_matches_a_plain_name_as_a_substring() {
        let _g = KG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_KG_QUERY_LIMIT");
        std::env::remove_var("IRONMEM_KG_BOOST_FANOUT");

        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);
        kg.upsert_entity("c++", "language").unwrap();
        kg.upsert_entity("cat", "animal").unwrap();

        let mut candidates = vec![
            candidate("whole-punctuated", "notes on C++ templates"),
            candidate("embedded-punctuated", "abc++ compiles"),
            candidate("whole-plain", "the cat, asleep"),
            candidate("inflected-plain", "two cats asleep"),
            candidate("embedded-plain", "a category of things"),
        ];
        kg_boost(&mut candidates, "c++ and my cat", &kg).unwrap();

        assert_eq!(
            boosted(&candidates),
            [
                "whole-punctuated",
                "whole-plain",
                "inflected-plain",
                "embedded-plain"
            ]
        );
    }

    /// A script written without spaces has no word boundary to find: the
    /// drawer text runs straight on after the name.
    #[test]
    fn kg_boost_boosts_a_drawer_in_a_script_written_without_spaces() {
        let _g = KG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_KG_QUERY_LIMIT");
        std::env::remove_var("IRONMEM_KG_BOOST_FANOUT");

        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);
        kg.upsert_entity("東京", "city").unwrap();

        let mut candidates = vec![
            candidate("tokyo", "東京の天気は晴れ"),
            candidate("osaka", "大阪の天気は雨"),
        ];
        kg_boost(&mut candidates, "東京 の天気", &kg).unwrap();

        assert_eq!(boosted(&candidates), ["tokyo"]);
    }

    /// The query side folds curly quotes and typographic hyphens to ASCII, so
    /// the drawer side must too, in both directions: an ASCII name against a
    /// drawer pasted from a document, and a name stored curly against an
    /// ASCII drawer.
    #[test]
    fn kg_boost_folds_typographic_punctuation_in_the_drawer_and_the_name() {
        let _g = KG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_KG_QUERY_LIMIT");
        std::env::remove_var("IRONMEM_KG_BOOST_FANOUT");

        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);
        kg.upsert_entity("O'Brien", "person").unwrap();
        kg.upsert_entity("D\u{2019}Angelo", "person").unwrap();
        kg.upsert_entity("iron-build", "tool").unwrap();

        let mut candidates = vec![
            candidate("curly-drawer", "met O\u{2019}Brien today"),
            candidate("ascii-drawer", "an album by D'Angelo"),
            candidate("hyphen-drawer", "ran iron\u{2011}build twice"),
            candidate("neither", "nothing to see here"),
        ];
        kg_boost(
            &mut candidates,
            "O\u{2019}Brien, d'angelo and iron-build",
            &kg,
        )
        .unwrap();

        assert_eq!(
            boosted(&candidates),
            ["curly-drawer", "ascii-drawer", "hyphen-drawer"]
        );
    }

    /// A triple can point at an entity row whose name is not text (a BLOB an
    /// outside import wrote). That row is skipped; it must not fail the
    /// search, nor stop the rows after it from boosting.
    #[test]
    fn kg_boost_skips_a_related_entity_whose_name_is_not_text() {
        let _g = KG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_KG_QUERY_LIMIT");
        std::env::remove_var("IRONMEM_KG_BOOST_FANOUT");

        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);
        kg.add_triple("Hub", "thing", "knows", "Friend", "thing", None, 1.0, None)
            .unwrap();
        let hub_id = kg.resolve_entity("Hub", None).unwrap().id;
        // Dated far ahead so it sorts before the Friend triple and is the
        // first one the expansion reads.
        db.exec_raw(&format!(
            "INSERT INTO entities (id, name, entity_type) VALUES ('blob-row', x'636174ff', 'thing');
             INSERT INTO triples (id, subject, predicate, object, confidence, extracted_at)
             VALUES ('blob-triple', '{hub_id}', 'knows', 'blob-row', 1.0, '2999-01-01T00:00:00Z');"
        ))
        .unwrap();

        let mut candidates = vec![
            candidate("direct", "note about hub"),
            candidate("related", "note about friend"),
            candidate("neither", "nothing to see here"),
        ];
        let (result, log) = with_captured_log(|| kg_boost(&mut candidates, "Hub", &kg));
        result.unwrap();

        assert_eq!(boosted(&candidates), ["direct", "related"]);
        // The skip is said once, with the row's id. (The mention scan reads
        // the same row and reports it too, in a warning of its own.)
        let warnings: Vec<&str> = log
            .lines()
            .filter(|l| l.contains("WARN") && l.contains("KG boost skipped 1 "))
            .collect();
        assert_eq!(warnings.len(), 1, "{log}");
        assert!(warnings[0].contains("blob-row"), "{log}");
    }

    /// A name of nothing but whitespace folds to the empty string, which
    /// every drawer contains.
    #[test]
    fn kg_boost_ignores_a_related_entity_with_a_blank_name() {
        let _g = KG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_KG_QUERY_LIMIT");
        std::env::remove_var("IRONMEM_KG_BOOST_FANOUT");

        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);
        kg.add_triple("Hub", "thing", "knows", " \t", "thing", None, 1.0, None)
            .unwrap();

        let mut candidates = vec![candidate("unrelated", "nothing to see here")];
        kg_boost(&mut candidates, "Hub", &kg).unwrap();

        assert_eq!(boosted(&candidates), [] as [&str; 0]);
    }

    #[test]
    fn kg_boost_propagates_db_errors_instead_of_swallowing() {
        let _g = KG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_KG_QUERY_LIMIT");
        std::env::remove_var("IRONMEM_KG_BOOST_FANOUT");

        let db = Database::open_in_memory().unwrap();
        let kg = KnowledgeGraph::new(&db);

        // Register an entity so the query mentions it and the 1-hop expansion
        // runs (it would short-circuit on an empty mention set).
        kg.upsert_entity("Hub", "thing").unwrap();

        // Simulate a DB-level failure: drop the `triples` table, which the
        // first lookup in the expansion (`query_entity_current`) reads from.
        // The `entities` table is left intact so the mention scan still finds
        // "Hub" and execution reaches that triple query.
        db.exec_raw("DROP TABLE triples").unwrap();

        let mut candidates = vec![candidate("c0", "note about hub")];

        // The error from the dropped table must surface at the `?` on
        // `query_entity_current`, not be swallowed. This covers the first of
        // the two propagation sites. The second (`get_entity`, which reads
        // `entities`) cannot be isolated at the table level: `get_entity` and
        // `find_entities_in_text` issue the same `SELECT ... FROM entities`, so
        // any structural break to `entities` fails the mention scan first,
        // before the `get_entity` call is ever reached. Both sites use
        // identical `?` propagation and the symmetric fix is verified by review.
        let result = kg_boost(&mut candidates, "Hub", &kg);
        assert!(
            result.is_err(),
            "kg_boost must propagate the DB error from query_entity_current, got {result:?}"
        );
    }

    const DIM: usize = ironrace_embed::EMBED_DIM;

    /// The query vector: the first axis.
    fn query_vec() -> Vec<f32> {
        let mut v = vec![0.0; DIM];
        v[0] = 1.0;
        v
    }

    /// A unit vector near the query axis. `spread` sets how far it leans off
    /// that axis along a deterministic pseudo-random direction (seeded by
    /// `seed`), so a larger `spread` ranks further from the query.
    fn near_query(seed: u64, spread: f32) -> Vec<f32> {
        let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let mut v = vec![0.0; DIM];
        v[0] = 1.0;
        for slot in v.iter_mut().skip(1).take(16) {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *slot = spread * ((state >> 33) as f32 / (1u64 << 31) as f32 - 0.5);
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / norm).collect()
    }

    fn add(app: &App, id: &str, wing: &str, room: &str, embedding: &[f32]) {
        app.db
            .insert_drawer(
                id,
                &format!("content {id}"),
                embedding,
                wing,
                room,
                "",
                "test",
            )
            .unwrap();
    }

    fn scope(wing: Option<&str>, room: Option<&str>) -> SearchFilters {
        SearchFilters {
            wing: wing.map(str::to_string),
            room: room.map(str::to_string),
            limit: 10,
            include_superseded: false,
        }
    }

    /// 70 drawers in wing "a" all nearer the query than the one in wing "b",
    /// so "b" is outside the global top 30 (the overfetch floor).
    fn app_with_scope_outside_top_k() -> App {
        let app = App::open_for_test().unwrap();
        for i in 0..70 {
            add(&app, &format!("a{i:02}"), "a", "r", &near_query(i, 0.3));
        }
        add(&app, "b00", "b", "rb", &near_query(99, 0.5));
        // Build the index the way a search does after writes, not by single
        // inserts: that graph is the one real searches run against.
        app.mark_dirty();
        app.ensure_index_fresh().unwrap();
        app
    }

    #[test]
    fn scoped_candidates_hold_only_in_scope_ids_capped_at_overfetch() {
        let app = app_with_scope_outside_top_k();
        let q = query_vec();

        let in_a = vector_candidates(&app, &q, None, 30, &scope(Some("a"), None)).unwrap();
        assert_eq!(in_a.len(), 30);
        assert!(in_a.iter().all(|id| id.starts_with('a')));
    }

    /// Cosine similarity of two vectors, computed exactly for test premises.
    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (na * nb)
    }

    /// Wing "a" holds more than `overfetch` drawers nearer the query than the
    /// single drawer in wing "b", so "b00" is outside the global top 30 and a
    /// scoped search of the index could miss it. A scoped search scores the
    /// scope directly, so it returns exactly that drawer on every build.
    #[test]
    fn scoped_candidates_return_the_scope_drawer_outside_the_global_top_k() {
        let app = app_with_scope_outside_top_k();
        let q = query_vec();

        // Premise, computed exactly rather than through the index.
        let b_score = cosine(&near_query(99, 0.5), &q);
        let nearer = (0..70)
            .filter(|i| cosine(&near_query(*i, 0.3), &q) > b_score)
            .count();
        assert!(nearer > 30, "only {nearer} wing-a drawers are nearer");

        let by_wing = vector_candidates(&app, &q, None, 30, &scope(Some("b"), None)).unwrap();
        assert_eq!(by_wing, ["b00"]);
        let by_room = vector_candidates(&app, &q, None, 30, &scope(None, Some("rb"))).unwrap();
        assert_eq!(by_room, ["b00"]);
        let cv = near_query(7, 0.3);
        let with_variant =
            vector_candidates(&app, &q, Some(&cv), 30, &scope(Some("b"), None)).unwrap();
        assert_eq!(with_variant, ["b00"]);
    }

    /// Through `search()` itself. The noop embedder gives a zero query
    /// vector, so every in-scope drawer scores 0 and only the scope decides
    /// what the vector leg hands on. The query matches no drawer's text, so
    /// BM25 contributes nothing.
    #[test]
    fn search_scoped_to_a_wing_returns_its_one_drawer_with_one_candidate() {
        let app = App::open_for_test().unwrap();
        for i in 0..80 {
            add(&app, &format!("a{i:02}"), "a", "r", &near_query(i, 0.3));
        }
        add(&app, "b00", "b", "rb", &near_query(99, 0.5));
        app.mark_dirty();

        let filters = SearchFilters {
            wing: Some("b".to_string()),
            room: None,
            limit: 10,
            include_superseded: false,
        };
        let result = search(&app, "orthogonal", &filters).unwrap();
        let ids: Vec<&str> = result
            .results
            .iter()
            .map(|r| r.drawer.id.as_str())
            .collect();
        assert_eq!(ids, ["b00"]);
        assert_eq!(result.total_candidates, 1);
        assert_eq!(result.bm25_hit_count, 0);
    }

    /// A scoped search never reads the HNSW index, so after a write it does
    /// not pay for a rebuild; the next unscoped search, which reads it, does.
    #[test]
    fn a_scoped_search_leaves_the_index_rebuild_to_the_next_unscoped_search() {
        let app = App::open_for_test().unwrap();
        add(&app, "a00", "a", "r", &at_similarity(0.5, 1.0));
        app.mark_dirty();
        search(&app, "anything", &scope(Some("a"), None)).unwrap();
        assert!(app.index_is_dirty());
        search(&app, "anything", &scope(None, None)).unwrap();
        assert!(!app.index_is_dirty());
    }

    /// A scoped search with PRF on (forced: the env gate is OnceLock-cached).
    /// 110 in-scope drawers match the query by keyword (at least
    /// `prf_min_corpus`, so PRF fires), and so do drawers in another wing.
    /// Every vector is zero, so BM25 drives PRF, and "alpha" is the term it
    /// expands with. Every result stays in scope, PRF re-runs its keyword
    /// leg, and the expanded query is scored against the rows the first scan
    /// kept: the scope's embeddings are read once.
    #[test]
    fn a_scoped_search_with_prf_stays_in_scope_and_reads_the_scope_once() {
        let mut app = App::open_for_test().unwrap();
        let zero = vec![0.0; DIM];
        for (wing, count) in [("p", 110), ("q", 40)] {
            for i in 0..count {
                let id = format!("{wing}{i:03}");
                app.db
                    .insert_drawer(
                        &id,
                        &format!("needle alpha {id}"),
                        &zero,
                        wing,
                        "r",
                        "",
                        "t",
                    )
                    .unwrap();
            }
        }
        let filters = SearchFilters {
            limit: 20,
            ..scope(Some("p"), None)
        };
        let run = |app: &mut App| {
            let reads = app.db.record_column_reads();
            let result = search(app, "needle", &filters).unwrap();
            let reads = reads.lock().unwrap().clone();
            let count = |table: &str, column: &str| {
                reads
                    .iter()
                    .filter(|(t, c)| t == table && c == column)
                    .count()
            };
            (
                result,
                count("drawers", "embedding"),
                count("drawers_fts", "drawers_fts"),
            )
        };

        let (_, plain_embedding_reads, plain_keyword_reads) = run(&mut app);
        app.force_prf = true;
        let (result, embedding_reads, keyword_reads) = run(&mut app);

        assert_eq!(result.results.len(), 20);
        assert!(
            result.results.iter().all(|r| r.drawer.wing == "p"),
            "{:?}",
            result
                .results
                .iter()
                .map(|r| r.drawer.id.as_str())
                .collect::<Vec<_>>()
        );
        // PRF ran: its expanded query took a second keyword search.
        assert!(plain_keyword_reads > 0);
        assert_eq!(keyword_reads, 2 * plain_keyword_reads);
        // And it did not read the scope's embeddings a second time.
        assert_eq!(plain_embedding_reads, 1);
        assert_eq!(embedding_reads, 1);
    }

    /// An Autopilot backlog-lineage row is stored with a zero vector and so is
    /// left out of the vector index, but keyword search still finds it, as
    /// `load_all_vectors`' doc and the CHANGELOG say.
    #[test]
    fn search_finds_a_zero_vector_lineage_row_by_keyword() {
        let app = App::open_for_test().unwrap();
        add(&app, "real", "a", "r", &at_similarity(0.5, 1.0));
        app.db
            .insert_drawer(
                "lineage",
                "superseded by the zanzibar backlog item",
                &vec![0.0; DIM],
                crate::autopilot::WING,
                crate::autopilot::ROOM,
                "",
                "autopilot",
            )
            .unwrap();
        app.mark_dirty();
        let result = search(&app, "zanzibar", &scope(None, None)).unwrap();
        let ids: Vec<&str> = result
            .results
            .iter()
            .map(|r| r.drawer.id.as_str())
            .collect();
        // "real" comes from the vector leg (the noop query matches anything
        // in the index); "lineage" can only come from the keyword leg.
        assert!(ids.contains(&"lineage"), "{ids:?}");
    }

    /// A vector whose cosine similarity with `query_vec()` is exactly `s`,
    /// scaled to length `len` (so the score must divide by the norm).
    fn at_similarity(s: f32, len: f32) -> Vec<f32> {
        let mut v = vec![0.0; DIM];
        v[0] = s * len;
        v[1] = (1.0 - s * s).sqrt() * len;
        v
    }

    #[test]
    fn scoped_candidates_rank_by_similarity_ties_by_id_and_truncate() {
        let app = App::open_for_test().unwrap();
        add(&app, "o0", "o", "r", &at_similarity(0.1, 1.0));
        add(&app, "o2", "o", "r", &at_similarity(0.5, 1.0));
        add(&app, "o1", "o", "r", &at_similarity(0.5, 1.0));
        add(&app, "o3", "o", "r", &at_similarity(0.9, 1.0));
        // Stored at length 3: it ranks by its direction (0.7), not its length.
        add(&app, "o4", "o", "r", &at_similarity(0.7, 3.0));
        let wing_o = scope(Some("o"), None);

        let all = vector_candidates(&app, &query_vec(), None, 30, &wing_o).unwrap();
        assert_eq!(all, ["o3", "o4", "o1", "o2", "o0"]);
        // A query of another length gives the same order.
        let long_query: Vec<f32> = query_vec().iter().map(|x| x * 5.0).collect();
        assert_eq!(
            vector_candidates(&app, &long_query, None, 30, &wing_o).unwrap(),
            all
        );
        // The top `overfetch`, cut after the order is decided.
        assert_eq!(
            vector_candidates(&app, &query_vec(), None, 3, &wing_o).unwrap(),
            ["o3", "o4", "o1"]
        );
        // A zero query (the noop embedder's) scores every drawer 0, so the id
        // tie-break alone decides, and nothing divides by zero.
        let zero = vec![0.0; DIM];
        assert_eq!(
            vector_candidates(&app, &zero, None, 3, &wing_o).unwrap(),
            ["o0", "o1", "o2"]
        );
    }

    /// A query with no usable norm that is not all-zero (a NaN or infinite
    /// component, or a norm that overflows) has no direction to rank by. It
    /// gets no vector candidates and one warning, rather than every row
    /// scoring 0 and the ids deciding. The content-word variant is judged on
    /// its own.
    #[test]
    fn scoped_candidates_skip_a_query_with_no_usable_norm() {
        let app = App::open_for_test().unwrap();
        add(&app, "u0", "u", "r", &at_similarity(0.5, 1.0));
        add(&app, "u1", "u", "r", &at_similarity(0.9, 1.0));
        let wing_u = scope(Some("u"), None);
        let mut nan = query_vec();
        nan[1] = f32::NAN;
        let overflow = vec![1e20; DIM];
        for bad in [nan, overflow] {
            let (ids, log) =
                with_captured_log(|| vector_candidates(&app, &bad, None, 30, &wing_u).unwrap());
            assert!(ids.is_empty(), "{ids:?}");
            let warnings: Vec<&str> = log.lines().filter(|l| l.contains("WARN")).collect();
            assert_eq!(warnings.len(), 1, "{log}");
            assert!(
                warnings[0].contains("query embedding has no usable norm"),
                "{log}"
            );
            let cv = query_vec();
            let (ids, _) = with_captured_log(|| {
                vector_candidates(&app, &bad, Some(&cv), 30, &wing_u).unwrap()
            });
            assert_eq!(ids, ["u1", "u0"]);
        }
    }

    /// Rows reach the scoped leg in whatever order SQLite returns them
    /// (here insertion order, the reverse of id order). Equal scores still
    /// come out in id order, for the primary query and for the union with
    /// the content-word variant.
    #[test]
    fn scoped_candidates_break_ties_by_id_whatever_order_the_rows_arrive_in() {
        let app = App::open_for_test().unwrap();
        let v = at_similarity(0.5, 1.0);
        for id in ["t4", "t3", "t2", "t1", "t0"] {
            add(&app, id, "t", "r", &v);
        }
        let wing_t = scope(Some("t"), None);
        let q = query_vec();
        assert_eq!(
            vector_candidates(&app, &q, None, 30, &wing_t).unwrap(),
            ["t0", "t1", "t2", "t3", "t4"]
        );
        let cv = at_similarity(0.5, 1.0);
        assert_eq!(
            vector_candidates(&app, &q, Some(&cv), 3, &wing_t).unwrap(),
            ["t0", "t1", "t2"]
        );
    }

    /// A score that is not finite is dropped, not ranked: `total_cmp` would
    /// put a NaN or +inf first. Decoded embeddings are finite and their norms
    /// finite and non-zero, so the scoped leg cannot itself produce one; this
    /// feeds the scorer a norm too small for its vector to force one.
    #[test]
    fn exact_top_k_drops_a_non_finite_score() {
        let mut extreme = vec![0.0; DIM];
        extreme[0] = f32::MAX;
        extreme[1] = f32::MAX;
        let mut query = vec![0.0; DIM];
        query[0] = 1.0;
        query[1] = 1.0;
        let candidates = [
            candidate_of("extreme", extreme, 1.0),
            candidate_of("plain", at_similarity(0.5, 1.0), 1.0),
        ];
        let mut unscorable = [false; 2];
        let top = exact_top_k(&candidates, &query, scoring_of(&query), 10, &mut unscorable);
        assert_eq!(top.len(), 1, "{top:?}");
        assert_eq!(top[0].0, "plain");
        // The dropped candidate is reported, so the scoped leg can count it.
        assert_eq!(unscorable, [true, false]);
        // A second query on the same slice drops it again, and it is still
        // marked once: a row no query can score counts once.
        let scaled: Vec<f32> = query.iter().map(|x| x * 2.0).collect();
        let top = exact_top_k(
            &candidates,
            &scaled,
            scoring_of(&scaled),
            10,
            &mut unscorable,
        );
        assert_eq!(top.len(), 1, "{top:?}");
        assert_eq!(unscorable.iter().filter(|dropped| **dropped).count(), 1);
    }

    /// The scoring a usable or all-zero test query gets.
    fn scoring_of(query: &[f32]) -> QueryScoring {
        query_scoring(query).expect("a usable or all-zero query")
    }

    fn candidate_of(id: &str, embedding: Vec<f32>, norm: f32) -> ScopeCandidate {
        ScopeCandidate {
            id: id.to_string(),
            embedding,
            norm,
        }
    }

    /// The bounded ranking keeps exactly what a full sort (score descending,
    /// then id ascending) cut at `k` keeps, ties included, whatever order the
    /// rows arrive in.
    #[test]
    fn top_k_matches_a_full_sort() {
        let mut state = 7u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state >> 33
        };
        for n in [0usize, 1, 5, 64, 300] {
            // Five distinct scores (one negative, one -0.0), so most rows tie.
            let scores = [-0.5, -0.0, 0.0, 0.25, 0.9];
            let mut rows: Vec<(String, f32)> = (0..n)
                .map(|i| (format!("id{i:03}"), scores[(next() % 5) as usize]))
                .collect();
            // Arrival order: a deterministic shuffle.
            for i in (1..rows.len()).rev() {
                let j = (next() % (i as u64 + 1)) as usize;
                rows.swap(i, j);
            }
            let mut sorted = rows.clone();
            sorted.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            for k in [0, 1, 2, 10, n, n + 3] {
                let mut top = TopK::new(k);
                for (id, score) in &rows {
                    top.offer(id, *score);
                }
                let expected: Vec<(String, f32)> = sorted.iter().take(k).cloned().collect();
                assert_eq!(top.into_sorted(), expected, "n={n} k={k}");
            }
        }
    }

    /// `exact_top_k` itself breaks ties by id, not by its input order.
    #[test]
    fn exact_top_k_breaks_ties_by_id_not_input_order() {
        let v = at_similarity(0.5, 1.0);
        let candidates = [
            candidate_of("c", v.clone(), 1.0),
            candidate_of("a", v.clone(), 1.0),
            candidate_of("top", at_similarity(0.9, 1.0), 1.0),
            candidate_of("b", v, 1.0),
        ];
        let mut unscorable = [false; 4];
        let order: Vec<String> = exact_top_k(
            &candidates,
            &query_vec(),
            scoring_of(&query_vec()),
            10,
            &mut unscorable,
        )
        .into_iter()
        .map(|(id, _)| id)
        .collect();
        assert_eq!(order, ["top", "a", "b", "c"]);
        // With a zero query every score is 0 and the id order alone decides.
        let zero = vec![0.0; DIM];
        let order: Vec<String> =
            exact_top_k(&candidates, &zero, scoring_of(&zero), 2, &mut unscorable)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
        assert_eq!(order, ["a", "b"]);
        assert_eq!(unscorable, [false; 4]);
    }

    #[test]
    fn scoped_candidates_honour_a_room_only_and_a_wing_and_room_scope() {
        let app = App::open_for_test().unwrap();
        let v = at_similarity(0.5, 1.0);
        add(&app, "w1r1", "w1", "r1", &v);
        add(&app, "w1r2", "w1", "r2", &v);
        add(&app, "w2r1", "w2", "r1", &v);
        let q = query_vec();

        let room_only = vector_candidates(&app, &q, None, 30, &scope(None, Some("r1"))).unwrap();
        assert_eq!(room_only, ["w1r1", "w2r1"]);
        let wing_and_room =
            vector_candidates(&app, &q, None, 30, &scope(Some("w1"), Some("r1"))).unwrap();
        assert_eq!(wing_and_room, ["w1r1"]);
    }

    #[test]
    fn scoped_candidates_exclude_exactly_what_the_final_filter_excludes() {
        let app = App::open_for_test().unwrap();
        let v = at_similarity(0.5, 1.0);
        let collab_room = crate::db::drawers::COLLAB_MESSAGE_ROOM;
        add(&app, "x-current", "x", "r", &v);
        add(&app, "x-old", "x", "r", &v);
        add(&app, "x-collab", "x", collab_room, &v);
        add(&app, "y-collab", "y", collab_room, &v);
        app.db
            .exec_raw("UPDATE drawers SET superseded_by = 'x-current' WHERE id = 'x-old'")
            .unwrap();
        let every_id = ["x-current", "x-old", "x-collab", "y-collab"];
        let drawers = app.db.get_drawers_by_ids(&every_id).unwrap();

        for (wing, room, include_superseded) in [
            (Some("x"), None, false),
            (Some("x"), None, true),
            (None, Some(collab_room), false),
            (Some("y"), Some(collab_room), true),
            (Some("x"), Some("r"), true),
        ] {
            let filters = SearchFilters {
                include_superseded,
                ..scope(wing, room)
            };
            let mut expected: Vec<&str> = every_id
                .iter()
                .copied()
                .filter(|id| drawer_matches_filters(&drawers[*id], &filters))
                .collect();
            expected.sort_unstable();
            let got = vector_candidates(&app, &query_vec(), None, 30, &filters).unwrap();
            assert_eq!(got, expected, "{filters:?}");
        }
        // Spot-check the rules themselves, not just the agreement.
        let with_history = SearchFilters {
            include_superseded: true,
            ..scope(Some("x"), None)
        };
        assert_eq!(
            vector_candidates(&app, &query_vec(), None, 30, &with_history).unwrap(),
            ["x-current", "x-old"]
        );
        let current = vector_candidates(&app, &query_vec(), None, 30, &scope(Some("x"), None));
        assert_eq!(current.unwrap(), ["x-current"]);
    }

    /// Every skipped row but the deliberate all-zero one is malformed and is
    /// counted in the one warning, which names the scope.
    #[test]
    fn scoped_candidates_skip_zero_malformed_and_wrong_dimension_embeddings() {
        let app = App::open_for_test().unwrap();
        add(&app, "good", "s", "r", &at_similarity(0.5, 1.0));
        add(&app, "zero", "s", "r", &vec![0.0; DIM]);
        add(&app, "short", "s", "r", &[1.0; 10]);
        // Finite components whose sum of squares overflows `f32`.
        add(&app, "overflow", "s", "r", &vec![1e20; DIM]);
        add(&app, "bad-blob", "s", "r", &at_similarity(0.5, 1.0));
        app.db
            .exec_raw("UPDATE drawers SET embedding = x'0102' WHERE id = 'bad-blob'")
            .unwrap();
        // A value that is not a blob at all (a hand edit or a foreign import):
        // the column's BLOB affinity does not convert it.
        add(&app, "text-blob", "s", "r", &at_similarity(0.5, 1.0));
        app.db
            .exec_raw("UPDATE drawers SET embedding = 'text' WHERE id = 'text-blob'")
            .unwrap();

        let (ids, log) = with_captured_log(|| {
            vector_candidates(&app, &query_vec(), None, 30, &scope(Some("s"), Some("r"))).unwrap()
        });
        assert_eq!(ids, ["good"]);
        let warnings: Vec<&str> = log.lines().filter(|line| line.contains("WARN")).collect();
        assert_eq!(warnings.len(), 1, "{log}");
        assert!(warnings[0].contains("malformed=4"), "{log}");
        assert!(warnings[0].contains("wing=\"s\""), "{log}");
        assert!(warnings[0].contains("room=\"r\""), "{log}");

        // A scope whose only skipped row is the all-zero one warns about nothing.
        add(&app, "t-good", "t", "r", &at_similarity(0.5, 1.0));
        add(&app, "t-zero", "t", "r", &vec![0.0; DIM]);
        let (ids, log) = with_captured_log(|| {
            vector_candidates(&app, &query_vec(), None, 30, &scope(Some("t"), None)).unwrap()
        });
        assert_eq!(ids, ["t-good"]);
        assert!(!log.contains("WARN"), "{log}");
    }

    /// One `SELECT` statement, for an empty scope and for a populated one (so
    /// a per-row read would show); the `PRAGMA table_info` column probe that
    /// precedes it is not a `SELECT` and is not counted.
    #[test]
    fn a_scoped_search_reads_its_rows_with_one_select() {
        let mut app = App::open_for_test().unwrap();
        for id in ["a00", "a01", "a02"] {
            add(&app, id, "a", "r", &at_similarity(0.5, 1.0));
        }

        let selects = app.db.count_select_statements();
        let ids =
            vector_candidates(&app, &query_vec(), None, 30, &scope(Some("none"), None)).unwrap();
        assert!(ids.is_empty());
        assert_eq!(selects.load(std::sync::atomic::Ordering::SeqCst), 1);

        let selects = app.db.count_select_statements();
        let ids = vector_candidates(&app, &query_vec(), None, 30, &scope(Some("a"), None)).unwrap();
        assert_eq!(ids, ["a00", "a01", "a02"]);
        assert_eq!(selects.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// The unscoped candidate sequence computed straight from the index: the
    /// top `overfetch` of the primary query (unioned with the content-word
    /// variant's), mapped to drawer ids, superseded rows dropped. An unscoped
    /// `vector_candidates` must return exactly this.
    fn expected_unscoped_candidates(
        app: &App,
        primary_vec: &[f32],
        content: Option<&[f32]>,
        overfetch: usize,
    ) -> Vec<String> {
        let state = app.index_state.read().unwrap();
        let primary_hnsw = state.index.search(primary_vec, overfetch);
        let hnsw_results = if let Some(cv) = content {
            let content_hnsw = state.index.search(cv, overfetch);
            union_hnsw(primary_hnsw, content_hnsw, overfetch)
        } else {
            primary_hnsw
        };
        let hnsw_ids: Vec<String> = hnsw_results
            .iter()
            .filter_map(|(idx, _)| state.id_map.get(*idx).cloned())
            .collect();
        drop(state);
        filter_superseded_candidate_ids(app, hnsw_ids, false).unwrap()
    }

    #[test]
    fn unscoped_candidates_are_the_plain_global_top_k() {
        let app = app_with_scope_outside_top_k();
        let q = query_vec();

        let unscoped = vector_candidates(&app, &q, None, 30, &scope(None, None)).unwrap();
        assert_eq!(unscoped, expected_unscoped_candidates(&app, &q, None, 30));
        assert_eq!(unscoped.len(), 30);
        assert!(!unscoped.contains(&"b00".to_string()));

        // Same with the content-word variant: one union capped at overfetch.
        let cv = near_query(7, 0.3);
        let with_variant = vector_candidates(&app, &q, Some(&cv), 30, &scope(None, None)).unwrap();
        assert_eq!(
            with_variant,
            expected_unscoped_candidates(&app, &q, Some(&cv), 30)
        );
        assert_eq!(with_variant.len(), 30);
    }

    /// The index holds superseded drawers too (a rebuild loads every row), so
    /// the unscoped leg drops them itself, unless history is requested. Real
    /// vectors, so the superseded drawer is in the index and outranks the
    /// current one.
    #[test]
    fn unscoped_candidates_drop_a_superseded_drawer_unless_history_is_requested() {
        let app = App::open_for_test().unwrap();
        add(&app, "current", "a", "r", &at_similarity(0.5, 1.0));
        add(&app, "old", "a", "r", &at_similarity(0.9, 1.0));
        app.db
            .exec_raw("UPDATE drawers SET superseded_by = 'current' WHERE id = 'old'")
            .unwrap();
        app.mark_dirty();
        app.ensure_index_fresh().unwrap();
        let q = query_vec();
        // Premise: the index itself returns the superseded drawer, first.
        assert_eq!(
            search_index_ids(&app, &q, None, 30).unwrap(),
            ["old", "current"]
        );

        let current = vector_candidates(&app, &q, None, 30, &scope(None, None)).unwrap();
        assert_eq!(current, ["current"]);
        let with_history = SearchFilters {
            include_superseded: true,
            ..scope(None, None)
        };
        let history = vector_candidates(&app, &q, None, 30, &with_history).unwrap();
        assert_eq!(history, ["old", "current"]);
    }

    /// A drawer stored with a zero vector (Autopilot and collab rows) has no
    /// direction, and the index's cosine distance is 0 against it, so in the
    /// index it would match every query at similarity 1.0. A rebuild leaves
    /// it out of the index, so the vector leg never returns it.
    #[test]
    fn unscoped_vector_candidates_hold_no_zero_vector_drawer() {
        let app = App::open_for_test().unwrap();
        for i in 0..20 {
            add(&app, &format!("n{i:02}"), "a", "r", &near_query(i, 0.3));
        }
        let zero = vec![0.0; DIM];
        for i in 0..5 {
            add(&app, &format!("z{i:02}"), "a", "r", &zero);
        }
        app.mark_dirty();
        app.ensure_index_fresh().unwrap();

        let ids = vector_candidates(&app, &query_vec(), None, 30, &scope(None, None)).unwrap();
        assert!(!ids.is_empty());
        assert!(ids.iter().all(|id| id.starts_with('n')), "{ids:?}");
    }
}

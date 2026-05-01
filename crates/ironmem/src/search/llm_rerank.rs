//! Optional rerank stage (pipeline step 9). Implementation-agnostic — works
//! with any `RerankerScorer`. Originally written for an ONNX cross-encoder;
//! the function name `cross_encoder_rerank` is preserved for git-blame
//! continuity even though `LlmReranker` is the production scorer today.
//!
//! Invariants:
//!   1. Pre-sort the FULL `scored` vec deterministically (score DESC,
//!      drawer_id ASC) before slicing the top-K. This makes the candidate
//!      pool reproducible across runs.
//!   2. Only the top-K window is reordered. Items at indices [K..] keep
//!      their pre-rerank order byte-identically.
//!   3. The set of drawer_ids in [..K] is unchanged — rerank reorders, never
//!      drops or duplicates.
//!   4. On scorer Err: log warn, return without mutation. No panics.

use std::sync::Arc;

use ironrace_rerank::RerankerScorer;

use crate::db::ScoredDrawer;
use crate::search::tunables;

/// Reorder the top-K of `scored` using `scorer`. See module doc for invariants.
pub fn cross_encoder_rerank(
    scorer: &Arc<dyn RerankerScorer>,
    query: &str,
    scored: &mut [ScoredDrawer],
) {
    // Invariant 1: pre-sort the full vec.
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.drawer.id.cmp(&b.drawer.id))
    });

    let k = tunables::rerank_top_k().min(scored.len());
    if k == 0 {
        return;
    }

    let passages: Vec<&str> = scored[..k]
        .iter()
        .map(|s| s.drawer.content.as_str())
        .collect();
    let new_scores = match scorer.score_pairs(query, &passages) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("cross_encoder_rerank: scorer error, skipping: {e}");
            return;
        }
    };
    if new_scores.len() != k {
        tracing::warn!(
            "cross_encoder_rerank: scorer returned {} scores for {} passages — skipping",
            new_scores.len(),
            k
        );
        return;
    }

    // Replace top-K scores in place.
    for (slot, new) in scored[..k].iter_mut().zip(new_scores) {
        slot.score = new;
    }

    // Invariant 2: re-sort ONLY the top-K window.
    scored[..k].sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.drawer.id.cmp(&b.drawer.id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::drawers::Drawer;
    use anyhow::Result;

    /// Build a `ScoredDrawer` with the minimum field set the tests need.
    /// `Drawer` has no `Default` impl, so all fields are spelled out.
    fn sd(id: i64, score: f32, content: &str) -> ScoredDrawer {
        ScoredDrawer {
            drawer: Drawer {
                id: format!("{id:032}"),
                content: content.to_string(),
                wing: "w".into(),
                room: "r".into(),
                source_file: String::new(),
                added_by: String::new(),
                filed_at: String::new(),
                date: String::new(),
            },
            score,
        }
    }

    /// Scorer that returns `-i as f32` for the i-th passage — reverses order.
    struct ReverseScorer;
    impl RerankerScorer for ReverseScorer {
        fn score_pairs(&self, _q: &str, passages: &[&str]) -> Result<Vec<f32>> {
            Ok((0..passages.len()).map(|i| -(i as f32)).collect())
        }
    }

    /// Always returns Err — for the failure-path test.
    struct ErrScorer;
    impl RerankerScorer for ErrScorer {
        fn score_pairs(&self, _q: &str, _p: &[&str]) -> Result<Vec<f32>> {
            anyhow::bail!("simulated scorer failure")
        }
    }

    #[test]
    fn top_k_window_reorders_tail_untouched() {
        // OnceLock means whichever IRONMEM_RERANK_TOP_K is first set wins.
        // For this binary we set 3.
        std::env::set_var("IRONMEM_RERANK_TOP_K", "3");

        let scorer: Arc<dyn RerankerScorer> = Arc::new(ReverseScorer);
        let mut scored = vec![
            sd(1, 0.9, "a"),
            sd(2, 0.8, "b"),
            sd(3, 0.7, "c"),
            sd(4, 0.6, "d"),
            sd(5, 0.5, "e"),
        ];

        // After the pre-sort the vec is already in score-DESC order, so the
        // tail-id snapshot we compare against is the post-pre-sort tail.
        let pre_tail: Vec<String> = scored[3..].iter().map(|s| s.drawer.id.clone()).collect();

        cross_encoder_rerank(&scorer, "q", &mut scored);

        // Tail untouched.
        let post_tail: Vec<String> = scored[3..].iter().map(|s| s.drawer.id.clone()).collect();
        assert_eq!(pre_tail, post_tail, "items outside top-K must be untouched");

        // Top-K is a permutation of the original top-K ids.
        let mut top_ids: Vec<String> = scored[..3].iter().map(|s| s.drawer.id.clone()).collect();
        top_ids.sort();
        assert_eq!(
            top_ids,
            vec![
                format!("{:032}", 1),
                format!("{:032}", 2),
                format!("{:032}", 3)
            ],
            "top-K id set must be unchanged"
        );
    }

    #[test]
    fn err_scorer_leaves_order_unchanged() {
        let scorer: Arc<dyn RerankerScorer> = Arc::new(ErrScorer);
        let mut scored = vec![sd(1, 0.9, "a"), sd(2, 0.8, "b"), sd(3, 0.7, "c")];

        let pre_after_sort: Vec<String> = {
            let mut copy = scored.clone();
            copy.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.drawer.id.cmp(&b.drawer.id))
            });
            copy.iter().map(|s| s.drawer.id.clone()).collect()
        };

        cross_encoder_rerank(&scorer, "q", &mut scored);

        let post: Vec<String> = scored.iter().map(|s| s.drawer.id.clone()).collect();
        // Pre-sort still happens before the err — order matches the sorted version.
        assert_eq!(
            pre_after_sort, post,
            "scorer Err leaves pre-sorted order intact"
        );
    }
}

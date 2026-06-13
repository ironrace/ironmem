//! Lexical shrinkage rerank — port of mempalace "hybrid v5" post-retrieval step.
//!
//! After the primary vector+BM25 merge produces a candidate list, this module
//! applies multiplicative distance shrinkage driven by three regex extractions:
//!
//!   1. Person names  (capitalized tokens, minus wh-words/auxiliaries/months)
//!   2. Quoted phrases (text inside single or double quotes, 3-60 chars)
//!   3. Predicate keywords (content words, with person names removed)
//!
//! Shrinkage is applied to the RRF *distance* proxy `(1.0 - score)`.
//! This preserves cosine similarity as the primary ordering signal and only
//! promotes candidates that contain matching lexical evidence.
//!
//! Weights (all from mempalace locomo_bench.py hybrid-v5):
//!   KW_WEIGHT    = 0.50  (predicate keywords, max 50% distance cut)
//!   QUOTED_WEIGHT = 0.60  (quoted phrases, max 60% distance cut)
//!   NAME_WEIGHT  = 0.20  (person names, max 20% distance cut — kept weak because
//!                          speaker names appear in every LoCoMo session and would
//!                          otherwise dilute predicate signal)
//!
//! Anti-overfit note: weights are module-level consts, not hardcoded at call sites.
//! The IDF-style dampener (tokens in ≥ 80% of candidates are suppressed) prevents
//! session-ubiquitous tokens from dominating on any corpus, not just LoCoMo.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use crate::db::ScoredDrawer;

use super::tunables;

// --- Regex patterns ----------------------------------------------------------

/// Capitalized word 3-16 chars. Intentionally simple; NOT_NAMES handles FP.
static NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[A-Z][a-z]{2,15}\b").unwrap());

/// Lowercase content words 3+ chars.
pub(crate) static KW_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[a-z]{3,}\b").unwrap());

/// Text inside single or double quotes, 3-60 chars.
static QUOTED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"['""]([^'""\n]{3,60})['""]"#).unwrap());

// --- Word-boundary token matcher ---------------------------------------------

/// Compile a word-boundary matcher for a single token, with light suffix
/// tolerance for common English inflections. Reuse the returned regex
/// across all candidate documents for one query — compile cost is ~µs.
///
/// Pattern: `(?i)(?:^|[^a-zA-Z0-9_]){escape(token)}(?:s|es|ed|ing|ion|ions)?(?:[^a-zA-Z0-9_]|$)`
///
/// - `(?i)` — case-insensitive (belt-and-suspenders; callers lowercase).
/// - `(?:^|[^a-zA-Z0-9_])` / `(?:[^a-zA-Z0-9_]|$)` — token must be preceded/followed
///   by line start, non-word char (space, punctuation), or end. Handles both word
///   chars and non-word chars (e.g. `c++`), and punctuation (e.g. `"suggestions?"`).
/// - `regex::escape` neutralizes regex metacharacters in the token.
/// - The optional suffix group covers verb→noun and tense inflections
///   common in English. `-ly` (adverbial) is intentionally excluded so
///   "current" does NOT match "currently".
fn compile_token_matcher(token: &str) -> Regex {
    let escaped = regex::escape(token);

    // English e-dropping morphology: tokens ending in 'e' (e.g. "bake")
    // form their -ed/-ing inflections by dropping the final 'e' first
    // ("baked", "baking"). Without this branch the matcher would miss
    // those inflected forms — observed regressing one multi-session
    // LongMemEval question ("bake" did not match "baked").
    let pattern = if let Some(stem) = token.strip_suffix('e') {
        // Stem with the final 'e' removed, escaped.
        let stem_no_e = regex::escape(stem);
        format!(
            r"(?i)(?:^|[^a-zA-Z0-9_])(?:{escaped}(?:s|es|ed|ing|ion|ions)?|{stem_no_e}(?:ed|ing))(?:[^a-zA-Z0-9_]|$)"
        )
    } else {
        format!(r"(?i)(?:^|[^a-zA-Z0-9_]){escaped}(?:s|es|ed|ing|ion|ions)?(?:[^a-zA-Z0-9_]|$)")
    };

    Regex::new(&pattern).expect("token regex must compile after escape")
}

/// Boundary-aware version of `doc.contains(token)`. Thin wrapper over
/// `Regex::is_match` so callers (the scorer and the IDF filter) share a
/// single hit-test seam.
fn token_hit(doc_lower: &str, matcher: &Regex) -> bool {
    matcher.is_match(doc_lower)
}

// --- Stop sets ---------------------------------------------------------------

/// Wh-words, auxiliaries, months, days and generic discourse words that are
/// Title-cased but are NOT person names. Matches mempalace NOT_NAMES.
static NOT_NAMES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        // Wh-words
        "What",
        "When",
        "Where",
        "Who",
        "Which",
        "Why",
        "How",
        // Auxiliaries and common verbs
        "Did",
        "Do",
        "Does",
        "Was",
        "Were",
        "Is",
        "Are",
        "Has",
        "Have",
        "Had",
        "Will",
        "Would",
        "Could",
        "Should",
        "Can",
        "May",
        "Might",
        "Said",
        "Say",
        "Tell",
        "Told",
        // Days
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
        // Months (May omitted intentionally — it's a name too; keep it as potential name)
        "January",
        "February",
        "March",
        "April",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
        // Discourse
        "Previously",
        "Recently",
        "Also",
        "Just",
        "Very",
        "More",
        "The",
        "This",
        "That",
        "These",
        "Those",
        "There",
        "Here",
        "Speaker",
        "Person",
        "Time",
        "Date",
        "Year",
        "Day",
        // Adverbs / quantifiers that get capitalised mid-question
        "About",
        "After",
        "Before",
        "Between",
        "During",
        "Since",
        "Until",
        "First",
        "Last",
        "Next",
        "Every",
        "Some",
        "Any",
        "All",
    ]
    .into_iter()
    .collect()
});

/// English stop words for predicate keyword extraction.
pub(crate) static KW_STOP: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "a", "an", "the", "and", "or", "but", "in", "on", "at", "to", "for", "of", "with", "by",
        "from", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had", "do",
        "does", "did", "will", "would", "could", "should", "may", "might", "shall", "can", "not",
        "no", "what", "when", "where", "who", "which", "how", "why", "that", "this", "these",
        "those", "there", "here", "i", "me", "my", "you", "your", "he", "she", "it", "we", "they",
        "their", "our", "its", "him", "her", "us", "them", "about", "any", "some", "all", "just",
        "more", "also", "than", "then", "into", "up", "out", "if", "so", "as", "during", "said",
        "get", "got", "give", "gave", "buy", "bought", "made", "make",
    ]
    .into_iter()
    .collect()
});

// --- Public API --------------------------------------------------------------

/// Signals extracted from the query, used for overlap scoring.
#[derive(Debug, Default)]
pub struct RerankSignals {
    pub names: Vec<String>,
    pub predicate_kws: Vec<String>,
    pub quoted_phrases: Vec<String>,
}

impl RerankSignals {
    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.predicate_kws.is_empty() && self.quoted_phrases.is_empty()
    }
}

/// Extract rerank signals from a query string.
pub fn extract_signals(query: &str) -> RerankSignals {
    // Person names — capitalized tokens not in NOT_NAMES
    let names: Vec<String> = NAME_RE
        .find_iter(query)
        .map(|m| m.as_str().to_string())
        .filter(|w| !NOT_NAMES.contains(w.as_str()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let name_words: HashSet<String> = names.iter().map(|n| n.to_lowercase()).collect();

    // All content keywords (lowercased, stop-filtered, length-capped)
    let all_kws: Vec<String> = KW_RE
        .find_iter(&query.to_lowercase())
        .map(|m| m.as_str().to_string())
        .filter(|w| !KW_STOP.contains(w.as_str()))
        .filter(|w| w.len() <= 64)
        .collect();

    // Predicate keywords = all_kws minus lowercased names (the v5 split)
    let predicate_kws: Vec<String> = all_kws
        .into_iter()
        .filter(|w| !name_words.contains(w.as_str()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    // Quoted phrases
    let quoted_phrases: Vec<String> = QUOTED_RE
        .captures_iter(query)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .filter(|p| p.len() >= 3)
        .collect();

    RerankSignals {
        names,
        predicate_kws,
        quoted_phrases,
    }
}

/// Apply multiplicative distance shrinkage rerank to a candidate list in-place.
///
/// Score is a similarity proxy (higher = better). We convert to distance
/// `d = 1 - score`, apply shrinkage, then convert back. This preserves the
/// ordering of candidates with no signal; boosted candidates only move up.
///
/// An IDF-style dampener skips tokens that appear in ≥ 80% of the candidates
/// so corpus-ubiquitous tokens (e.g. both speakers' names in a LoCoMo session)
/// do not uniformly boost every candidate.
pub fn shrinkage_rerank(candidates: &mut [ScoredDrawer], signals: &RerankSignals) {
    if signals.is_empty() || candidates.is_empty() {
        return;
    }

    let use_boundary = tunables::shrinkage_word_boundary_enabled();

    // Lowercase each candidate document exactly once, up front. Both the IDF
    // df-count below and the per-candidate overlap scoring reuse these views,
    // replacing the previous token×candidate and per-candidate re-lowercasing.
    let lower_docs: Vec<String> = candidates
        .iter()
        .map(|c| c.drawer.content.to_lowercase())
        .collect();

    let n = candidates.len() as f32;
    let threshold = (n * tunables::high_df_threshold()).ceil() as usize;

    // Build effective token lists (IDF-style: skip high-DF tokens). Each
    // surviving token carries its compiled boundary matcher so the scoring
    // loop reuses it instead of recompiling — compile happens once per token.
    let effective_kws = idf_filter(&signals.predicate_kws, &lower_docs, threshold, use_boundary);
    let effective_names = idf_filter(&signals.names, &lower_docs, threshold, use_boundary);

    // Quoted phrases are not IDF-filtered, but their lowercasing is hoisted
    // out of the per-candidate loop here.
    let quoted_lower: Vec<String> = signals
        .quoted_phrases
        .iter()
        .map(|p| p.to_lowercase())
        .collect();

    for (c, doc) in candidates.iter_mut().zip(lower_docs.iter()) {
        let kw_boost = overlap_fraction(&effective_kws, doc);
        let name_boost = overlap_fraction(&effective_names, doc);

        // Quoted phrase overlap fraction
        let quoted_boost = if quoted_lower.is_empty() {
            0.0
        } else {
            let hits = quoted_lower
                .iter()
                .filter(|p| doc.contains(p.as_str()))
                .count();
            hits as f32 / quoted_lower.len() as f32
        };

        if kw_boost == 0.0 && quoted_boost == 0.0 && name_boost == 0.0 {
            continue;
        }

        // Convert to distance, apply shrinkage, convert back
        let dist = 1.0 - c.score;
        let mut shrunken = dist;
        if kw_boost > 0.0 {
            shrunken *= 1.0 - tunables::kw_weight() * kw_boost;
        }
        if quoted_boost > 0.0 {
            shrunken *= 1.0 - tunables::quoted_weight() * quoted_boost;
        }
        if name_boost > 0.0 {
            shrunken *= 1.0 - tunables::name_weight() * name_boost;
        }
        c.score = (1.0 - shrunken).clamp(0.0, 2.0);
    }
}

/// A query token that survived the IDF filter, paired with its compiled
/// boundary matcher. The matcher is `None` in legacy substring mode.
struct EffectiveToken {
    lower: String,
    matcher: Option<Regex>,
}

/// Fraction of `tokens` that hit `doc_lower`. Shares the single hit-test seam
/// (`token_hit` for the boundary path, `contains` for legacy) with the IDF
/// df-count so both stay consistent.
fn overlap_fraction(tokens: &[EffectiveToken], doc_lower: &str) -> f32 {
    if tokens.is_empty() {
        return 0.0;
    }
    let hits = tokens.iter().filter(|t| token_in_doc(t, doc_lower)).count();
    hits as f32 / tokens.len() as f32
}

/// Does an effective token occur in a (pre-lowercased) document?
fn token_in_doc(token: &EffectiveToken, doc_lower: &str) -> bool {
    match &token.matcher {
        Some(m) => token_hit(doc_lower, m),
        None => doc_lower.contains(token.lower.as_str()),
    }
}

/// Filter a token list to those appearing in fewer than `threshold` of the
/// (pre-lowercased) candidate documents, compiling each survivor's boundary
/// matcher exactly once for reuse in scoring.
fn idf_filter(
    tokens: &[String],
    lower_docs: &[String],
    threshold: usize,
    use_boundary: bool,
) -> Vec<EffectiveToken> {
    tokens
        .iter()
        .filter_map(|t| {
            let token = EffectiveToken {
                lower: t.to_lowercase(),
                matcher: use_boundary.then(|| compile_token_matcher(&t.to_lowercase())),
            };
            let df = lower_docs
                .iter()
                .filter(|doc| token_in_doc(&token, doc))
                .count();
            (df < threshold).then_some(token)
        })
        .collect()
}

// --- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_names_excludes_wh_words() {
        let s = extract_signals("What city did Melanie visit?");
        assert!(!s.names.contains(&"What".to_string()));
        assert!(s.names.contains(&"Melanie".to_string()));
    }

    #[test]
    fn test_names_removed_from_predicates() {
        let s = extract_signals("Where did Rachel go to school?");
        assert!(!s.predicate_kws.contains(&"rachel".to_string()));
        assert!(s.predicate_kws.contains(&"school".to_string()));
    }

    #[test]
    fn test_quoted_phrases() {
        let s = extract_signals(r#"What did she call "the project"?"#);
        assert!(s.quoted_phrases.iter().any(|p| p.contains("project")));
    }

    #[test]
    fn test_shrinkage_boosts_matching_candidate() {
        use crate::db::drawers::Drawer;

        let make = |content: &str, score: f32| ScoredDrawer {
            drawer: Drawer {
                id: "x".into(),
                content: content.into(),
                wing: "w".into(),
                room: "r".into(),
                source_file: "".into(),
                added_by: "".into(),
                filed_at: "".into(),
                date: "".into(),
            },
            score,
        };

        let mut candidates = vec![
            make("Rachel went to school in Boston", 0.70),
            make("unrelated content about weather", 0.72),
        ];
        let signals = extract_signals("Where did Rachel go to school?");
        shrinkage_rerank(&mut candidates, &signals);

        // Boston/school candidate should rank above unrelated after rerank
        candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        assert!(candidates[0].drawer.content.contains("Rachel"));
    }

    #[test]
    fn test_no_panic_on_empty() {
        let mut candidates = vec![];
        let signals = extract_signals("hello world");
        shrinkage_rerank(&mut candidates, &signals); // must not panic
    }

    #[test]
    fn token_matcher_exact_form_matches() {
        let m = compile_token_matcher("suggest");
        assert!(m.is_match("can you suggest a name?"));
    }

    #[test]
    fn token_matcher_inflected_forms_match() {
        let m = compile_token_matcher("suggest");
        for body in [
            "i suggested it",
            "she is suggesting",
            "any suggestions?",
            "one suggestion stands",
        ] {
            assert!(m.is_match(body), "expected to match in {body:?}");
        }
    }

    #[test]
    fn token_matcher_does_not_match_unrelated_substring() {
        // "current" must NOT match "currently" — adverb -ly is not in the
        // suffix list. This is the photography-failure failure pattern.
        let m = compile_token_matcher("current");
        assert!(
            !m.is_match("we are currently shipping"),
            "currently must not match current"
        );
    }

    #[test]
    fn token_matcher_does_not_match_prefix_extension() {
        // Front-edge boundary: the prefix `pre` makes this not a word-boundary match.
        let m = compile_token_matcher("suggest");
        assert!(!m.is_match("we presuggest carefully"));
    }

    #[test]
    fn token_matcher_escapes_metacharacters() {
        // Tokens with regex metacharacters must compile and match literally.
        let m = compile_token_matcher("c++");
        assert!(m.is_match("i write c++ daily"));
    }

    #[test]
    fn token_matcher_is_case_insensitive() {
        // Even though callers lowercase upstream, the (?i) flag belt-and-suspenders.
        let m = compile_token_matcher("photography");
        assert!(m.is_match("Photography setup notes"));
    }

    #[test]
    fn token_hit_wraps_is_match() {
        let m = compile_token_matcher("setup");
        assert!(token_hit("a clean setup of tools", &m));
        assert!(!token_hit("a clean setup_thing", &m));
    }

    #[test]
    fn token_matcher_handles_e_dropping_inflections() {
        // English "drop-final-e, add -ed/-ing": bake -> baked, baking
        let m = compile_token_matcher("bake");
        for body in [
            "i bake every weekend",
            "i baked egg tarts last week",
            "she is baking cookies",
            "he bakes pies on sundays",
        ] {
            assert!(m.is_match(body), "expected to match in {body:?}");
        }
        // Negative cases for e-dropping path: must still respect boundaries.
        assert!(!compile_token_matcher("bake").is_match("rebake the cake"));
        assert!(!compile_token_matcher("bake").is_match("a bakery item"));
    }

    #[test]
    fn token_matcher_no_e_dropping_for_non_e_tokens() {
        // "current" does NOT end in `e` — pattern unchanged.
        let m = compile_token_matcher("current");
        assert!(m.is_match("the current state"));
        assert!(!m.is_match("we are currently shipping"));
    }

    /// Build a synthetic 200-candidate set with realistic prose so the rerank
    /// exercises the boundary matcher + per-candidate lowercasing hot paths.
    #[cfg(test)]
    fn synthetic_candidates(n: usize) -> Vec<ScoredDrawer> {
        use crate::db::drawers::Drawer;
        // A handful of repeated sentence templates with varied tokens so that
        // some candidates hit signals and some do not — mirrors a real merge.
        let bodies = [
            "Rachel went to school in Boston and studied photography every weekend",
            "Melanie visited the museum downtown and took notes about the exhibits",
            "unrelated content about weather patterns over the pacific northwest region",
            "the project shipped last quarter after several rounds of careful review",
            "he baked egg tarts and suggested a new recipe for the holiday dinner party",
            "she is currently working on suggestions for the photography portfolio layout",
        ];
        (0..n)
            .map(|i| ScoredDrawer {
                drawer: Drawer {
                    id: format!("d{i}"),
                    content: format!("{} (record {i})", bodies[i % bodies.len()]),
                    wing: "w".into(),
                    room: "r".into(),
                    source_file: "".into(),
                    added_by: "".into(),
                    filed_at: "".into(),
                    date: "".into(),
                },
                // Spread scores so ordering is non-trivial.
                score: 0.50 + (i % 50) as f32 / 100.0,
            })
            .collect()
    }

    /// Timed benchmark for issue #85. Ignored by default (timing is not a hard
    /// assertion); run explicitly to capture before/after latency numbers:
    ///
    ///   cargo test -p ironmem rerank_latency_bench -- --ignored --nocapture
    #[test]
    #[ignore = "timing benchmark; run with --ignored --nocapture"]
    fn rerank_latency_bench() {
        use std::time::Instant;

        let base = synthetic_candidates(200);
        let signals =
            extract_signals("Where did Rachel go to school and what did Melanie suggest?");

        const ITERS: usize = 500;
        let mut samples: Vec<u128> = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            // Clone fresh each iteration so prior reranks don't perturb input;
            // the clone is outside the timed region.
            let mut candidates = base.clone();
            let t0 = Instant::now();
            shrinkage_rerank(&mut candidates, &signals);
            samples.push(t0.elapsed().as_micros());
        }

        samples.sort_unstable();
        let median = samples[ITERS / 2];
        let p90 = samples[(ITERS * 9) / 10];
        let mean = samples.iter().sum::<u128>() / ITERS as u128;
        println!(
            "rerank_latency_bench (200 candidates, {ITERS} iters): \
             median={median}µs p90={p90}µs mean={mean}µs"
        );
    }
}

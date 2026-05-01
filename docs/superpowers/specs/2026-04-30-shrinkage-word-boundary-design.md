# Word-boundary matching in `shrinkage_rerank`

**Status:** design approved, awaiting plan
**Author:** Jeff Crum
**Date:** 2026-04-30
**Branch (planned):** `feat/shrinkage-word-boundary`
**Related retro:** `docs/superpowers/specs/2026-04-30-pref-enrich-experiment-retro.md`

## Problem

The `shrinkage_rerank` stage (step 8 in the search pipeline) inflates the
score of topic-irrelevant drawers because it matches predicate keywords
against drawer content using `String::contains` — a substring check that
fires on partial-token matches.

For the LongMemEval preference question
*"Can you suggest some accessories that would complement my current
photography setup?"*, the extracted predicate keywords are
`["photography", "accessories", "complement", "current", "suggest", "setup"]`.
None of these appear in a drawer about Zumba workout playlists — but
`doc.contains("suggest")` matches the noun "suggestions", and
`doc.contains("current")` matches the adverb "currently". With two of
six keywords "hitting" via substring confusion, the Zumba drawer's score
inflates from ~0.014 (RRF baseline) to 0.18, putting it above the gold
session at score 0.0164.

Direct probe (commit-time, against a cached LongMemEval haystack):

| Run | Top-1 drawer | Score | Gold rank |
|---|---|---|---|
| Shrinkage ON (default) | Zumba playlists | 0.1789 | 14 |
| Shrinkage OFF | **Gold (Sony A7R IV photography)** | **0.0164** | **1** |
| Delta on gold's own score | — | 0.0000 | — |

The gold's score is identical with shrinkage on/off — proof that the
gold doesn't share predicate_kws with the query. Shrinkage is purely
inflating the wrong drawers via substring confusion.

The same substring bug also lives in `idf_filter`, which uses
`doc.contains(token)` to decide which tokens are "too common to count."
Under-counting via substring lets through tokens that the scorer would
heavily reward, compounding the inflation.

## Goal

Replace `String::contains` with a word-boundary regex match in both
`shrinkage_rerank` and `idf_filter`, with light suffix tolerance for
common English inflections (`s|es|ed|ing|ion|ions`). Apply to
`predicate_kws` and `names`. Leave `quoted_phrases` substring (users put
quotes around literal strings they want matched as-is).

**Acceptance criteria:**

1. **Primary:** ≥ +5pp R@5 on LongMemEval `single-session-preference`
   (n=30) with `IRONMEM_SHRINKAGE_WORD_BOUNDARY=1` (default) vs `=0`.
2. **Guardrail:** no other category regresses by more than −1pp R@5.
3. **Performance:** no more than +5ms median search latency at the
   50-drawer haystack scale.

## Non-goals

- **N-gram stripping** ("can you suggest", "do you have") — separate
  hypothesis. Word-boundary matching is the smaller, more surgical fix
  pointed at by direct measurement; n-gram stripping is a follow-up if
  R@5 lift falls short.
- **Adding common verbs to `KW_STOP`** ("suggest", "want", "looking") —
  separate stop-list refinement. Fix the matcher first; decide on
  stop-list expansion based on residual misses.
- **Stemming via `rust-stemmers`** — duplicates BM25/FTS5's upstream
  porter stemming; the rerank stage should provide a *different* signal,
  not a copy.
- **Refactoring `idf_filter`'s per-candidate `to_lowercase` allocation** —
  pre-existing inefficiency, unrelated to this bug.
- **Quoted phrases** — keep substring matching. User intent for quoted
  input is literal-string matching.

## Architecture

```
crates/ironmem/src/search/rerank.rs
  ┌─────────────────────────────────────────────────────────────┐
  │ NEW: fn compile_token_matcher(token: &str) -> Regex          │
  │       (?i)\b{escape(token)}(?:s|es|ed|ing|ion|ions)?\b       │
  │                                                              │
  │ NEW: fn token_hit(doc_lower: &str, m: &Regex) -> bool        │
  │       Thin wrapper over `Regex::is_match`.                  │
  └─────────────────────────────────────────────────────────────┘
                        │
              consumed by both:
  ┌─────────────────────┴──────────────────────┐
  ▼                                            ▼
shrinkage_rerank (line 226)             idf_filter (line 296)
  • predicate_kws → token_hit            • Same matcher
  • names         → token_hit              (symmetric counting:
  • quoted_phrases → unchanged              hits and rewards
                                            agree on what counts)
```

```
crates/ironmem/src/search/tunables.rs
  + pub fn shrinkage_word_boundary_enabled() -> bool
      reads IRONMEM_SHRINKAGE_WORD_BOUNDARY, default true
```

**Single conceptual move:** swap the substring `doc.contains(token)`
checks at two sites for a precompiled word-boundary regex match. Apply
to single-token signals (predicate_kws, names). Quoted phrases keep
substring. One env-var gate, default ON.

**No new files in `src/`, no new crate-level deps** (`regex` is already
in `crates/ironmem/Cargo.toml:40`).

## Data flow

The pipeline shape is unchanged. The only change is internal to two
functions in `rerank.rs`:

```
raw query
  ├─ sanitize_query()                       [unchanged]
  ├─ embed(clean_query) → HNSW              [unchanged]
  ├─ bm25_search(clean_query)               [unchanged]
  ├─ RRF fuse                               [unchanged]
  ├─ kg_boost                               [unchanged]
  ├─ collapse_synthetic_into_parents        [unchanged]
  ├─ extract_signals(clean_query)           [unchanged]
  │   → RerankSignals { names, predicate_kws, quoted_phrases }
  │
  ├─ shrinkage_rerank(candidates, signals)  [CHANGED INTERNALS]
  │   ├─ pre-compile a Regex per effective_kw and per effective_name
  │   ├─ for each candidate doc:
  │   │     kw_boost   = matchers.iter().filter(token_hit).count() / N
  │   │     name_boost = matchers.iter().filter(token_hit).count() / N
  │   │     quoted_boost = doc.contains(p)              [unchanged]
  │   └─ existing multiplicative shrinkage arithmetic   [unchanged]
  │
  └─ sort + truncate                        [unchanged]
```

**Tunable resolution (per-call, not OnceLock-cached):**
- `IRONMEM_SHRINKAGE_WORD_BOUNDARY=0` → legacy substring path.
- Anything else (including unset) → new word-boundary path.

Per-call evaluation matches the `pref_enrich_enabled` pattern — bench
needs to flip per-test.

**Cost analysis** (per query):
- ≤10 unique tokens → ≤10 `Regex::new` compilations → ≤10µs total.
- Per-candidate: `Regex::is_match` is O(doc_len) — same order as
  `String::contains` over the lowercased copy. No additional allocation
  beyond what already happens (`doc.to_lowercase()` is the dominant cost
  in this loop).
- Total expected per-query overhead: <1ms at the LongMemEval haystack
  size; bench will confirm.

## Components & file changes

### Modified files

| Path | Action | Purpose |
|---|---|---|
| `crates/ironmem/src/search/rerank.rs` | modify | Add `compile_token_matcher` + `token_hit` helpers; thread tunable through `shrinkage_rerank` and `idf_filter` |
| `crates/ironmem/src/search/tunables.rs` | modify | Add `shrinkage_word_boundary_enabled()` |
| `crates/ironmem/tests/shrinkage_word_boundary_test.rs` | create | 8 integration tests covering both branches |

### New helper in `rerank.rs`

```rust
/// Compile a word-boundary matcher for a single token, with light suffix
/// tolerance for common English inflections. Reuse the returned regex
/// across all candidate documents for one query — compile cost ~µs.
fn compile_token_matcher(token: &str) -> Regex {
    // (?i) = case-insensitive at compile time
    // \b   = unicode word boundary (regex crate default)
    // (?:s|es|ed|ing|ion|ions)? = English-suffix tolerance, bounded
    let escaped = regex::escape(token);
    Regex::new(&format!(r"(?i)\b{escaped}(?:s|es|ed|ing|ion|ions)?\b"))
        .expect("token regex must compile after escape")
}

/// Boundary-aware version of `doc.contains(token)`. The caller chooses
/// boundary mode vs substring mode based on the tunable. Both modes are
/// exposed so the IDF filter (counting hits across docs) and the scorer
/// (per-doc hit) use the same matcher and counts agree with rewards.
fn token_hit(doc_lower: &str, matcher: &Regex) -> bool {
    matcher.is_match(doc_lower)
}
```

### `shrinkage_rerank` body change (lines 226–293)

Pre-compile matchers *outside* the per-candidate loop; thread `use_boundary`
through both kw_boost and name_boost branches; keep quoted_boost
substring. Score arithmetic unchanged.

```rust
pub fn shrinkage_rerank(candidates: &mut [ScoredDrawer], signals: &RerankSignals) {
    if signals.is_empty() || candidates.is_empty() { return; }

    let n = candidates.len() as f32;
    let threshold = (n * tunables::high_df_threshold()).ceil() as usize;
    let effective_kws   = idf_filter(&signals.predicate_kws, candidates, threshold);
    let effective_names = idf_filter(&signals.names,         candidates, threshold);

    let use_boundary = tunables::shrinkage_word_boundary_enabled();
    let kw_matchers: Vec<Regex> = if use_boundary {
        effective_kws.iter().map(|kw| compile_token_matcher(kw)).collect()
    } else { Vec::new() };
    let name_matchers: Vec<Regex> = if use_boundary {
        effective_names.iter().map(|n| compile_token_matcher(&n.to_lowercase())).collect()
    } else { Vec::new() };

    for c in candidates.iter_mut() {
        let doc = c.drawer.content.to_lowercase();

        let kw_boost = if effective_kws.is_empty() { 0.0 }
            else if use_boundary {
                let hits = kw_matchers.iter().filter(|re| token_hit(&doc, re)).count();
                hits as f32 / effective_kws.len() as f32
            } else {
                let hits = effective_kws.iter().filter(|kw| doc.contains(kw.as_str())).count();
                hits as f32 / effective_kws.len() as f32
            };

        let name_boost = if effective_names.is_empty() { 0.0 }
            else if use_boundary {
                let hits = name_matchers.iter().filter(|re| token_hit(&doc, re)).count();
                hits as f32 / effective_names.len() as f32
            } else {
                let hits = effective_names.iter()
                    .filter(|n| doc.contains(n.to_lowercase().as_str())).count();
                hits as f32 / effective_names.len() as f32
            };

        // quoted_boost: unchanged substring path
        let quoted_boost = if signals.quoted_phrases.is_empty() { 0.0 }
            else {
                let hits = signals.quoted_phrases.iter()
                    .filter(|p| doc.contains(p.to_lowercase().as_str())).count();
                hits as f32 / signals.quoted_phrases.len() as f32
            };

        if kw_boost == 0.0 && quoted_boost == 0.0 && name_boost == 0.0 { continue; }

        // Existing distance-shrinkage arithmetic — unchanged.
        let dist = 1.0 - c.score;
        let mut shrunken = dist;
        if kw_boost     > 0.0 { shrunken *= 1.0 - tunables::kw_weight()     * kw_boost; }
        if quoted_boost > 0.0 { shrunken *= 1.0 - tunables::quoted_weight() * quoted_boost; }
        if name_boost   > 0.0 { shrunken *= 1.0 - tunables::name_weight()   * name_boost; }
        c.score = (1.0 - shrunken).clamp(0.0, 2.0);
    }
}
```

### `idf_filter` body change (lines 296–309)

Same matcher-selection branch. Symmetric with the scorer.

```rust
fn idf_filter(tokens: &[String], candidates: &[ScoredDrawer], threshold: usize) -> Vec<String> {
    let use_boundary = tunables::shrinkage_word_boundary_enabled();
    tokens.iter().filter(|t| {
        let t_lower = t.to_lowercase();
        let df = if use_boundary {
            let m = compile_token_matcher(&t_lower);
            candidates.iter()
                .filter(|c| m.is_match(&c.drawer.content.to_lowercase()))
                .count()
        } else {
            candidates.iter()
                .filter(|c| c.drawer.content.to_lowercase().contains(t_lower.as_str()))
                .count()
        };
        df < threshold
    }).cloned().collect()
}
```

### New tunable in `tunables.rs`

```rust
/// `IRONMEM_SHRINKAGE_WORD_BOUNDARY=0` reverts to the legacy substring
/// matcher in `shrinkage_rerank` (and its IDF filter). Default ON.
///
/// The legacy substring path causes false-positive boosts: predicate_kws
/// like "suggest" or "current" substring-match drawer text ("suggestions",
/// "currently"), inflating topic-irrelevant drawers. Word-boundary matching
/// with light suffix tolerance fixes this without harming inflected-form
/// recall.
pub fn shrinkage_word_boundary_enabled() -> bool {
    !matches!(
        std::env::var("IRONMEM_SHRINKAGE_WORD_BOUNDARY").as_deref(),
        Ok("0") | Ok("false")
    )
}
```

Not OnceLock-cached (per-test flippability, matches `pref_enrich_enabled`).

### Tests

`crates/ironmem/tests/shrinkage_word_boundary_test.rs` — 8 cases:

1. **`boundary_match_exact_form`** — `"suggest"` matches `"suggest"` in doc.
2. **`boundary_match_inflected_forms`** — `"suggest"` matches
   `"suggested"`, `"suggesting"`, `"suggestions"`. (Suffix tolerance.)
3. **`boundary_no_match_substring_with_prefix`** — `"suggest"` does NOT
   match `"presuggest"` in doc. (Boundary respects the front edge.)
4. **`boundary_no_match_unrelated_substring`** — `"current"` does NOT
   match `"currently"` in doc. (Adverbial `-ly` is not in the suffix
   list; this is the photography-failure pattern.)
5. **`legacy_substring_still_matches_when_disabled`** — set
   `IRONMEM_SHRINKAGE_WORD_BOUNDARY=0`, verify `"current"` substring-matches
   `"currently"`. Proves the tunable wires both paths.
6. **`name_substring_bug_fixed`** — name `"Sam"` does NOT match
   `"sample"` in boundary mode. Matches in substring mode.
7. **`quoted_phrase_unchanged`** — `"the project"` substring-matches
   `"the project"` in BOTH modes.
8. **`idf_filter_uses_same_matcher`** — synthesize a candidate set where
   `"suggest"` would be filtered under boundary (correctly counted hits
   ≥ threshold) but kept under substring (under-counted via partial-token
   matches). Assert filter behavior matches matcher.

The 8 tests live in one integration-test file; tests 5–8 acquire a static
`ENV_LOCK: Mutex<()>` to serialize env-var-touching test access (matches
the pattern from `preference_enrichment_test.rs`).

## Error handling

- Regex compilation: `compile_token_matcher` calls
  `expect("token regex must compile after escape")`. With `regex::escape`
  applied first, any failure is a `regex` crate bug and should fail loud.
  Caught at first-call by tests.
- Tunable parse: malformed env var falls through to default ON. No error
  path.
- Empty token list / empty candidate list: existing fast-paths in
  `shrinkage_rerank` return early. New code adds nothing here.
- UTF-8: `\b` in the `regex` crate is Unicode-aware. Names like
  `"François"` tokenize correctly.

## Observability

- One new `tracing::trace!` inside `shrinkage_rerank` reporting
  `use_boundary` once per call. Off in release at default log level.
- Existing pipeline-end `tracing::debug!("search_pipeline_telemetry", …)`
  unchanged; sufficient to attribute score deltas to the matcher swap.

## Rollout

- **PR 1 (this spec):** land everything default-ON. Bench measures
  preference R@5 lift OFF (substring, legacy) vs ON (boundary, default).
  No production behavior change required if bench shows no regression.
- **No PR 2 needed.** Default is already ON. The env var stays as a
  permanent opt-out.

## Acceptance verification

```bash
# Reuse the OFF baseline from the pref-enrich retro (n=30 preference)
# /tmp/pref_off.json — already on disk, gold_rank computed 1-indexed.

# New ON run with word-boundary enabled (default; explicit for clarity)
IRONMEM_SHRINKAGE_WORD_BOUNDARY=1 IRONMEM_EMBED_MODE=real \
  python3 scripts/benchmark_longmemeval.py /tmp/lme_pref_only.json \
  --limit 30 --ironmem-binary target/release/ironmem \
  --db-cache-dir /tmp/bench_word_boundary_on \
  --per-question-json /tmp/pref_word_boundary.json

# Comparison: per-type R@5 deltas
python3 - <<'PY'
import json
def L(p): return [json.loads(l) for l in open(p) if l.strip()]
off = L('/tmp/pref_off.json')
on  = L('/tmp/pref_word_boundary.json')
def at(rs, k):
    pref = [r for r in rs if r['question_type'] == 'single-session-preference']
    hit  = sum(1 for r in pref if r.get('gold_rank') is not None and 1 <= r['gold_rank'] <= k)
    return hit, hit/len(pref)*100
for k in (1,3,5,10):
    h_off, p_off = at(off, k); h_on, p_on = at(on, k)
    print(f'  R@{k}: {p_off:5.1f}% -> {p_on:5.1f}%  delta={p_on-p_off:+.2f}pp')
PY
```

Acceptance check: preference R@5 delta ≥ +5pp.

## Risks

- **Suffix tolerance still allows the "suggest" → "suggestions" case.**
  The empirical photography-question bug has two contributing tokens:
  `"current"` → `"currently"` (FIXED — `-ly` not in the suffix list) and
  `"suggest"` → `"suggestions"` (NOT fixed — `-ions` IS in the list).
  Honest framing: this spec partially closes the photography failure.
  The remaining `"suggest"` inflation needs a follow-up (verb stop-listing
  or n-gram stripping). The +5pp acceptance reflects this; full closure
  is a separate spec.
- **`\b` semantics in unicode mode** could be too eager on hyphenated
  tokens or domain names. Tests cover the common ASCII English cases; if
  the bench surfaces an issue on real conversation text, narrow `\b` via
  a future tunable.
- **Other code paths might depend on substring inflation** to surface
  relevant-but-low-RRF docs. The probe with shrinkage OFF showed gold at
  rank 1 with score 0.0164 — the OFF behavior is *not catastrophic*, just
  doesn't lift good docs above near-tied competitors. This suggests
  shrinkage's inflation is mostly noise, not signal — but the bench
  might surprise us on non-preference categories. Guardrail: -1pp R@5
  budget per other category.

## References

- Diagnostic probe (commit-time, against
  `/tmp/bench_cache_off/b98a594c7133db1a7f86dacb.sqlite3`):
  `python3 /tmp/probe_stages.py … "Can you suggest some accessories that
  would complement my current photography setup?" "1e2176863a89…"`
  showed gold rank 14 → 1 with shrinkage off.
- `crates/ironmem/src/search/rerank.rs:226` — `shrinkage_rerank`
- `crates/ironmem/src/search/rerank.rs:296` — `idf_filter`
- `crates/ironmem/src/search/rerank.rs:175` — `extract_signals`
- Pref-enrich retro: `docs/superpowers/specs/2026-04-30-pref-enrich-experiment-retro.md`

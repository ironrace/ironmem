# Preference-enrichment synthetic-doc port from mempalace

**Status:** design approved, awaiting plan
**Author:** Jeff Crum
**Date:** 2026-04-29
**Branch (planned):** `feat/pref-enrich-synthetic-doc`

## Problem

On LongMemEval's `single-session-preference` slice (30 of 500 questions), ironmem
underperforms because the question vocabulary rarely matches the haystack
vocabulary. The user said "Adobe Premiere Pro" in session 12; the question asks
about "video editing." Dense embedding sometimes bridges this; often it
doesn't.

Mempalace solves the same gap by extracting first-person preference phrases
from each session via a regex pass, building a synthetic
`"User has mentioned: phrase1; phrase2; …"` document per session, and indexing
it with the same `corpus_id` as its source session. The synthetic doc gives the
embedding model a target it *can* match — a sentence containing "Premiere Pro"
explicitly — while still crediting the gold session for top-K recall. Reported
lift: `single-session-preference` R@5 went from 93.3% → 96.7% (+3.4pp).

This spec ports that mechanism to ironmem.

## Goal

Add an opt-in ingest-time preference extractor that produces one synthetic
sibling drawer per conversational drawer. Search collapses sibling hits into
their parent so top-K results stay clean. Off by default; flipped on for the
LongMemEval bench via `IRONMEM_PREF_ENRICH=1`.

**Acceptance criterion:** with `IRONMEM_PREF_ENRICH=1`, R@5 on the
`single-session-preference` slice improves by ≥ +2pp vs `IRONMEM_PREF_ENRICH=0`,
with no regression > -0.5pp on any other LongMemEval category.

## Non-goals

- LLM-based extractor (the trait makes it pluggable; implementation is later).
- A new `parent_drawer_id` schema column (revisit only if a second feature
  needs the same shape).
- Hall classification / palace mode.
- Parallel multi-query expansion (separate spec).
- Convomem / membench / locomo benches — preference is LongMemEval-specific.

## Architecture

```
┌─ crates/ironrace-pref-extract  (new crate)
│    pub trait PreferenceExtractor { fn extract(&self, text: &str) -> Vec<String>; }
│    pub struct RegexPreferenceExtractor;     // V4 patterns ported from mempalace
│    pub fn looks_conversational(text: &str) -> bool;
│    pub fn synthesize_doc(phrases: &[String]) -> Option<String>;
│
└─ crates/ironmem
     src/mcp/tools/drawers.rs   handle_add_drawer  ── new enrichment pass
     src/search/pipeline.rs     step 7.5: collapse synthetic → parent score
     src/search/tunables.rs     pref_enrich_enabled()
     src/db/drawers.rs          delete_drawers_by_parent()
```

A drawer can have at most one synthetic sibling. The sibling is a normal row
in the `drawers` table with `source_file = "pref:<parent_drawer_id>"` as the
backref. The `pref:` sentinel disambiguates from `mine_directory`'s real file
paths and avoids a schema migration.

The crate split mirrors the existing `ironrace-rerank` / `ironrace-embed` /
`ironrace-core` factoring: pure, testable, reusable, no I/O in the extractor
crate.

## Data flow

### Ingest path (`handle_add_drawer`)

```
content (raw)
  │
  ├─ sanitize → embed → insert raw drawer (id_p)        [unchanged]
  │
  └─ if pref_enrich_enabled() && looks_conversational(content):
        phrases = RegexPreferenceExtractor.extract(content)
        if phrases.is_empty(): return Ok
        synth   = "User has mentioned: " + phrases.join("; ")
        id_s    = sha256(synth + wing + room)
        embed(synth) → insert synth drawer with
            source_file = "pref:" + id_p
        insert_into_index(id_s, synth_emb)
```

Both inserts execute in the same `with_transaction` call so there is no torn
state if the second embed fails.

### Search path (new step 7.5 in `pipeline.rs`)

Between rerank and truncation:

```
candidates: Vec<ScoredDrawer>   // already RRF-merged + reranked
  │
  └─ collapse_synthetic_into_parents(candidates):
       1. partition into (synth_hits, real_hits) by `source_file.starts_with("pref:")`
       2. for each synth hit S with parent_id = S.source_file["pref:".len()..]:
            if parent in real_hits:
                real_hits[parent].score = max(real_hits[parent].score, S.score)
            else:
                fetch parent by id (one indexed lookup); insert with S.score
       3. drop all synth hits
       4. re-sort by score; truncate to filters.limit
```

Top-K is K *real* drawers, scores possibly elevated by their synthetic siblings.
Synthetic content never leaves the search subsystem — production callers and
MCP clients see only the parent rows.

### Bench harness

No code changes. `scripts/benchmark_longmemeval.py:178-188` already forwards
`IRONMEM_PREF_ENRICH` to the server env and keys the corpus cache off it. Run:

```
IRONMEM_PREF_ENRICH=1 python3 scripts/benchmark_longmemeval.py \
    --limit 165 --per-question-json /tmp/pref_on.json

IRONMEM_PREF_ENRICH=0 python3 scripts/benchmark_longmemeval.py \
    --limit 165 --per-question-json /tmp/pref_off.json
```

Compare `per_type["single-session-preference"][5]` between the two runs.

## Components & file changes

### New crate: `crates/ironrace-pref-extract/`

- `Cargo.toml` — depends on `regex`; no async, no I/O.
- `src/lib.rs` — public surface:
  ```rust
  pub trait PreferenceExtractor: Send + Sync {
      fn extract(&self, text: &str) -> Vec<String>;
  }
  pub struct RegexPreferenceExtractor;
  impl Default for RegexPreferenceExtractor;
  pub fn looks_conversational(text: &str) -> bool;  // first-person pronoun
                                                     // ("I", "I've", "I'm",
                                                     // "I'd", "my", "me")
                                                     // in the first 500 chars
  pub fn synthesize_doc(phrases: &[String]) -> Option<String>;
  ```
- `src/patterns.rs` — V4 regexes ported from
  `mempalace/benchmarks/longmemeval_bench.py:1587-1610`, compiled once via
  `OnceLock<Vec<Regex>>`. Bounded length (5..=80 chars), case-insensitive,
  dedup-preserving-order, capped at 12 phrases per session.
- `tests/extract.rs` — fixture-based unit tests covering each pattern family
  plus the negative case (file-shaped content yields nothing).

### Changes in `crates/ironmem/`

- `Cargo.toml` — add `ironrace-pref-extract = { path = "../ironrace-pref-extract" }`.
- `src/search/tunables.rs` — add
  `pub fn pref_enrich_enabled() -> bool` reading `IRONMEM_PREF_ENRICH`,
  default `false`, OnceLock-cached.
- `src/mcp/tools/drawers.rs::handle_add_drawer` — after the existing insert,
  run the enrichment block when tunable on, content sniffs conversational,
  and ≥ 1 phrase extracted. Both row inserts execute in one
  `with_transaction` call. After the transaction commits, call
  `insert_into_index` twice (once per drawer id) — same pattern the existing
  single-drawer path uses, just doubled.
- `src/db/drawers.rs` — add
  `delete_drawers_by_parent(parent_id: &str) -> Result<usize, MemoryError>`.
  Call it from `delete_drawer_tx` so deletes cascade.
- `src/search/pipeline.rs` — new step 7.5 `collapse_synthetic_into_parents`
  between rerank and truncate. Batched parent lookup for orphan-synth case.
- `src/search/mod.rs` — re-export the collapse fn for tests.

### Schema / migration

None. `source_file` already exists; we establish a sentinel-prefix convention
(`"pref:<id>"`) here.

### MCP tool surface

Unchanged. `add_drawer` and `search` keep their existing arg shapes.

## Tests

1. `crates/ironrace-pref-extract/tests/extract.rs` — pattern coverage +
   negative cases (Rust source, prose without first-person, empty string).
2. `crates/ironmem/tests/preference_enrichment_test.rs` (new) —
   `add_drawer(conversational_content)` with env on creates parent + synth
   rows; `delete_drawer(parent)` cascades to synth; with env off creates
   only the parent row.
3. `crates/ironmem/tests/search_collapse_test.rs` (new) — sibling pair where
   synth ranks above parent → top-K shows parent only with synth's score;
   sibling pair where parent ranks higher → unchanged top-K; orphan synth
   (parent not in candidate set) → parent fetched and surfaced.
4. The existing test suite must pass unchanged with `IRONMEM_PREF_ENRICH`
   unset (default-off contract).

## Error handling

- Extraction is infallible. Regex compilation happens once inside `OnceLock`;
  a malformed pattern panics at startup, caught immediately by tests.
- Empty extraction is the common case for non-conversational input. Return
  early; no synthetic insert; no log noise.
- Synthetic embed failure → log at `warn`, return `Ok(())` for the parent
  insert. The parent already committed; we don't fail the whole `add_drawer`
  because a recall *enhancement* failed.
- Orphan-parent at search time (parent deleted between index and query) →
  drop the synth quietly. Never surface a synthetic-only result.
- Concurrent ingest: existing `insert_into_index` lock covers both inserts.
  No new lock surface.

## Observability

- One `tracing::debug!` per enrichment:
  `"pref_enrich: parent={id_p} phrases={n} synth_id={id_s}"`.
- Counters on `App`: `pref_enrich_total`,
  `pref_enrich_skipped_non_conversational`. Sanity-check that the sniff isn't
  over- or under-firing on real corpora.
- Search-side: `tracing::trace!` when collapse fires and how many slots it
  freed.

## Rollout

- **PR 1 (this spec):** land everything default-OFF. Bench shows R@5 lift on
  preference slice. No production behavior change.
- **PR 2 (later, separate spec):** flip `pref_enrich_enabled()` default to
  `true` after measuring on a real chatty corpus: size growth, ingest latency,
  recall on non-preference categories.
- The env var is preserved as an opt-out after the default flips.

## Risks

- **Extraction sniff false positives** (e.g. a dictation transcript of a Rust
  file). Mitigated by `looks_conversational` requiring first-person pronouns;
  falls back to zero phrases → no insert.
- **Embedding model mismatch with mempalace.** Their +3.4pp may not transfer
  1:1. Acceptance bar set at +2pp.
- **Drawer count growth.** On a fully-conversational corpus, every drawer
  gains a sibling — 2× row count. The synthetic is small (≤ 12 phrases ×
  ~80 chars, ~200 chars total); HNSW index roughly 2×; query latency
  expected to be unchanged because top-K dominates over candidate set size.

## References

- Mempalace V4 extractor: `~/git-repos/mempalace/benchmarks/longmemeval_bench.py:1587-1610`
- Mempalace failure-mode comment: `longmemeval_bench.py:727-730`
- Mempalace reported lift: `~/git-repos/mempalace/benchmarks/HYBRID_MODE.md:151-167`
- Existing ironmem env-var anticipation: `scripts/benchmark_longmemeval.py:178-188`
- Existing ironmem add path: `crates/ironmem/src/mcp/tools/drawers.rs:14`
- Existing search pipeline: `crates/ironmem/src/search/pipeline.rs`

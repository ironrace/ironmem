# Preference-enrichment experiment retrospective

**Status:** experiment closed, infrastructure landed default-OFF
**Author:** Jeff Crum
**Date:** 2026-04-30
**Branch:** `feat/pref-enrich-synthetic-doc` (15 commits, ready to merge into `feat/cross-encoder-rerank`)
**Spec:** `docs/superpowers/specs/2026-04-29-preference-synthetic-doc-design.md`
**Plan:** `docs/superpowers/plans/2026-04-29-preference-synthetic-doc.md`

## Goal recap

Port mempalace's per-session preference extractor + synthetic sibling
drawer + search-time collapse to ironmem. Acceptance: ≥ +2pp R@5 on the
LongMemEval `single-session-preference` slice.

## What landed

15 commits across two phases:

**Phase 1 — original 8-task plan (committed 21:07–21:48):**

- `de14910` scaffold `ironrace-pref-extract` crate
- `8337288` port mempalace V4 regex set + extractor trait
- `f6eabb9` `pref_enrich_enabled()` tunable (default off)
- `1bd16ef` ironmem dep on `ironrace-pref-extract`
- `b747fa5` cascade-delete synthetic preference siblings
- `def5e52` synthesize preference sibling at `add_drawer` time (3 integration tests)
- `d66bcf5` harden pref_enrich error paths (fix HIGH review findings)
- `6c0269c` step 7.5 collapses synthetic siblings into parents (5 integration tests)
- `933cfe8` cleanup search_collapse_test (dead code, helper rename)

**Phase 2 — empirical iteration (committed 22:48–next-day):**

- `351a667` fix(bench): `args.binary` → `args.ironmem_binary`
- `3dd59d0` drop `"User has mentioned: "` prefix from synth doc
- `0f6ec4b` LLM-based PreferenceExtractor (claude -p backend)
- `e06a613` API backend for LLM extractor (ureq → api.anthropic.com)

Total: 1 new crate (`ironrace-pref-extract`), 6 modified ironmem files,
1 schema-free sentinel convention (`source_file = "pref:<parent_id>"`),
20 unit tests + 9 integration tests, all green.

## Empirical results

LongMemEval `single-session-preference` slice (n=30, no rerank, no shrinkage tuning):

| Iteration | Synth content | Preference R@5 | Δ vs OFF | Per-question shifts |
|---|---|---|---|---|
| **OFF (baseline)** | — | **70.0%** | — | — |
| V1 regex with `"User has mentioned:"` prefix | first-person fragments | 70.0% | **+0.0pp** | net-neutral (165-q slice) |
| V2 regex bare format | first-person fragments | 66.7% | **-3.3pp** | net negative |
| V3 LLM Haiku via API backend | rich topic vocabulary | **70.0%** | **+0.0pp** | **0 gained / 0 lost / 30 unchanged** |

**Acceptance criterion was +2pp. None of the three variants met it.** V3
is the most striking: with the LLM producing *perfect* topic-vocabulary
synth content (verified by direct inspection — synth for the photography
gold session reads "The user is seeking recommendations for Sony A7R IV
camera accessories and equipment upgrades, including flash options like
the Godox V1 versus Sony HVL-F60RM, protective cases, external battery
packs, lens cleaning methods..."), **no preference question's gold rank
changed at all** — every single one of 30 questions had identical
gold_rank in V3 vs OFF.

**API backend cost confirmation:** the 30-question run with
`IRONMEM_PREF_LLM_BACKEND=api` completed in ~7 minutes (vs an estimated
~3-5 hours for the CLI backend), with one in-process HTTPS call per
add_drawer and zero subprocess fan-out. Confirmed the resource issue
that motivated the API backend, and the API path itself works.

## Why none of the variants worked

The synth-doc mechanism is sound: synth rows are correctly persisted,
correctly indexed in HNSW, and correctly hidden from search responses
by step 7.5 collapse. Every observable invariant of the spec held under
direct probing.

The bottleneck is **downstream of the synth-doc mechanism**, in step 8
(shrinkage rerank). For the photography question:

- **Query:** "Can you suggest some accessories that would complement my current photography setup?"
- **Gold parent's V3 synth (LLM-generated):** "The user is seeking recommendations for Sony A7R IV camera accessories and equipment upgrades, including flash options like the Godox V1 versus Sony HVL-F60RM, protective cases, external battery packs, lens cleaning methods for their Sony 24-70mm lens, tripod options like the Gitzo GT3543LS, the G-Lock Ultra system, and a durable camera bag designed for Sony cameras."
- **Post-collapse rank of gold parent:** 13 with score **0.0164**.
- **Post-collapse rank 0–12:** Zumba playlists, data-science notes, CV resume, summer playlist, soup recipes, etc., all with scores ~0.18.

Why are unrelated drawers at score ~0.18 while a topically-perfect
synth's parent scores 0.0164? Direct sweep: querying for vocabulary
that *only* appears in the gold conversation ("Sony A7R IV Godox V1
flash") puts the gold parent at rank 0 with score **0.6091** — 40×
higher than the next result. The retrieval machinery *can* surface
gold; it just doesn't when the query uses abstract vocabulary
("accessories", "complement", "photography setup") that the user's raw
turns don't echo verbatim.

**The shrinkage rerank stage is over-rewarding generic question-form
word matches.** Drawers that share words like "looking for some", "do
you have", "can you suggest", "recommend" with the query — regardless
of topic — get score multipliers from the keyword/predicate rerank.
Topical synth content with score 0.0164 cannot beat unrelated drawers
at 0.18 because the rerank is lifting the wrong thing.

This is **not a synth-doc problem** and cannot be fixed by tuning the
extractor. It's a query-sanitizer/rerank issue independent of this
spec, and is the correct target for the next experiment.

## What's reusable

The infrastructure landed by this branch is correct, tested, and
reusable for any future synth-doc-style experiment under the same
trait abstraction:

- `ironrace_pref_extract::PreferenceExtractor` trait
- Three concrete impls: `RegexPreferenceExtractor` (V4 regexes),
  `LlmPreferenceExtractor` with `ClaudeCliClient`, `LlmPreferenceExtractor`
  with `ApiClient` (ureq → Messages API)
- Sentinel-prefix sibling convention on `source_file` column (no migration)
- `delete_drawers_by_parent_tx` cascade delete
- `pipeline::collapse_synthetic_into_parents` step 7.5 (commensurable
  scoring; orphan-fetch path; FTS-aware)
- `IRONMEM_PREF_ENRICH` (master switch), `IRONMEM_PREF_EXTRACTOR`
  (regex|llm), `IRONMEM_PREF_LLM_BACKEND` (cli|api),
  `IRONMEM_PREF_LLM_MODEL`, `IRONMEM_PREF_LLM_TIMEOUT_MS`,
  `IRONMEM_PREF_LLM_MAX_TOKENS`
- Bench harness improvements: stderr capture (was DEVNULL),
  `args.binary`→`args.ironmem_binary` fix, cache key extended to
  include `IRONMEM_PREF_EXTRACTOR` and `IRONMEM_PREF_LLM_MODEL`

The API backend (`e06a613`) is a real win on its own merits — one
in-process HTTPS call per add_drawer instead of subprocess fan-out
(measured ~3.5s API vs ~17s CLI per call when the spawned `claude`
boots a nested ironmem MCP server + hooks). Useful for any future LLM
ingest experiment.

## Decision

**Merge as default-OFF infrastructure.** All tunables default to off /
regex / cli; with no env overrides, production behavior is unchanged.
The trait + collapse + sentinel + API backend stand on their own as
reusable scaffolding; future synth-doc strategies (e.g. hall-routed,
query-expansion-derived, retrieval-feedback-derived) can land under the
same abstractions.

Cost when ON: +4ms median search latency, +N rows in DB (where N is
the number of conversational sessions), +1 LLM call per add_drawer if
`IRONMEM_PREF_EXTRACTOR=llm`.

## Next experiment (separate spec)

Title: **"Question-form word stripping in the search sanitizer to
prevent shrinkage-rerank score inflation."**

Hypothesis: stripping question-form n-grams ("can you suggest", "do
you have", "i'm looking for", etc.) from queries before they reach
keyword shrinkage scoring will prevent topic-irrelevant drawers from
inheriting boosts due to shared question-form vocabulary. Apply only
to the rerank-side query, not the embedded query (embedding may benefit
from full natural phrasing).

Expected impact: lift on `single-session-preference` and possibly
`single-session-user`; should not regress categories that already
match topic vocabulary literally.

Brainstorm + spec to follow under `superpowers:brainstorming`.

## Lessons

- **Embedder-specific lifts don't always transfer.** Mempalace's
  +3.4pp on this slice came on `all-MiniLM-L6-v2`; we ran on
  `bge-base-en-v1.5`. Absolute baselines and relative gains both
  shifted, even though the algorithm transferred faithfully.
- **Subprocess fan-out is expensive.** Spawning `claude -p` from
  inside a binary that's part of the user's MCP config recursively
  loads the whole config — measured at ~14s overhead per call before
  the actual LLM work. Always use the API directly for high-volume
  ingest paths.
- **Off-by-one math kills empirical work.** Initial reports said V2
  was +0.0pp; corrected math says -3.3pp. `gold_rank` is 1-indexed.
  Always cross-check against the bench's own printed summary.
- **A working mechanism with negative results is not a failure** —
  it eliminates a hypothesis cleanly and produces reusable
  infrastructure. The trait + collapse + sentinel + API backend are
  load-bearing scaffolding for any future ingest-time enrichment work.

# ProvBench Phase 0b Labeler

Mechanical labeler for the ProvBench-CodeContext pilot corpus.
**Frozen contract:** `../SPEC.md`. This crate is excluded from the
ironrace-memory workspace because Phase 0 must be releasable as a
standalone reproducible artifact.

## Reproducing the pilot corpus

1. **Verify tooling pins.**
   ```
   cargo run --manifest-path benchmarks/provbench/labeler/Cargo.toml -- verify-tooling
   ```
   The expected hashes are SPEC §13.1. A mismatch is fatal; do not
   attempt to "work around" it.

2. **Clone the pilot at T₀.**
   ```
   mkdir -p benchmarks/provbench/work
   git clone https://github.com/BurntSushi/ripgrep \
     benchmarks/provbench/work/ripgrep
   git -C benchmarks/provbench/work/ripgrep checkout \
     af6b6c543b224d348a8876f0c06245d9ea7929c5
   ```

3. **Run the labeler.**
   ```
   cargo run --release --manifest-path benchmarks/provbench/labeler/Cargo.toml -- run \
     --repo benchmarks/provbench/work/ripgrep \
     --t0 af6b6c543b224d348a8876f0c06245d9ea7929c5 \
     --out benchmarks/provbench/corpus/ripgrep-af6b6c54-$(git rev-parse --short HEAD).jsonl
   ```
   Output is JSONL, one `fact_at_commit` row per line, sorted
   `(fact_id, commit_sha)`. Every row carries `labeler_git_sha` matching
   the labeler commit at run time.

4. **Determinism check.**
   ```
   cargo test --release --manifest-path benchmarks/provbench/labeler/Cargo.toml --test determinism
   ```
   And on the real corpus:
   ```
   <re-run step 3 with --out file2>
   diff <file1> <file2>
   ```
   The diff must be empty.

5. **Spot-check sampling.**
   ```
   cargo run --manifest-path benchmarks/provbench/labeler/Cargo.toml -- spotcheck \
     --corpus benchmarks/provbench/corpus/<file>.jsonl \
     --out benchmarks/provbench/spotcheck/sample-$(git rev-parse --short HEAD).csv
   ```
   The CSV columns are: `fact_id,commit_sha,bucket,predicted_label,human_label,disagreement_notes`.
   On success the CLI echoes the resolved sampling seed
   (`wrote 200 samples to … (seed=0x…)`); the same seed value is also
   persisted in a sibling `<out>.meta.json` so the artifact under
   `spotcheck/` is self-describing.

   **Seed selection.** Omit `--seed` to use the historical default
   (`DEFAULT_SEED = 0xC0DEBABEDEADBEEF`) so a reviewer can resume an
   in-progress CSV deterministically. The SPEC §9.1 acceptance-gate
   draw **must** use the default seed. Supply `--seed <u64>` only for
   post-merge / anti-tuning validation runs against a freshly
   regenerated corpus, and never re-run with a different seed against
   an already partially-filled CSV — the row order will shift and the
   `human_label` column will misalign.

   Open the CSV and fill the `human_label` column for each of the 200
   rows. Save and run:
   ```
   cargo run --manifest-path benchmarks/provbench/labeler/Cargo.toml -- report \
     --csv benchmarks/provbench/spotcheck/sample-<sha>.csv
   ```
   The report prints the point estimate and Wilson 95% lower bound, and
   appends a Markdown summary under `benchmarks/provbench/spotcheck/`.

## Acceptance gate (SPEC §9.1)

Phase 0b is accepted iff:
- The point estimate of spot-check agreement is **≥95%**.
- The Wilson 95% lower bound is reported (informational; not part of
  the gate but required by §9.1).
- The determinism check is byte-identical.

## Behavior

- **Visibility narrowing classified as source changed.** Public symbols
  whose visibility is narrowed to `pub(crate)`, `pub(super)`,
  `pub(in path)`, or private are classified as `StaleSourceChanged` rather
  than `NeedsRevalidation`. Per SPEC §5 rule ordering, a narrowing
  represents a semantic change to the public API surface and is therefore
  treated as a source change, not merely a revalidation trigger.

- **Replay symbol resolution is commit-tree-local.** For each commit a
  `CommitSymbolIndex` is built from that commit's `.rs` tree (via
  tree-sitter) before any fact is classified; `rust-analyzer` is no longer
  consulted at replay time. The index also covers `.py` paths via
  `python_entries`; Python facts use `lookup_python` which returns a
  4-variant enum (`ExactAtOriginalPath` / `UniqueFallbackAtPath` /
  `AmbiguousFallback` / `Absent`). Rust facts continue to use
  `symbol_exists_in_tree` (bool). Per-fact-kind: `PublicSymbol` respects a
  single-underscore-leaf filter applied symmetrically in index building and
  matching — a leaf name beginning with `_` (e.g. `_foo`) does NOT continue
  a public `foo` across commits. This eliminates the runtime RA dependency
  for the hot classification path. Live RA tooling and the `PINNED_BINARIES`
  tooling-pin table (in `src/tooling.rs`) remain in the crate for
  `tests/replay_ra.rs` (pinned-binary test) and for future cross-crate /
  macro-expanded work. Files added after T₀ are included in the per-commit
  index, so same-qualified symbols moved into new files route to
  `NeedsRevalidation` rather than `StaleSourceDeleted`.

- **Rename detection requires AST context match.** A `RenameCandidate` is a
  typed struct carrying `container` (the enclosing module or impl block)
  along with `qualified_name`, `leaf_name`, and `span`. Same-kind filtering
  is enforced upstream by `rename_candidates_for_typed`, which only collects
  candidates from the original fact's kind. A candidate is only promoted to
  a rename when the container matches the original symbol *and* the
  candidate was not already present at T₀ — the structural T₀-presence
  check prevents false positives from pre-existing symbols with the same
  name.

- **DocClaim matching is relocation-tolerant.** The post-state doc-claim
  lookup searches by `qualified_name` rather than by byte-offset hash.
  A claim that moves to a different line or section in a later commit is
  still matched correctly as long as the qualified name is preserved.

- **TestAssertion matching is disambiguated by ordinal within `test_fn`.**
  `test_assertion::extract` emits one `Fact::TestAssertion` per
  `assert!`/`assert_eq!`/`assert_ne!` invocation, so a test fn with N
  assertions produces N facts at T₀. Post-commit pairing is by
  `(test_fn, zero-based ordinal in tree-sitter walk order)` — not by
  `test_fn` alone — so a body-modified assertion at the same ordinal
  classifies `StaleSourceChanged` and unchanged siblings stay `Valid`.
  When the post-commit test fn has fewer assertions than the T₀
  ordinal (deleted-tail), `matching_post_fact` returns `None`: with
  `commit_index` present the row routes to `NeedsRevalidation` if
  the test fn name still exists in the tree, `StaleSourceDeleted` if
  it is gone wholesale.

  **Known limitation:** ordinal pairing is fragile to "insertion above"
  edits. If a future commit inserts a new assertion *before* an
  existing one in the same `test_fn`, every subsequent T₀ assertion's
  ordinal mismatches its post-commit counterpart by one, causing
  unchanged assertions at the new index to misclassify as
  `StaleSourceChanged` (and the inserted assertion to be invisible to
  any T₀ fact). A hybrid neighborhood/hash matcher is the pass-5+ path
  for this edge case; pass-4 ships pure ordinal because the dominant
  failure mode the labeler must catch is "assertion #N's text changed",
  which ordinal pairing handles correctly. See
  `benchmarks/provbench/spotcheck/2026-05-12-post-pass3-findings.md`
  for the full SPEC §5 analysis that motivated the choice.

- **Byte-identical source files short-circuit to `Valid`.** SPEC §5
  structural invariant: when a fact's source path is byte-identical
  between T₀ and the replay commit, the labeler classifies every fact
  at that path as `Valid` before per-fact matching is invoked. The
  guardrail covers all five fact kinds (including `DocClaim` on
  byte-identical markdown) and is computed once per `(path, commit)`,
  visibly bypassing `matching_post_fact`,
  `CommitSymbolIndex::symbol_exists_in_tree`, rename detection, and
  whitespace/comment diffing.

  **Defense-in-depth rationale:** per-fact matchers should
  independently uphold "unchanged file ⇒ Valid for every fact at that
  path", but a structural guardrail makes the invariant a property of
  the labeler rather than a property only of well-behaved matchers.
  Pass-3's spot-check surfaced one such matcher-correctness gap as a
  `FunctionSignature::is_hidden` byte-identical violation; the
  guardrail covers that class structurally without requiring each
  matcher to be perfect. See
  `benchmarks/provbench/spotcheck/2026-05-12-post-pass3-findings.md`.

- **FunctionSignature matching is disambiguated by `(qualified_name, cfg_attribute_set, impl_receiver_type)`.**
  `function_signature::extract` emits one `Fact::FunctionSignature`
  per `fn` declaration; when the same `qualified_name` appears in
  multiple `#[cfg(...)]` variants or across multiple `impl` blocks,
  pass-5 disambiguates post-commit matching by the normalized cfg
  attribute set plus the enclosing impl receiver type, with a zero-
  based ordinal as a within-key tiebreaker for genuine duplicates.
  The disambiguator lives on the private replay-internal
  `ObservedFact`; `Fact` enum + `fact_id` format are unchanged.
  Pre-pass-5, a deleted cfg variant could mis-pair against a surviving
  variant's span/hash → `StaleSourceChanged`; pass-5 routes such
  deletions to `NeedsRevalidation` when a same-qualified-name survivor
  exists in `CommitSymbolIndex` (or `StaleSourceDeleted` when none
  does). See `benchmarks/provbench/spotcheck/2026-05-13-post-pass4-findings.md`.

- **PublicSymbol bare `pub use` re-exports preserve public surface continuity.**
  When a T₀ `pub fn X` (or `pub struct X`, `pub trait X`, etc.) is
  replaced at a later commit by `pub use path::X;` or
  `pub use path::Original as X;`, the exported name `X` is still
  publicly available from the crate even though the underlying form
  changed. Pass-5 classifies these as `Valid`. Restricted-visibility
  uses (`pub(crate) use`, `pub(super) use`, `pub(in path) use`,
  plain `use`) and glob re-exports (`pub use path::*;`) do NOT take
  this path; they fall through to the existing pass-3 narrowing
  logic or the absent-symbol logic and continue to classify as
  `StaleSourceChanged` / `StaleSourceDeleted`. See
  `benchmarks/provbench/spotcheck/2026-05-13-post-pass4-findings.md`.

- **Field same-file same-leaf moves route to `NeedsRevalidation`.**
  When a T₀ field's exact `qualified_path` (e.g. `Config::dfa_size_limit`)
  no longer resolves at a later commit but the same leaf name appears
  in another struct or enum-struct variant in the SAME file (e.g.
  `ConfigInner::dfa_size_limit` after a nesting refactor, or
  `SinkContext::Match::kind` after a struct-to-enum conversion), the
  labeler routes the row to `NeedsRevalidation`. Same-leaf-different-
  container is the gray area the SPEC §5 label set reserves for LLM
  follow-up review. Cross-file field-leaf tracking is intentionally
  not extended into `CommitSymbolIndex` — cross-file matching of bare
  leaf names like `kind` / `name` / `id` / `path` collides across
  hundreds of unrelated structs. See
  `benchmarks/provbench/spotcheck/2026-05-13-post-pass4-findings.md`.

- **Spot-check seed is configurable but defaults are deterministic.**
  The stratified sampler is seeded by `DEFAULT_SEED`
  (`0xC0DEBABEDEADBEEF`) unless `--seed <u64>` is supplied. Re-running
  `spotcheck` with the same seed and the same corpus is byte-identical,
  which is what makes the human-review CSV resumable across sessions.
  A non-default seed is intended for post-merge / anti-tuning runs only
  and must NOT be used for the SPEC §9.1 acceptance gate; the resolved
  seed is echoed to stdout and persisted in `<out>.meta.json` so the
  on-disk artifact is self-describing.

The labeler is **fail-closed** by design. Silently producing labels in any
of the following situations would corrupt the corpus, so each surfaces as
an error and aborts the run:

- **Tooling-pin mismatch.** `verify-tooling` calls
  `tooling::resolve_from_env()`, which hard-fails when a binary on `PATH`
  (or at the documented fallback) does not match the SHA-256 in
  `PINNED_BINARIES` for the current platform. Distros patch — version
  strings are not enough. Replay itself no longer calls rust-analyzer.

  **Local-dev override:** if your rustup-managed `rust-analyzer` drifts
  from the SPEC §13.1 pin (rustup auto-updates routinely move it),
  install the pinned `1.85.0 (4d91de4e 2025-02-17)` binary
  side-by-side and point the labeler at it via the
  `PROVBENCH_RUST_ANALYZER` env var. The matching recipe depends on
  which row of `PINNED_BINARIES` your host falls under, because the
  two pinned hashes correspond to **different upstream artifacts**
  (rustup component on macOS, GitHub release `.gz` on Linux).

  **macOS aarch64** — pinned hash is the rustup
  `1.85.0-aarch64-apple-darwin` component. The GitHub release `.gz`
  does NOT match this hash; use rustup:

  ```bash
  rustup toolchain install 1.85.0 --component rust-analyzer
  RA=$(rustup which --toolchain 1.85.0 rust-analyzer)
  shasum -a 256 "$RA"   # must print f85740bf…0e1f9aee
  export PROVBENCH_RUST_ANALYZER="$RA"
  ```

  Installing the `1.85.0` toolchain leaves your active toolchain
  untouched, so your IDE keeps using whatever rust-analyzer it had.

  **Linux x86_64** — pinned hash is the decompressed
  `rust-analyzer-x86_64-unknown-linux-gnu.gz` from the upstream
  `2025-02-17` GitHub release; rustup builds for Linux currently
  differ. Use the GitHub artifact:

  ```bash
  curl -L -o /tmp/ra.gz \
    https://github.com/rust-lang/rust-analyzer/releases/download/2025-02-17/rust-analyzer-x86_64-unknown-linux-gnu.gz
  gunzip -f /tmp/ra.gz && chmod +x /tmp/ra
  shasum -a 256 /tmp/ra   # must print e7a85d27…65f7410
  mkdir -p $HOME/.local/provbench && mv /tmp/ra $HOME/.local/provbench/rust-analyzer
  export PROVBENCH_RUST_ANALYZER=$HOME/.local/provbench/rust-analyzer
  ```

  Resolution priority for both overrides
  (`PROVBENCH_RUST_ANALYZER`, `PROVBENCH_TREE_SITTER`): env var →
  `PATH` → documented fallback. The override moves the discovery
  point only; the resolved binary's bytes are still hash-checked
  against the SPEC §13.1 freeze record. There is no way to bypass
  the freeze via this knob.
- **Invalid UTF-8 in markdown.** The doc-claim extractor refuses to
  silently produce zero facts on a corrupted README; it returns `Err`
  with the offending file path so reviewers can locate the bad blob.
- **Malformed git SHA.** `validate_sha_hex` rejects anything that isn't
  exactly 40 lowercase hex characters before that string is passed to
  `git ls-tree`, so a malformed value cannot reach a subprocess argv.
- **Cross-platform `fact_id`s.** Source paths are normalized to forward
  slashes via a pure-string transform (`normalize_path_for_fact_id`)
  before they enter a `fact_id`. The single `Path::canonicalize` call in
  the labeler runs once, on the repo root, in `Pilot::open`. No absolute
  filesystem path can leak into a `fact_id` regardless of `pwd`.

The spot-check CSV is written via the `csv` crate, not hand-rolled
formatting, so a stray comma or newline in a `disagreement_notes` cell
round-trips through reader/writer correctly.

## Phase 0c artifacts (`emit-facts`, `emit-diffs`)

Phase 0c's LLM-as-invalidator baseline (under
`benchmarks/provbench/baseline/`) consumes two JSON artifact sets derived
from a frozen labeler corpus. Both subcommands are read-only against the
pilot repo and idempotent against the corpus JSONL — re-running them
produces byte-identical output for a given `(corpus, repo, t0)` triple.

### `emit-facts`

Emit one T₀ fact-body row per unique `fact_id` referenced in the corpus,
written as JSONL sorted by `fact_id`. Feeds the baseline runner's
prompt assembly (SPEC §6.1).

Arguments:
- `--corpus <path>` — labeler corpus JSONL (`Run` output). Only the
  `fact_id` column is consulted.
- `--repo <path>` — local path to the cloned pilot repo at T₀.
- `--t0 <sha>` — T₀ commit SHA (40-char lowercase hex).
- `--out <path>` — output JSONL path (one `FactBodyRow` per line).

Example (ripgrep pilot):

```
cargo run --release --manifest-path benchmarks/provbench/labeler/Cargo.toml -- emit-facts \
  --corpus benchmarks/provbench/corpus/<run>.jsonl \
  --repo   benchmarks/provbench/work/ripgrep \
  --t0     af6b6c543b224d348a8876f0c06245d9ea7929c5 \
  --out    benchmarks/provbench/baseline/work/facts.jsonl
```

### `emit-diffs`

Emit one `<commit_sha>.json` artifact per distinct `commit_sha` in the
corpus. Each artifact contains either a `unified_diff` (full file
context per SPEC §6.1) or an `excluded` reason (`"t0"` for the T₀ commit
itself, `"no_parent"` for root commits without a parent).

Arguments:
- `--corpus <path>` — labeler corpus JSONL (`Run` output). Only the
  `commit_sha` column is consulted.
- `--repo <path>` — local path to the cloned pilot repo.
- `--t0 <sha>` — T₀ commit SHA (40-char lowercase hex). Used to mark
  the T₀ commit's artifact as `excluded: "t0"`.
- `--out-dir <path>` — output directory (one `<commit_sha>.json` per
  distinct commit).

Example (ripgrep pilot):

```
cargo run --release --manifest-path benchmarks/provbench/labeler/Cargo.toml -- emit-diffs \
  --corpus  benchmarks/provbench/corpus/<run>.jsonl \
  --repo    benchmarks/provbench/work/ripgrep \
  --t0      af6b6c543b224d348a8876f0c06245d9ea7929c5 \
  --out-dir benchmarks/provbench/baseline/work/diffs
```

Both outputs feed the Phase 0c baseline runner under
`benchmarks/provbench/baseline/` (the runner's `sample` subcommand takes
`--facts <out.jsonl>` and `--diffs-dir <out-dir>` directly).

## Limitations

- **Python post-commit classification is implemented** (Plan A.2 / SPEC §11
  row 2026-05-19). The held-out Python repo (`flask`) is exercised by this
  labeler via `classify_python_against_commit` + `matching_post_fact_python`
  + `CommitSymbolIndex::python_entries`. The prior claim that "v1 supports
  Rust only" is retracted. Remaining gaps:
  - **Python `DocClaim` is a stub** — the `DocClaim` arm in
    `matching_post_fact_python` returns `None` unconditionally. Full Python
    doc-claim classification is v1.4+ work.
  - **Python `FunctionSignature` disambiguator not implemented** — the
    cfg-attribute + impl-receiver tiebreaker used for Rust (pass-5) has no
    Python equivalent. YAGNI; add only when fixtures surface genuine
    duplicates.
  - **Python same-file `Field` leaf-elsewhere pre-check intentionally
    omitted** — the Rust path uses a `::` qualified-name assumption to scope
    the same-leaf search to the enclosing file; this assumption does not
    translate cleanly to Python's `.` separator, so the pre-check is absent
    for Python `Field` facts.
  - **Cross-file Python moves** resolve via `lookup_python`'s 4-variant
    fallback (`ExactAtOriginalPath` → `UniqueFallbackAtPath` →
    `AmbiguousFallback` → `Absent`). A symbol that is unique across the
    post-commit tree but has no body-hash match routes to
    `NeedsRevalidation` rather than `Stale_Symbol_Renamed`.
- Per-commit classification uses a tree-sitter-built `CommitSymbolIndex`
  rather than a live `rust-analyzer` query. This loses semantic resolution
  for cross-crate references and macro-expanded symbols. The pinned RA
  binary is still covered by `verify-tooling` and `tests/replay_ra.rs` for
  future work that needs deeper semantic resolution.

## Reproducibility / supported platforms

The pinned-binary table in `src/tooling.rs` covers exactly two platforms:

| Platform                  | rust-analyzer install path                                   | tree-sitter install path                              |
|---------------------------|--------------------------------------------------------------|-------------------------------------------------------|
| `aarch64-darwin` (macOS)  | rustup `stable-aarch64-apple-darwin` component               | Homebrew (`brew install tree-sitter`)                 |
| `x86_64-linux-gnu` (CI)   | Decompressed `rust-analyzer-x86_64-unknown-linux-gnu.gz`     | Decompressed `tree-sitter-linux-x64.gz`               |

For `x86_64-linux-gnu` (the `ubuntu-latest` GitHub runner), the pinned
hashes correspond to the **decompressed** binaries published as `.gz`
artifacts on each tool's GitHub release. CI must install the tools by
downloading the upstream `.gz`, gunzipping, and placing the resulting
binary on `PATH` (or at the documented fallback under `/usr/local/bin/`).
Installs via `apt`, `snap`, or `rustup` may produce different on-disk
bytes and **will fail hash verification**.

Upstream artifact URLs for the Linux pin:

- `rust-analyzer` 1.85.0:
  `https://github.com/rust-lang/rust-analyzer/releases/download/2025-02-17/rust-analyzer-x86_64-unknown-linux-gnu.gz`
- `tree-sitter` 0.25.6:
  `https://github.com/tree-sitter/tree-sitter/releases/download/v0.25.6/tree-sitter-linux-x64.gz`

`x86_64-darwin` (Intel Mac) and `aarch64-linux` (e.g. ARM CI runners)
are explicitly **out of scope** for this hardening pass. Adding them
requires running `shasum -a 256` against the decompressed upstream
artifact and committing the result to `PINNED_BINARIES` — never copy a
hash from a secondary source.

Phase 0b tooling verification remains valid only on a supported
platform. `resolve_from_env()` hard-fails on any other host.

## Python support (v1.2b)

The labeler accepts Python repos in addition to Rust. Python parsing
uses `tree-sitter-python 0.25` (SPEC §13.1 pin); symbol resolution is a
tree-sitter scope walker + lexical import graph — no Python runtime
required. Same fact schema as the Rust path: `FunctionSignature`,
`Field`, `PublicSymbol`, `TestAssertion` (`DocClaim` is currently a
stub for Python; see `src/facts/python/doc_claim.rs`).

Usage:
```
provbench-labeler run         --repo path/to/python/repo --t0 <sha> --out corpus.jsonl
provbench-labeler emit-facts  --repo path/to/python/repo --t0 <sha> --out facts.jsonl
provbench-labeler emit-diffs  --repo path/to/python/repo --t0 <sha> --out-dir diffs/
provbench-labeler spotcheck   --lang python --corpus corpus.jsonl --out spotcheck.csv \
                              --n 200 --seed 0xC0DEBABEDEADBEEF
```

Path dispatch is by file extension via the `Language` enum
(`src/lang.rs`): `.rs` → Rust path, `.py` → Python path, anything else
is ignored as a source file (`.md` is still handled by the doc-claim
path independently).

Known coverage limitations (recorded in commit messages + held-out
findings docs):

- `__init__.py` collapse not implemented. A package's `__init__` is
  treated as the module `package.__init__`. Real Python packages with
  sparse `__init__.py` re-exports (e.g. flask's `from .app import Flask`)
  will under-resolve through `PythonResolver`.
- Multi-hop import chains capped at one hop in the resolver.
- Relative imports (`from . import X`) are punted.
- Star imports (`from X import *`) are skipped unless `__all__` is
  defined (and the resolver does not currently parse `__all__`).
- `TYPE_CHECKING`-conditional imports, dynamic dispatch, and metaclass
  attribute generation are not modeled.

Determinism is enforced by:

- `tests/determinism_python.rs` — fixture-level (default-run, ~0.3s)
- `tests/determinism_flask.rs` — full flask (`#[ignore]`; opt-in via
  `cargo test -- --ignored`). Pallets/flask @ T₀ `2f0c62f5`.

Spot-check material for the v1.2b held-out round lives at
`../results/python-labeler-2026-05-15-spotcheck.csv` and the
companion findings template `python-labeler-2026-05-15-spotcheck-findings.md`.

### Plan A.2 — Post-commit classification machinery (2026-05-19)

Plan A.2 extends Python support from T₀ extraction only (v1.2b) to full
per-commit classification. The Plan A.1 short-circuit that routed every
changed Python file directly to `NeedsRevalidation` (`is_python_path`
guard in the replay loop) **has been removed**.

**Dispatch.** `replay::classify_commit` detects `PostAst::Python(_)` and
routes to `classify_python_against_commit` instead of the Rust path.
The byte-identical short-circuit (valid for all paths) runs first and
bypasses per-fact matching as before.

**`matching_post_fact_python` — 5 arms:**

| Fact kind | Post-commit behaviour |
|---|---|
| `FunctionSignature` | Body-hash compare + cross-file move via `lookup_python` 4-variant fallback |
| `Field` | Exact `qualified_path` lookup; absent → `NeedsRevalidation` (same-file leaf-elsewhere pre-check not implemented for Python) |
| `PublicSymbol` | `CommitSymbolIndex::lookup_python`; single-underscore-leaf filter applied (`_foo` does NOT continue `foo`) |
| `TestAssertion` | Ordinal end-to-end via `push_test_assertion_facts`; same ordinal pairing as Rust path |
| `DocClaim` | **Stub — returns `None`** (Python DocClaim is v1.4+ work) |

**`PublicSymbol` single-underscore-leaf filter.** A public `foo` at T₀ is
NOT matched by a post-commit `_foo`. The filter is applied symmetrically
in `CommitSymbolIndex` index building (Python entries) and in
`matching_post_fact_python` matching, so the invariant holds for both
directions.

**Cross-file move resolution (`lookup_python`).** Resolution proceeds
through 4 variants in order:
1. `ExactAtOriginalPath` — qualified name found at the original file path.
2. `UniqueFallbackAtPath` — qualified name not at original path but unique
   across the full post-commit tree; promotes to the new path.
3. `AmbiguousFallback` — name found at multiple paths; routes to
   `NeedsRevalidation`.
4. `Absent` — name not found anywhere; routes to `StaleSourceDeleted` (or
   `NeedsRevalidation` if the test fn still exists, for `TestAssertion`).

Unique-but-no-body-hash cases (symbol present but body changed) route to
`NeedsRevalidation` rather than `Stale_Symbol_Renamed`.

**`RenameCandidate::new_python`.** Uses `.` splitting for qualified-name
decomposition. Rust's `::` splitter is not used for Python candidates.

**Flask H3 (R3 dominance) closeable.** The v1.2b held-out findings noted
R3 dominance as a labeler-layer concern. Plan A.2's full per-commit
classification closes the H3 finding at the labeler layer; the held-out
re-run confirming this is the follow-up PR.

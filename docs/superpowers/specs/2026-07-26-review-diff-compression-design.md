# Review Diff Compression Design

**Issue:** #228 — `perf(collab): compress review diffs with selective expansion`

## Goal

Reduce repeated review-turn diff ingestion without removing a reviewer's ability
to inspect every changed file or hunk. Compression is opt-in; the existing raw
`git diff` behavior remains the safe fallback.

## Scope

The change covers both paths that repeatedly load a branch diff during collab
review:

- Codex's `CodeReviewFixGlobalPending` global-review prompt.
- Claude's `CodeReviewLocalPending` local-review prompt, including the shared
  `/ultrareview-local` workflow it invokes.

It does not change MCP tool-result serialization, failure-report serialization,
or the underlying review finding protocol.

## Chosen Approach

Introduce a feature-gated `ironmem review-diff` CLI command. When IronMEM is
built with `headroom-compression`, the command reads a deterministic Git range
and produces a compact review artifact. The artifact has:

1. Range metadata (`base`, `head`, and original size).
2. A complete file and hunk index, with stable selectors.
3. A Headroom-compressed unified diff body.
4. Exact expansion instructions for each indexed file and hunk.

`--expand-file <path>` and `--expand-hunk <path>:<ordinal>` recompute the same
range and return the requested original unified-diff material. They never expand
the lossy compressed text. This makes every indexed item discoverable even when
the compressor drops a file or hunk from its compact body.

The command exists in all builds so prompts can call it consistently. Builds
without `headroom-compression`, unsupported ranges, and unresolved selectors
return a clear non-zero result. Prompt wiring treats any such result as a
signal to use the current full `git diff` command unchanged.

## Alternatives Considered

### Prompt-only diff trimming

This would be cheap to add but would bypass the reviewed Headroom dependency,
provide no stable hunk selectors, and leave discoverability dependent on
ad-hoc reviewer instructions. It does not meet the issue's artifact contract.

### Persisted expansion cache

A cache could avoid recomputing a range for each expansion, but it introduces
expiry, cleanup, and multi-worktree correctness concerns. Git can reproduce the
source range deterministically, so a cache is unnecessary for this scoped
feature.

## Data Flow

```text
base SHA + head SHA
        |
        v
ironmem review-diff
        |
        +--> parse source unified diff --> complete file/hunk selector index
        |
        +--> Headroom DiffCompressor --> compact body
        |
        v
review prompt receives index + compact body
        |
        +--> needs detail? --> review-diff --expand-file / --expand-hunk
        |
        +--> command unavailable/error? --> existing git diff fallback
```

## Prompt Wiring

The global and local review prompts first attempt the command only when
compression is enabled in the installed binary. They inject the compact
artifact into the review turn and direct reviewers to use the stable selectors
for details. A command failure must not block review or hide source: the next
step is the exact existing full-diff command for that range.

The `/ultrareview-local` instructions preserve its existing reviewer contract:
review agents still inspect the source range themselves. The compact artifact
only replaces repeated broad diff ingestion and gives each agent a narrow,
explicit expansion path.

## Verification and Measurement

Tests will cover:

- compression output contains every file and hunk selector from a representative
  multi-file fixture;
- file and hunk expansion return the original source material;
- disabled-feature and unresolved-expansion failures select the raw-diff
  fallback;
- all affected prompt files contain the artifact-first, fallback-safe contract;
- a representative fixture reports fewer review-ingested bytes for the compact
  artifact than for the raw diff, using the existing collab report/metrics
  surface.

The final validation runs the feature-enabled Rust tests plus the project's
standard formatting, Clippy, and workspace-test gates.

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

### Body hunk tags

Added with the move of the `headroom-core` pin to
`74b7ac23861944f1411d30c5394043ec0505b334`. At that revision the compressor
splits a source hunk wherever it trims context and renumbers each piece the way
`git diff -U2` would. At the previous pin (`5bd2266f`) a trimmed hunk kept its
source header, so index and body lined up; now a file with four index hunks can
have seven body hunks, none of whose headers appear in the index, and "the third
body hunk" is not index hunk 3.

Each body hunk header therefore carries the selector of the index hunk it was
cut from, in square brackets after the closing `@@` and before any section text
Git wrote there:

```text
index:  src/lib.rs#hunk-1: @@ -7,19 +7,19 @@ fn main() {
body:   @@ -8,5 +8,5 @@ [src/lib.rs#hunk-1] fn main() {
        @@ -14,5 +14,5 @@ [src/lib.rs#hunk-1] fn main() {
```

- The tag is the index selector byte for byte, the same text that hunk's index
  entry starts with. It is the index's rendering of the path, not Git's.
  The index quotes and escapes a path only when it holds a control character,
  `"` or `\` (`["line\nbreak.txt"#hunk-1]`); a path Git quotes for any other
  reason is written as it is, so `é.txt`, which Git writes as
  `"\303\251.txt"`, is tagged `[é.txt#hunk-1]`. The tag is always the first
  thing after the closing `@@`.
- The tag always names the file section the hunk sits in, and that file is
  the index file whose source section opens with the same `diff --git` line,
  which the compressor writes as it read it. Nothing under that line is used
  to name the file: the compressor does not read a quoted
  `diff --git "a/..."` line as a file header, so it prints the quoted file's
  `similarity` and `rename` lines directly under the `diff --git` line of the
  file before it.
- A header is tagged only when exactly one index hunk of that file holds the
  body hunk's lines at the body hunk's line numbers. Line numbers alone are
  not enough: the compressor counts a quoted file's hunks as hunks of the
  file before it, and when those are together more than it keeps it can
  leave the quoted file's hunk inside that file's section, where the hunk
  fits the wrong file's ranges.
- Tagging reads each source file once, when its section opens in the body, and
  finds a body hunk's place in it by the hunk's first old and new line
  numbers, so the work is linear in the size of the source plus the body.
- Anything else is left exactly as the compressor wrote it. An untagged header
  means no source hunk was found, never a guess. Untagged cases: a hunk filed
  under the wrong file as above, a path the index lists twice (a type change is
  a deletion and a creation of one path, under one `diff --git` line), and
  combined-diff sections
  (`diff --cc`, `@@@`), which the index does not parse and neither supported
  source (`<base>...<head>`, `HEAD`) produces.
- Git's own section text can start with `[` (a CSS attribute selector under the
  `css` diff driver), so `@@ -1,3 +1,3 @@ [hidden] {` can be an untagged
  header. A tag is recognisable because it is a selector the index lists.
- Tags are part of the rendered artifact, so the "artifact must be smaller than
  the source" check counts them.

The command exists in all builds so prompts can call it consistently. Builds
without `headroom-compression`, unsupported ranges, and unresolved selectors
return a clear non-zero result. Prompt wiring treats any such result as a
signal to use the current full `git diff` command unchanged.

## Alternatives Considered

### Prompt-only diff trimming

This would be cheap to add but would bypass the pinned Headroom dependency,
provide no stable hunk selectors, and leave discoverability dependent on
ad-hoc reviewer instructions. It does not meet the issue's artifact contract.

This document first called it "the reviewed Headroom dependency". That
described the revision pinned when it was written, `5bd2266f`. The pin has
since moved to `74b7ac23861944f1411d30c5394043ec0505b334` so the workspace
could take `rusqlite` 0.41, and the upstream commits between the two revisions
have not been audited: the dependency is pinned, not reviewed. The move also
added `tree-sitter`, eight grammar crates and `tree-sitter-language` to the
feature build.

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
- every body hunk header carries the selector of the index hunk that holds its
  lines, including when the compressor splits one source hunk into several, and
  a hunk with no such index hunk stays untagged;
- file and hunk expansion return the original source material;
- disabled-feature and unresolved-expansion failures select the raw-diff
  fallback;
- all affected prompt files contain the artifact-first, fallback-safe contract;
- a representative fixture reports fewer review-ingested bytes for the compact
  artifact than for the raw diff, using the existing collab report/metrics
  surface.

The final validation runs the feature-enabled Rust tests plus the project's
standard formatting, Clippy, and workspace-test gates. CI runs the
feature-enabled review-diff tests on every pull request that targets `main` or
`master` and on every push to those branches, because the compressed body
comes from the pinned dependency and a pin move can change it. Those tests
assert the hunk headers and tags, file sections and changed lines of their
fixtures' bodies, so they fail on a change to that content, not on every
change to the compressor's output.

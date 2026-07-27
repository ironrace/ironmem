# Review Diff Compression Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `subagent-driven-development` (recommended) or `executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in, deterministic review-diff artifact with selective file/hunk expansion, wire it into both collab review paths, and preserve the raw-diff fallback.

**Architecture:** A new `review_diff` library module obtains a Git range, indexes every source file and hunk, and uses `headroom_core::transforms::DiffCompressor` only when the `headroom-compression` feature is compiled. The CLI renders that artifact or an exact source expansion. The collab prompts attempt this artifact first and retain their current `git diff`/`gh pr diff` commands on every error path.

**Tech Stack:** Rust 2021, Clap 4, `headroom-core` optional feature, Python `pytest` template checks, existing `ironmem report`/`scripts/collab_baseline.py` metrics workflow.

---

## File Structure

- Create: `crates/ironmem/src/review_diff.rs` — deterministic Git-source reader, unified-diff indexer, artifact renderer, expansion renderer, and before/after size metrics.
- Modify: `crates/ironmem/src/lib.rs` — export the new review-diff module.
- Modify: `crates/ironmem/src/main.rs` — expose `ironmem review-diff` with range/worktree and expansion flags.
- Create: `crates/ironmem/tests/review_diff.rs` — real temporary-Git-repository integration coverage for artifacts, expansion, and feature-off fallback.
- Modify: `.codex-plugin/prompts/collab-global-review.md` — artifact-first global review with a raw branch-diff fallback.
- Modify: `.claude-plugin/prompts/collab-turn-review-local.md` — artifact-first local collab review with a raw branch-diff fallback.
- Modify: `.claude-plugin/commands/ultrareview-local.md` — artifact-first PR and worktree modes with existing `gh`/Git fallbacks.
- Modify: `docs/COLLAB.md` — document the opt-in artifact and reviewer expansion contract.
- Modify: `README.md` and `docs/CODEX.md` — document feature-enabled build/install and `review-diff` invocation.
- Modify: `tests/collab_turn_templates/test_lint.py` and `scripts/check_collab_turn_templates.py` — require the artifact-first/fallback-safe wording on both review prompt surfaces.
- Modify: `scripts/collab_baseline.py` and `scripts/test_collab_baseline.py` — accept and record generated review-artifact byte/token profiles alongside the existing public `ironmem report --json` baseline.

### Task 1: Build the deterministic artifact library

**Files:**
- Create: `crates/ironmem/src/review_diff.rs`
- Modify: `crates/ironmem/src/lib.rs`
- Test: `crates/ironmem/tests/review_diff.rs`

- [ ] **Step 1: Write the failing artifact and expansion tests**

Create a temporary Git repository with a committed base and a changed head containing more than 20 hunks across at least two files. Add feature-gated tests that exercise the public library API and assert every source selector remains available even when the compressed body drops hunks:

```rust
#[cfg(feature = "headroom-compression")]
#[test]
fn artifact_indexes_every_source_file_and_hunk_while_compressing_the_body() {
    let repo = fixture_repo_with_many_hunks();
    let artifact = build_review_diff(&ReviewDiffRequest::range(
        repo.path(), "HEAD~1", "HEAD",
    )).expect("fixture range should build");

    assert_eq!(artifact.files.len(), 2);
    assert!(artifact.files.iter().all(|file| !file.hunks.is_empty()));
    assert!(artifact.metrics.artifact_bytes < artifact.metrics.source_bytes);
    assert!(artifact.render().contains("Expand file:"));
    assert!(artifact.render().contains("Expand hunk:"));
}

#[cfg(feature = "headroom-compression")]
#[test]
fn expansion_returns_the_original_selected_file_and_hunk() {
    let repo = fixture_repo_with_many_hunks();
    let request = ReviewDiffRequest::range(repo.path(), "HEAD~1", "HEAD");
    let artifact = build_review_diff(&request).unwrap();
    let file = &artifact.files[0];

    let file_patch = expand_review_diff(&request, &file.path, None).unwrap();
    let hunk_patch = expand_review_diff(&request, &file.path, Some(1)).unwrap();

    assert!(file_patch.contains(&format!("diff --git a/{0} b/{0}", file.path)));
    assert!(hunk_patch.contains(&file.hunks[0].header));
    assert!(file_patch.len() > hunk_patch.len());
}

#[test]
fn feature_off_reports_a_clear_unavailable_error() {
    #[cfg(not(feature = "headroom-compression"))]
    assert!(build_review_diff(&ReviewDiffRequest::worktree(".")).unwrap_err()
        .to_string().contains("headroom-compression"));
}
```

- [ ] **Step 2: Run the tests to verify RED**

Run: `cargo test -p ironmem --test review_diff --features headroom-compression`

Expected: compilation fails because `ironmem::review_diff`, `ReviewDiffRequest`, `build_review_diff`, and `expand_review_diff` do not exist.

- [ ] **Step 3: Implement the minimal source/index/artifact API**

Create `review_diff.rs` with immutable request and artifact data. Source acquisition must use argument-vector `Command::new("git")`, never `sh -c`; Git failures return `MemoryError::Validation` with the action and status, not raw shell output. Use the following public API:

```rust
#[derive(Debug, Clone)]
pub enum ReviewDiffSource {
    Range { base: String, head: String },
    Worktree,
}

#[derive(Debug, Clone)]
pub struct ReviewDiffRequest {
    pub repo: std::path::PathBuf,
    pub source: ReviewDiffSource,
}

impl ReviewDiffRequest {
    pub fn range(repo: impl Into<std::path::PathBuf>, base: impl Into<String>, head: impl Into<String>) -> Self;
    pub fn worktree(repo: impl Into<std::path::PathBuf>) -> Self;
}

#[derive(Debug, Clone)]
pub struct ReviewDiffMetrics {
    pub source_bytes: usize,
    pub artifact_bytes: usize,
    pub source_estimated_tokens: usize,
    pub artifact_estimated_tokens: usize,
}

pub fn build_review_diff(request: &ReviewDiffRequest) -> Result<ReviewDiffArtifact, MemoryError>;
pub fn expand_review_diff(request: &ReviewDiffRequest, path: &str, hunk: Option<usize>) -> Result<String, MemoryError>;
```

Read `git diff --no-ext-diff --unified=3 <base>...<head>` for ranges and
`git diff --no-ext-diff --unified=3 HEAD` for worktree mode. Parse `diff --git`
and `@@` boundaries into source-owned `ReviewDiffFile`/`ReviewDiffHunk` values,
number hunk selectors starting at one per file, and retain original patch
segments for expansion. Under `headroom-compression`, render the index plus
`DiffCompressor::default().compress(&source, "review diff")`; otherwise return
`MemoryError::Validation("review-diff compression requires the headroom-compression feature")`.
Compute byte counts from UTF-8 string lengths and estimated tokens with
`(bytes + 3) / 4`; render both before/after values in the artifact footer so
the existing collab report baseline can record the same units. If the fully
rendered artifact (including its index) is not smaller than the source, return
`MemoryError::Validation("review-diff artifact did not reduce ingestion size")`
so the prompt executes its raw-diff fallback instead of paying extra context.

Export it from `lib.rs`:

```rust
/// Deterministic compressed review artifacts with source-preserving expansion.
pub mod review_diff;
```

- [ ] **Step 4: Run the tests to verify GREEN**

Run: `cargo test -p ironmem --test review_diff --features headroom-compression`

Expected: PASS; the representative fixture shows `artifact_bytes < source_bytes`, and file/hunk expansions exactly match the original Git source.

- [ ] **Step 5: Commit the GREEN implementation**

```bash
git add crates/ironmem/src/lib.rs crates/ironmem/src/review_diff.rs crates/ironmem/tests/review_diff.rs
git commit -m "feat: add compressed review diff artifact"
```

### Task 2: Expose the feature-safe CLI contract

**Files:**
- Modify: `crates/ironmem/src/main.rs`
- Test: `crates/ironmem/tests/review_diff.rs`

- [ ] **Step 1: Write the failing CLI tests**

Extend `review_diff.rs` with subprocess tests using `env!("CARGO_BIN_EXE_ironmem")`. One invokes a fixture range and checks the rendered artifact's range, selectors, and metric footer. Another passes `--expand-file <fixture path> --hunk 1` and checks that only the requested hunk is printed. A third invokes a non-feature build only when that build is under test and asserts the stderr mentions `headroom-compression` and exits non-zero.

```rust
let output = Command::new(env!("CARGO_BIN_EXE_ironmem"))
    .current_dir(repo.path())
    .args(["review-diff", "--base", "HEAD~1", "--head", "HEAD"])
    .output()
    .unwrap();
assert!(output.status.success());
assert!(String::from_utf8_lossy(&output.stdout).contains("Review diff artifact"));
```

- [ ] **Step 2: Run the CLI tests to verify RED**

Run: `cargo test -p ironmem --test review_diff --features headroom-compression`

Expected: FAIL because Clap does not recognize `review-diff`.

- [ ] **Step 3: Add `ReviewDiff` to the Clap command and dispatch it**

Add this `Commands` variant in `main.rs`; the group prevents ambiguous source modes and `--hunk` is valid only with `--expand-file`:

```rust
/// Render an opt-in compressed review diff with file/hunk expansion selectors
ReviewDiff {
    #[arg(long, default_value = ".")]
    repo: String,
    #[arg(long, conflicts_with = "worktree")]
    base: Option<String>,
    #[arg(long, requires = "base", conflicts_with = "worktree")]
    head: Option<String>,
    #[arg(long, conflicts_with_all = ["base", "head"])]
    worktree: bool,
    #[arg(long)]
    expand_file: Option<String>,
    #[arg(long, requires = "expand_file")]
    hunk: Option<usize>,
}
```

In `run`, reject every source combination except `--worktree` or a complete
`--base`/`--head` pair with `MemoryError::Validation("review-diff requires --worktree or both --base and --head")`; then construct the corresponding
`ReviewDiffRequest`. Print the artifact for normal mode or `expand_review_diff`
for expansion mode. Do not
create an MCP tool, persist artifacts, or alter generic MCP failure/result
serialization.

- [ ] **Step 4: Run the CLI tests to verify GREEN**

Run: `cargo test -p ironmem --test review_diff --features headroom-compression`

Expected: PASS. Then run:

`cargo test -p ironmem --test review_diff`

Expected: PASS; the unavailable-feature test proves callers get a safe,
actionable fallback signal rather than compressed-looking incomplete output.

- [ ] **Step 5: Commit the CLI contract**

```bash
git add crates/ironmem/src/main.rs crates/ironmem/tests/review_diff.rs
git commit -m "feat: expose review diff compression command"
```

### Task 3: Use the artifact in both review paths and record the measurement

**Files:**
- Modify: `.codex-plugin/prompts/collab-global-review.md`
- Modify: `.claude-plugin/prompts/collab-turn-review-local.md`
- Modify: `.claude-plugin/commands/ultrareview-local.md`
- Modify: `docs/COLLAB.md`
- Modify: `README.md`
- Modify: `docs/CODEX.md`
- Modify: `tests/collab_turn_templates/test_lint.py`
- Modify: `scripts/check_collab_turn_templates.py`
- Modify: `scripts/collab_baseline.py`
- Modify: `scripts/test_collab_baseline.py`

- [ ] **Step 1: Write the failing prompt and baseline tests**

Add a `pytest` test that requires all three review surfaces to contain both a
feature-enabled `ironmem review-diff` invocation and its existing raw-diff
fallback. Add a mutation test that replaces `ironmem review-diff` in the
global-review fixture and expects `scripts/check_collab_turn_templates.py` to
fail with `missing review-diff fallback contract`.

Extend the baseline unit fixture with one generated review artifact and assert
that `build_baseline(...)` preserves its named profile:

```python
baseline = build_baseline(
    report_fixture(),
    session_id="session-1",
    prompt_specs=[],
    review_artifact_specs=[("review_global", artifact_path)],
    captured_at="2026-07-26T12:00:00Z",
    threshold=0.2,
)
assert baseline["review_artifacts"][0]["phase"] == "review_global"
assert baseline["review_artifacts"][0]["artifact_chars"] < baseline["review_artifacts"][0]["source_chars"]
```

- [ ] **Step 2: Run the tests to verify RED**

Run: `python3 -m pytest tests/collab_turn_templates/test_lint.py scripts/test_collab_baseline.py`

Expected: FAIL because neither prompt contract nor `review_artifact_specs` exists.

- [ ] **Step 3: Wire artifact-first review and fallback-safe prompt behavior**

In the Codex global prompt, replace the instruction to read the complete range
with this sequence: run
`ironmem review-diff --repo "$repo_path" --base "$base_sha" --head "$last_head_sha"`;
inject its output only on success; and otherwise run the existing full-range
`git diff` command. In the Claude collab local prompt, use the same range and
fallback before choosing full/reduced audit depth. Tell every reviewer that a
file or hunk is expanded with:

```text
ironmem review-diff --repo "$repo_path" --base "$base_sha" --head "$last_head_sha" --expand-file "<path>" --hunk <ordinal>
```

In `/ultrareview-local`, use the range command for PR mode and
`ironmem review-diff --worktree` for local mode. If either command fails, keep
the exact existing `gh pr diff <N>` or `git diff HEAD` command. Do not remove
the instruction that review agents independently inspect source ranges.

Make the Python linter require those contracts on the three paths. Extend
`collab_baseline.py` with repeatable `--review-artifact PHASE=PATH` input;
parse the artifact's `source_bytes`/`artifact_bytes` footer, reject malformed
or non-improving measurements, and add the profile to the emitted baseline
beside `codex_dispatch_prompts`. This preserves `ironmem report --json` as the
live token source while making a fixture's review-ingestion reduction auditable
in the existing baseline artifact.

Document the feature build command and operational measurement:

```bash
cargo build -p ironmem --features headroom-compression
ironmem review-diff --base origin/main --head HEAD
ironmem report --task <collab-session-id> --json > report.json
python3 scripts/collab_baseline.py capture --session <collab-session-id> \
  --report report.json --review-artifact review_global=artifact.txt --output baseline.json
```

- [ ] **Step 4: Run the prompt, baseline, and documentation checks to verify GREEN**

Run: `python3 -m pytest tests/collab_turn_templates/test_lint.py scripts/test_collab_baseline.py && python3 scripts/check_collab_turn_templates.py && bash scripts/check_versions.sh`

Expected: PASS; the fixture records smaller artifact ingestion while the linter
proves all consumers retain a raw-source fallback.

- [ ] **Step 5: Commit the prompt/docs/metrics surface**

```bash
git add .codex-plugin/prompts/collab-global-review.md \
  .claude-plugin/prompts/collab-turn-review-local.md \
  .claude-plugin/commands/ultrareview-local.md docs/COLLAB.md README.md docs/CODEX.md \
  tests/collab_turn_templates/test_lint.py scripts/check_collab_turn_templates.py \
  scripts/collab_baseline.py scripts/test_collab_baseline.py
git commit -m "perf(collab): compress review diff prompts"
```

### Task 4: Run the complete feature-enabled verification suite

**Files:**
- Verify only: all files changed by Tasks 1–3

- [ ] **Step 1: Verify formatting and feature compilation**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings`

Expected: both commands exit 0.

- [ ] **Step 2: Verify workspace behavior and plugin wiring**

Run: `cargo test --workspace --all-features && bash scripts/check_versions.sh && python3 scripts/mcp_smoke_test.py --binary ./target/debug/ironmem`

Expected: all tests pass; plugin versions remain synchronized; the MCP smoke test succeeds.

- [ ] **Step 3: Inspect the final branch diff**

Run: `git diff --check main...HEAD && git diff --stat main...HEAD && git status --short`

Expected: no whitespace errors; only #228 implementation files plus the intentionally ignored `.worktree-ports.json` are uncommitted.

- [ ] **Step 4: Commit any verification-only correction**

If a check requires a source correction, add a focused regression test first, run it RED, implement the correction, rerun GREEN, and commit it as:

```bash
git commit -m "fix: address review diff verification finding"
```

No commit is created when every preceding task is already green.

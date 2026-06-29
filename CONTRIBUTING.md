# Contributing

`ironmem` is a Rust workspace with one CLI/MCP server crate (`ironmem`) and two support crates (`ironrace-core`, `ironrace-embed`).

## Prerequisites

- Stable Rust via `rustup`
- `python3` for helper scripts and CI smoke checks
- macOS or Linux for the current supported development flow

## Git Hooks

This repo ships tracked Git hooks in `.githooks/`.

Enable it once per clone:

```bash
git config core.hooksPath .githooks
chmod +x .githooks/pre-commit .githooks/pre-push
```

After that:

- every `git commit` runs:
- `cargo fmt --all -- --check`
- `python3 scripts/check_collab_turn_templates.py`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- every `git push` runs:
- `cargo test --workspace`

## Local Development Loop

From the repo root:

```bash
cargo fmt --all -- --check
python3 scripts/check_collab_turn_templates.py
python3 -m pytest tests/collab_turn_templates/
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
bash scripts/check_versions.sh
python3 scripts/mcp_smoke_test.py
```

Notes:

- `scripts/check_collab_turn_templates.py` lints the collab worker-per-turn templates and the `collab.md` dispatch surface; `tests/collab_turn_templates/` exercises that linter (including negative cases that assert it rejects malformed templates and matrix rows).
- `scripts/check_site_readme_sync.py` is the site/README drift guard (issue #160): it warns when a user-facing surface (`crates/ironmem/src/main.rs`, `crates/ironmem/src/mcp/tools/`, or `crates/ironmem/src/search/tunables.rs`) changes without a matching update to `site/` or `README.md`. CI runs it warn-only on PRs; run it locally against your base with `python3 scripts/check_site_readme_sync.py --base origin/main`. `scripts/test_check_site_readme_sync.py` exercises the guard. For user-facing PRs, route the docs update through the `doc-updater` agent.
- `scripts/check_versions.sh` verifies that plugin metadata versions stay in sync with `crates/ironmem/Cargo.toml`.
- `scripts/mcp_smoke_test.py` starts a real `ironmem serve` process in noop-embedder mode and sends a live `initialize` call over stdio.
- The smoke test uses an isolated temp DB and disables auto-bootstrap/migration so it stays fast and deterministic.

## Quick Binary Build

For a local release-style binary:

```bash
cargo build --release -p ironmem --bin ironmem
./target/release/ironmem setup
IRONMEM_MCP_MODE=trusted ./target/release/ironmem serve
```

If you only need to validate the MCP transport without downloading the embedding model:

```bash
IRONMEM_EMBED_MODE=noop \
IRONMEM_AUTO_BOOTSTRAP=0 \
IRONMEM_DISABLE_MIGRATION=1 \
./target/release/ironmem serve
```

## Versioning

The canonical release version lives in `crates/ironmem/Cargo.toml`.

Before tagging a release:

1. Update `CHANGELOG.md`.
2. Ensure plugin metadata versions match by running `bash scripts/check_versions.sh`.
3. Run the full local development loop.

## Release Process

GitHub Actions publishes tagged releases from `.github/workflows/release.yml`.

Release checklist:

1. Start from a clean `main`.
2. Verify local checks pass:
   - `cargo fmt --all -- --check`
   - `python3 scripts/check_collab_turn_templates.py`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - `cargo test --workspace`
   - `bash scripts/check_versions.sh`
   - `python3 scripts/mcp_smoke_test.py`
3. Tag the release with a `v` prefix, for example `v0.1.0`.
4. Push the tag:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The release workflow builds macOS and Linux archives and attaches them to the GitHub release automatically.

## Adding a Harness

ironmem's harness support is registry-driven. Every harness is a single
`HarnessSpec` entry in `REGISTRY` (`crates/ironmem/src/harness/mod.rs`). The
steps below walk through registering a new harness end-to-end.

### 1. Add a `HarnessSpec` entry to `REGISTRY`

Open `crates/ironmem/src/harness/mod.rs` and add an entry to the `REGISTRY`
constant. The fields are:

| Field | Description |
|---|---|
| `id` | Lowercase slug (`[a-z0-9][a-z0-9_-]*`) — used in CLI output, metrics, and hook paths. |
| `display_name` | Human-readable name shown in `ironmem harnesses` output. |
| `binary` | Executable name carried in the spec; used to derive `Harness::binary()` and `Harness::label()` for the existing `claude`/`codex` launch subcommands. |
| `rules_file` | File written by `ironmem write-rules --harness <id>` (e.g. `"GEMINI.md"`). |
| `write_rules_default` | `true` to include this harness in a no-flag `ironmem write-rules` run. |
| `client_info_aliases` | Substrings matched against `initialize.clientInfo.name` (lowercased) to attribute MCP sessions. |
| `env_aliases` | Strings accepted by `IRONMEM_HARNESS` that map to this harness. |
| `additional_context_support` | `true` if the harness supports `hookSpecificOutput.additionalContext`. Session-start memory injection and UserPromptSubmit context injection are only active when this is `true`. |
| `occupancy_support` | `true` if the harness emits token counts that ironmem can sample. |
| `transcript_parser` | `TranscriptParserKind::Claude`, `::Codex`, or `::None`. Use `None` if the harness has no recognized transcript format; token metric rows are skipped. |

> **What a `REGISTRY` entry enables:** attribution in `ironmem harnesses` output,
> hook dispatch, `ironmem write-rules --harness <id>`, doctor checks, metrics
> persistence, and packaging drift-lint coverage.
>
> **What it does NOT include:** an `ironmem <id> .` launch subcommand. The
> launcher is a closed two-variant `Harness` enum in
> `crates/ironmem/src/launcher/mod.rs` (`Claude` / `Codex`). Adding a
> `HarnessSpec` to `REGISTRY` does not add a variant or expose a new
> `ironmem <id> .` subcommand — the launcher subcommands and their
> `ensure_*_registered` MCP-registration strategies are deliberate per-harness
> code, mirroring how `/collab` is intentionally two-party.

### 2. Add plugin packaging assets

The packaging drift-lint test (`cargo test -p ironmem harness_packaging`) fails
if a registered harness lacks a `.{id}-plugin/` directory at the repo root.
Create it with at minimum:

- `bin/ironmem-mcp.sh` — wrapper that launches `ironmem serve`.
- `hooks/ironmem-hook.sh` — wrapper that calls `ironmem hook <name> --harness <id>`.
- `plugin.json` — plugin metadata (version must match `crates/ironmem/Cargo.toml`).

Use `.claude-plugin/` and `.codex-plugin/` as reference implementations.
Run `bash scripts/check_versions.sh` to confirm the version field is in sync.

### 3. Set `clientInfo` and env aliases

Ensure `client_info_aliases` covers every variant of the harness's MCP
`clientInfo.name` (the registry does a substring match after lowercasing). Set
`env_aliases` to at least `[id]` so the `IRONMEM_HARNESS` test-seam works.

### 4. Implement a transcript parser (optional)

If the harness writes session transcripts in a parseable format and you want
token metric rows to be captured:

1. Implement parsing in `crates/ironmem/src/abeval/` (see `claude.rs` and
   `codex.rs` for existing parsers).
2. Wire `transcript_parser` to the matching `TranscriptParserKind` variant.

If no parser is available, set `transcript_parser: TranscriptParserKind::None`;
token rows for this harness will be skipped rather than mis-attributed.

### 5. Note: `/collab` is not included

`/collab` is a deliberate **two-party Claude↔Codex protocol** and does not
extend to additional harnesses. See
[docs/COLLAB.md — Harness generalization vs two-party protocol](docs/COLLAB.md)
for the rationale.

### 6. Run the gate suite

```bash
cargo test --workspace          # drift-lint + harness tests
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
bash scripts/check_versions.sh  # plugin.json versions must match Cargo.toml
```

All four commands must pass before opening a PR.

## Pull Requests

- Keep changes scoped and explain user-visible behavior in the PR description.
- Add or update tests when behavior changes.
- Prefer documenting contributor workflow changes in this file rather than burying them in CI YAML.

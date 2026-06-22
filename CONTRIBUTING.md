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

## Pull Requests

- Keep changes scoped and explain user-visible behavior in the PR description.
- Add or update tests when behavior changes.
- Prefer documenting contributor workflow changes in this file rather than burying them in CI YAML.

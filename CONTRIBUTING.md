# Contributing

`ironrace-memory` is a Rust workspace with one CLI/MCP server crate (`ironrace-memory`) and two support crates (`ironrace-core`, `ironrace-embed`).

## Prerequisites

- Stable Rust via `rustup`
- `python3` for helper scripts and CI smoke checks
- macOS or Linux for the current supported development flow

## Local Development Loop

From the repo root:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
bash scripts/check_versions.sh
python3 scripts/mcp_smoke_test.py
```

Notes:

- `scripts/check_versions.sh` verifies that plugin metadata versions stay in sync with `crates/ironrace-memory/Cargo.toml`.
- `scripts/mcp_smoke_test.py` starts a real `ironmem serve` process in noop-embedder mode and sends a live `initialize` call over stdio.
- The smoke test uses an isolated temp DB and disables auto-bootstrap/migration so it stays fast and deterministic.

## Quick Binary Build

For a local release-style binary:

```bash
cargo build --release -p ironrace-memory --bin ironmem
./target/release/ironmem setup
./target/release/ironmem serve
```

If you only need to validate the MCP transport without downloading the embedding model:

```bash
IRONMEM_EMBED_MODE=noop \
IRONMEM_AUTO_BOOTSTRAP=0 \
IRONMEM_DISABLE_MIGRATION=1 \
./target/release/ironmem serve
```

## Versioning

The canonical release version lives in `crates/ironrace-memory/Cargo.toml`.

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

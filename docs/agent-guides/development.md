# Development, validation, and documentation

- Run the Rust checks relevant to the change before considering it complete.
- For repository-wide changes, run:

  ```bash
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace
  ```

- When plugin metadata or release wiring changes, also run:

  ```bash
  bash scripts/check_versions.sh
  python3 scripts/mcp_smoke_test.py --binary ./target/debug/ironmem
  ```

- When behavior, setup, release flow, or public API changes, update the relevant documentation in the same change. Keep `README.md`, `docs/CODEX.md`, `CONTRIBUTING.md`, plugin metadata, and workflow documents synchronized when each applies. Prefer concise, concrete examples.

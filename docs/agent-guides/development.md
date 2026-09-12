# Development, validation, and documentation

- Run the Rust checks relevant to the change before considering it complete.
- For repository-wide changes, run:

  ```bash
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace
  ```

- Run those three sequentially, never two at once: concurrent `cargo` invocations block on the `target/` lock, and a run that is really just waiting reads as a hang or a spurious failure.
- Never pipe a gate command through `tail`, `head`, or any other filter. A pipeline reports the *last* command's exit status, so `cargo clippy … | tail -5 && echo CLEAN` prints CLEAN while clippy exits 101. Redirect to a log and check the command's own `$?`. This has produced a false green more than once, including on work that then failed CI.

- When plugin metadata or release wiring changes, also run:

  ```bash
  bash scripts/check_versions.sh
  python3 scripts/mcp_smoke_test.py --binary ./target/debug/ironmem
  ```

- When behavior, setup, release flow, or public API changes, update the relevant documentation in the same change. Keep `README.md`, `docs/CODEX.md`, `CONTRIBUTING.md`, plugin metadata, and workflow documents synchronized when each applies. Prefer concise, concrete examples.

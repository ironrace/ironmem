# AGENTS.md

## Commands
- `cargo test --workspace` - run the full test suite
- `cargo fmt --all -- --check` - run the format task
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - run the lint task
- Run these three sequentially and read each command's own exit status - never pipe one through `tail`, which reports the pager's status and hides a red gate. See `docs/agent-guides/development.md`.

## Code Map
- `crates` - crates
- `docs` - project documentation
- `tests` - automated tests
- `.github` - project configuration

## Conventions
- Use `use` imports with `std::`, external crate, and `crate::` prefixes in Rust files.
- Keep Rust integration test files with the `.rs` extension.

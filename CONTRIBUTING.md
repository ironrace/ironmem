# Contributing

## Setup

- Install stable Rust with `rustup`.
- Clone the repo and work on a feature branch.
- Use `cargo` from the workspace root.

## Development Loop

- Format: `cargo fmt`
- Lint: `cargo clippy --all-targets --all-features -- -D warnings`
- Test: `cargo test --all-targets --all-features`

For focused work on the MCP crate, use:

```bash
cargo test -p ironrace-memory
```

## Expectations

- Add or update tests with every behavior change.
- Prefer unit tests for local logic and integration tests for end-to-end flows.
- Do not hardcode secrets or machine-specific paths.
- Keep docs in sync when CLI behavior, configuration, or workflows change.

## Pull Requests

- Summarize the user-visible change and the main implementation detail.
- List the verification commands you ran.
- Call out any follow-up work or known limitations.

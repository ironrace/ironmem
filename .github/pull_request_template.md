## Summary

<!-- What changed and why. Explain user-visible behavior in plain terms. -->

## Test plan

<!-- How you verified this change. -->

## Checklist

- [ ] Updated README + ironrace.dev site (`site/`) if this changes a user-facing surface (CLI subcommand/flag, MCP tool, or env-var tunable).
- [ ] Added or updated tests for changed behavior.
- [ ] `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings` pass.

<!--
The CI "Site/README Drift Guard" warns (non-blocking) when a user-facing surface
changes without a matching site/README update. For user-facing PRs, route the
docs update through the doc-updater agent.
-->

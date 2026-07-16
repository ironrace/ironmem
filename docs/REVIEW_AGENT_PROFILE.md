# Review-Agent MCP Profile

Canonical, harness-agnostic tool profile for **read-only review sub-agents**
(the `/ultrareview-local` lenses, `pr-review-toolkit` reviewers, and any future
equivalent). A review agent reads a diff and reports findings; it must never
carry ironmem memory tools in its advertised surface — they add token clutter,
invite accidental writes, and are never called on a review path.

## The profile (v1)

- **Allowed:** `Read`, `Grep`, `Glob`, `Bash`
- **Excluded:** every `mcp__ironmem__*` tool — memory, knowledge-graph, diary,
  collab, code-maps, **and** the read-only symbol-graph tools
  (`symbol_lookup`, `symbol_imports`, `symbol_neighbors`, `symbol_graph_index`).

Symbol-graph reads are excluded on purpose: the review briefs already use `grep`
as the caller-tracing method and treat symbol lookups as optional. Excluding them
keeps the profile zero-MCP and identical across every harness.

## Enforcement is client-side, not server-side

Under shared-daemon mode (#190) the `IRONMEM_MCP_MODE` access mode is
**daemon-process-global, not per-client** (see `docs/CODEX.md`). A server-side
"lean mode" therefore cannot be scoped to one review agent. The profile is
enforced in each harness's own agent tool-allowlist instead.

## Per-harness status

| Harness | Mechanism | Status |
|---|---|---|
| Claude Code | `tools:` allowlist in `.claude-plugin/agents/*.md` frontmatter | Enforced. Guarded by `crates/ironmem/tests/plugin_metadata.rs::claude_review_agents_advertise_lean_profile`. |
| Codex | MCP servers are global in `$CODEX_HOME/config.toml`; the installer does not modify agent configs, so per-agent MCP scoping is not ironmem-controllable. | Mitigated: the reviewer brief instructs the agent not to call memory tools; #190's thin proxies make the attached surface cheap (N proxies, not N servers). |
| Grok / Gemini | Plugin-wide `mcpServers` only; no review agents defined yet. | Deferred: apply this profile when review agents are added; #190 keeps the cost negligible until then. |

## Related

- #189 — this profile.
- #190 — shared-daemon mode; removes the process-count motivation and backs the
  Codex/Grok/Gemini "rely on cheap proxies" fallback.

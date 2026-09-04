# Muse Code Guide

## Purpose

`ironmem` gives Muse Code a private, local memory that persists across
sessions and is shared with the other registered harnesses — so Muse can
recall what a repository contains and what was already decided instead of
re-exploring it every time. This guide explains how to set that up with
Muse Code today and what is still unconfirmed.

> **Status.** MEASURED on Muse Code 1.0.2 (`muse-bin-1.0.2-R2040.1` strings
> plus a live `~/.config/muse/settings.json` read): MCP-over-stdio client
> (`initialize`/`clientInfo`/`capabilities`/`protocolVersion 2024-11-05`),
> `schema_version: 1` settings at `~/.config/muse/settings.json`, an
> ARRAY-shaped `mcpServers` of `{id, transport?, command}` /
> `{id, transport:"http", url}` entries, `permissions.mcp_servers` gating
> (`allowed_identities`/`denied_identities`/`allowed_sources`/`allowed_digests`/
> `allowed_kinds`), a `unix_socket` sandbox rule kind with `proxy-only`
> default network mode, hooks + `managed_hooks_path`/`managed_hooks_env_vars`,
> `session/tokenUsage` telemetry, and foreign MCP sources whose manifest
> `mcpServers` "must be an object, path, or path list". Still SCAFFOLDING
> (best-effort defaults): the exact wire `clientInfo.name` (binary
> internals are `tbh`/`musecode`-named — do not assume it contains "muse"),
> runtime permission-gating behavior, unix-socket allowlisting for the
> `--connect` proxy, `additionalContext` injectability, transcript format,
> occupancy output, and `.muse-plugin/` packaging. If your real config or
> wire traffic disagrees with anything below, file the measured shape and
> this guide plus `ensure_muse_registered` get updated to match.

For the bounded Claude↔Codex planning protocol, see [COLLAB.md](COLLAB.md).

## Registry-Driven Hooks and Attribution

Muse is one registered harness in the `REGISTRY` constant
(`crates/ironmem/src/harness/mod.rs`). Its `HarnessSpec` entry records:

- **`id`**: `"muse"` — used as the harness slug in metrics and hook paths.
- **`binary`**: `"muse"` — the launcher binary looked up on `PATH`
  (measured: `muse --help` runs, 1.0.2).
- **`rules_file`**: `"MUSE.md"` — the target for `ironmem write-rules --harness muse`.
- **`rules_strategy`**: `"import"` (`@AGENTS.md`) — `MUSE.md` is written with
  the canonical-block import directive, mirroring Grok/Gemini.
- **`client_info_aliases`**: `["muse"]` — substring matched against
  `initialize.clientInfo.name` to attribute MCP sessions (scaffolding: the
  exact wire name was never captured, and binary internals are
  `tbh`/`musecode`-named, so attribution may miss until the alias is
  narrowed to a captured value).
- **`env_aliases`**: `["muse"]` — accepted by `IRONMEM_HARNESS` for test overrides.
- **`additional_context_support`**: `false` — no
  `hookSpecificOutput.additionalContext` channel is known for Muse, so
  session-start memory injection and UserPromptSubmit context injection are
  Claude Code capabilities only. This is a capability flag in the registry,
  not a hard-coded prefix check.
- **`occupancy_support`**: `false` — no token-count hook output is known for
  Muse yet (scaffolding default).
- **`transcript_parser`**: `None` — no transcript format is known for Muse
  yet (scaffolding default).
- **`write_rules_default`**: `false` — scaffolding, like Grok/Gemini: Muse is
  not yet a default `write-rules` target.

Run `ironmem harnesses --format=json` to inspect the current registry at any
time. See [First run: one-command launchers](../README.md#first-run-one-command-launchers)
in the main README.

## Current Support Level

What works now (measured items above; scaffolding items flagged):

- Running `ironmem` as an MCP server over stdio with non-blocking startup
- Read and write MCP tools
- Semantic search
- Knowledge graph tools
- Restricted vs trusted access modes
- `mine` for workspace ingestion with incremental updates
- `hook` for session-start, stop, and precompact (resolves via the registry;
  unknown harnesses fall back to the Claude spec, while `muse` resolves to
  its own row with `additional_context_support: false`, i.e. silent degrade)
- Muse plugin packaging (`.muse-plugin/` minimal stand-in)
- Automatic migrate-or-init bootstrap on first use

What does not work yet / is unconfirmed:

- The exact wire `clientInfo.name` (attribution alias `["muse"]` is a guess)
- Runtime `permissions.mcp_servers` gating behavior (registering the server
  may not be enough to make it callable)
- Unix-socket allowlisting for the shared-daemon `--connect` proxy under
  `proxy-only` sandbox mode
- Hook behavior specific to Muse transcripts (token persistence, occupancy
  sampling, review capture all assume Claude/Codex-shaped hook input)

## Manual Muse MCP Setup

Add a server entry to your Muse MCP config (measured array shape — an
array of server objects matched by `id`, per the Muse 1.0.2 embedded docs):

```json
{
  "mcpServers": [
    {
      "id": "ironmem",
      "command": "/absolute/path/to/.ironrace/bin/ironmem",
      "args": ["serve", "--connect", "/absolute/path/to/.ironrace-memory/hook_state/daemon.sock"]
    }
  ]
}
```

`ironmem muse` already writes this form for you (and upgrades a
pre-existing bare `["serve"]` entry in place, preserving `env` and sibling
entries). Unrelated top-level keys (e.g. `theme`) and sibling server entries
are preserved; a present-but-non-array `mcpServers` is reported as malformed
rather than silently rewritten.

The daemon is spawned automatically on first connect (single-flight) and
shuts itself down after `IRONMEM_DAEMON_IDLE_SECS` (default 300s) of no
connections. See [Shared Daemon Mode](../README.md#shared-daemon-mode) in
the main README for the full flag/env-var reference and security notes.

**Access mode is daemon-process-global, not per-client.** `IRONMEM_MCP_MODE`
is read once, from whichever process's environment happened to spawn the
shared daemon first. See [CODEX.md](CODEX.md#manual-codex-mcp-setup) for the
full explanation.

Leave `IRONMEM_DB_PATH` unset to use the shared default store
(`~/.ironrace-memory/memory.sqlite3`). Set it only when you want an isolated
store.

## Manual Validation

After registering the MCP server, validate the basics:

1. Start Muse and confirm the server appears in MCP listings.
2. Call `status`.
3. Add a small drawer with `add_drawer`.
4. Search for it with `search`.

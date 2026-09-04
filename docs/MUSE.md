# Muse Code Guide

## Purpose

`ironmem` gives Muse Code a private, local memory that persists across
sessions and is shared with the other registered harnesses — so Muse can
recall what a repository contains and what was already decided instead of
re-exploring it every time. This guide explains how to set that up with
Muse Code today and what is still unconfirmed.

> **Status.** PROVEN LIVE on Muse Code 1.0.2 (`muse exec --provider echo`
> against a logging MCP shim, plus a live `~/.config/muse/settings.json`
> read): MCP-over-stdio client (`initialize`/`clientInfo`/`capabilities`/
> `protocolVersion 2024-11-05`) with wire
> `"clientInfo":{"name":"tbh","version":"0.1.0"}`, `schema_version: 1`
> settings at `$XDG_CONFIG_HOME/muse/settings.json` (else
> `~/.config/muse/settings.json` — an isolated-`XDG_CONFIG_HOME` run picks up
> the override file), and an OBJECT-shaped `mcpServers` keyed by server id,
> exactly Claude's shape. The `{id, transport?, command}` ARRAY shape in the
> binary's embedded docs describes the plugin-manifest context, NOT the
> settings file: an array-shaped `mcpServers` is silently ignored by Muse
> (zero tools start) and breaks Muse's own settings saves ("could not save
> the one-time flag"). Still SCAFFOLDING (best-effort defaults): runtime
> `permissions.mcp_servers` gating behavior, unix-socket allowlisting for
> the `--connect` proxy under `proxy-only` sandbox mode,
> `additionalContext` injectability, transcript format, occupancy output,
> hook wiring (the packaged hook script is inert — see below), and
> `.muse-plugin/` packaging. If your real config or wire traffic disagrees
> with anything below, file the measured shape and this guide gets updated
> to match.

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
- **`client_info_aliases`**: `["tbh"]` — substring matched against
  `initialize.clientInfo.name` to attribute MCP sessions (measured live:
  the wire name is `"tbh"`).
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

What works now (proven-live items above; scaffolding items flagged):

- Running `ironmem` as an MCP server over stdio with non-blocking startup
- Read and write MCP tools
- Semantic search
- Knowledge graph tools
- Restricted vs trusted access modes
- `mine` for workspace ingestion with incremental updates
- `ironmem muse` registration into the object-shaped `mcpServers`
  (seeded with the measured `{"schema_version": 1}` envelope on a fresh file)
- Automatic migrate-or-init bootstrap on first use

Scaffolding / unconfirmed:

- `.muse-plugin/` packaging is a minimal stand-in mirroring the
  Gemini/Grok plugin convention, not a validated native Muse manifest
- `.muse-plugin/hooks/ironmem-hook.sh` is packaged but INERT: nothing
  invokes it (no `hooks` key, no `hooks.json`, `managed_hooks_path` is never
  written), and the muse registry row disables every hook consumer — so
  session-start memory injection and transcript capture do not run for Muse
- Runtime `permissions.mcp_servers` gating behavior (registering the server
  may not be enough to make it callable)
- Unix-socket allowlisting for the shared-daemon `--connect` proxy under
  `proxy-only` sandbox mode
- Hook behavior specific to Muse transcripts (token persistence, occupancy
  sampling, review capture all assume Claude/Codex-shaped hook input)

## Manual Muse MCP Setup

Add a server entry to your Muse MCP config (object shape, keyed by server
id — proven live; do NOT use the `{id, ...}` array shape from the binary's
embedded docs, which describes the plugin-manifest context: Muse silently
ignores an array `mcpServers` and then fails its own settings saves):

```json
{
  "schema_version": 1,
  "mcpServers": {
    "ironmem": {
      "command": "/absolute/path/to/.ironrace/bin/ironmem",
      "args": ["serve", "--connect", "/absolute/path/to/.ironrace-memory/hook_state/daemon.sock"]
    }
  }
}
```

`ironmem muse` already writes this form for you (and upgrades a
pre-existing bare `["serve"]` entry in place, preserving `env` and sibling
entries). Unrelated top-level keys (e.g. `theme`) and sibling server entries
are preserved; a present-but-non-object `mcpServers` is reported as malformed
rather than silently rewritten. If your file still carries an array-shaped
`mcpServers` from an earlier ironmem version, delete the array entry and
re-run `ironmem muse`.

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

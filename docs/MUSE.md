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
> hook wiring (the packaged hook script is inert — see below). The
> `.muse-plugin/` manifest is the native shape (`schemaVersion: 1` with a
> `capabilities` block) and validates clean under `muse plugins validate`
> when checked as an isolated package (the monorepo root adds
> sibling-manifest and symlink diagnostics); installing the whole repo as a
> plugin is still not supported (measured: the package exceeds the install
> entry limit), so skills ship through the managed skills store instead —
> see below. If your real config or wire
> traffic disagrees with anything below, file the measured shape and this
> guide gets updated to match.

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
- `ironmem muse` and `scripts/install-ironmem.sh` registration into the
  object-shaped `mcpServers`
  (seeded with the measured `{"schema_version": 1}` envelope on a fresh
  file, and registered as an `optional` server so a failed start never
  aborts the Muse session)
- Automatic migrate-or-init bootstrap on first use
- `iron-spec` / `iron-plan` / `iron-build` / `iron-tdd` skills, generated
  from `skills/` for the `muse` harness and installed by
  `scripts/install-ironmem.sh` into the managed skills store
  (`$CONFIG_DIR/skills/`), where they take precedence over the
  Claude/Codex copies Muse also reads — see [Muse Skills](#muse-skills)

Scaffolding / unconfirmed:

- Installing the repo as a Muse plugin (`muse plugins install <repo>`) is
  not supported: the package exceeds the install entry limit, so the
  manifest's skills entries are descriptive until a slimmer plugin package
  exists. Skills and MCP wiring both ship through
  `scripts/install-ironmem.sh` instead.
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

## Muse Skills

Muse reads skills from several roots. Without ironmem's install, the
`iron-*` skills resolve to the Claude/Codex copies (`~/.claude/skills`,
`~/.codex/skills`) — Claude-flavored bodies naming tools Muse does not
have. The install below replaces those with Muse-native renders.

`skills/` is the single authored source; `scripts/sync_skills.py` renders
a `muse` harness into `.muse-plugin/skills/` (drift-gated by
`scripts/check_skills_sync.py` like the other harnesses). The Muse
vocabulary (`skills/vocab.toml`) is:

- tracking: `write_todos`
- dispatch: Workflow `agent(input=<full task text>, model=<model>,
  effort=<effort>)` — the only Muse dispatch path that accepts per-call
  routing; `subagent_spawn` inherits the parent route, so a tier routed
  that way does not take effect (see the Muse block in
  `iron-build/references/tiers.md`)
- workspaces: `git worktree add ../<branch> -b <branch>`

Tiers resolve to one model family with effort as the only dial —
`cheap`/`low`, `standard`/`medium`, `deep`/`high`, `frontier`/`max` on
`muse-spark-1.3` (pass the `-contributor` id when the session runs it).
That lineup is a judgment call from the installed model catalog, not a
measured optimum — if it routes badly, the table gets updated.

`scripts/install-ironmem.sh` installs all four through the managed store
(`muse skills install <dir> --scope user --force` each; `--force` covers
both the fresh install and the upgrade). The store owns the write, so the
installer never copies into `$CONFIG_DIR/skills/` directly, keeps no
`.ironmem-bases` snapshot for Muse, and performs no three-way merge: an
upgrade overwrites the managed copies, including hand edits. Managed
copies take precedence over the foreign Claude/Codex copies, which show
as shadowed in `muse skills list`. With no `muse` binary on `PATH` the
installer warns and prints the four manual commands instead of failing.

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
      "args": ["serve", "--connect", "/absolute/path/to/.ironrace-memory/hook_state/daemon.sock"],
      "mode": "optional"
    }
  }
}
```

`"mode": "optional"` matters. Muse's per-server settings (measured from the
1.0.2 binary: `enabled, mode, transport, command, args, env, framing, url,
headers`, with `mode` one of `required` | `optional`) default a server to
`required`, and a required server that fails to start aborts the whole Muse
session. Measured live (`muse exec --provider echo` with the entry's
`command` pointing at a missing file): with no `mode` key, or `required`,
Muse exits 1 with ``Required MCP server `ironmem` failed during startup:
the configured command is unavailable`` and never answers the prompt; with
`optional` it exits 0 and answers. So with `optional`, a moved ironmem
binary or an unspawnable daemon costs Muse its memory, not its session,
which is how every other registered harness already behaves when the
server is down. Drop the key only if you want Muse to refuse to start
without ironmem.

You rarely need to do this by hand: `scripts/install-ironmem.sh` registers
Muse alongside Claude and Codex (seeding `{"schema_version": 1}` on a fresh
file, adding `IRONMEM_MCP_MODE=trusted`, and backfilling a missing `mode`
on an entry that predates the key), and `ironmem muse` registers before it
launches.

`ironmem muse` already writes this form for you, `mode` included, on a
fresh entry (and upgrades a pre-existing bare `["serve"]` entry in place,
preserving `env`, `mode` if you set one, and sibling entries — an upgrade
never adds keys you did not write). Unrelated top-level keys (e.g. `theme`)
and sibling server entries are preserved. Anything else shaped is refused rather than silently
rewritten: a non-object `mcpServers` (including the `[{id, ...}]` array
from the plugin-manifest docs) or a non-object `ironmem` entry makes
`ironmem muse` stop with an "is not an object" error and `ironmem doctor`
report the file as malformed — fix it by hand to the object form above and
re-run. ironmem reads and writes only the `mcpServers` key; Muse also
accepts `mcp_servers` as an alias, but ironmem does not manage that key, so
keep the ironmem entry under `mcpServers`.

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
5. Confirm the skills resolve to the managed store: `muse skills list
   --source user` shows `iron-build`, `iron-plan`, `iron-spec`, and
   `iron-tdd` at `$CONFIG_DIR/skills/...`.

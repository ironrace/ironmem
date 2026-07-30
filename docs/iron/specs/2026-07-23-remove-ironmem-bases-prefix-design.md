# Design: eliminate the `.ironmem-bases:` command/skill prefix

Date: 2026-07-23
Status: Approved — ready for implementation

## Problem

Bundled Claude/Codex commands, skills, agents, and prompts are surfacing in the harness with an
unwanted `.ironmem-bases:` namespace prefix (e.g. `.ironmem-bases:collab`,
`.ironmem-bases:evaluate-issue`, `.ironmem-bases:ultrareview-local`).

## Root cause

`scripts/install-ironmem.sh` installs each bundled file into the harness discovery directory
(`~/.claude/commands`, `~/.claude/skills`, `~/.claude/agents`, `~/.claude/prompts`, and the
`~/.codex/*` mirrors), and also writes a **merge-base backup copy** used for 3-way merge on
re-install. That backup is written to `$target_root/.ironmem-bases/…` — i.e. a subdirectory
*inside* the discovery directory. The harness descends into `.ironmem-bases/` and surfaces every
backup as a namespaced command/skill.

Made worse by the `ironrace-memory → ironmem` repo rename: the flat
`~/.claude/commands/{collab,evaluate-issue}.md` are symlinks into the deleted
`…/git-repos/ironrace-memory/` checkout and are now **broken**, so their only loadable copy is the
backup — which is why `/collab` and `/evaluate-issue` appear *only* under the ugly prefix.

The merge machinery in `install_file_with_merge` / `install_dir_with_merge` already takes the base
path as a parameter; only the *derivation* of that path needs to change.

## Decisions

- **Scope:** durable installer fix + one-time cleanup of the current machine.
- **Backup location:** a sibling hidden directory under the harness home
  (`~/.claude/.ironmem-bases/<kind>/`, `~/.codex/.ironmem-bases/<kind>/`), which the harness does
  not scan. Smallest diff; keeps bases near their targets.
- **Broken symlinks:** replace with real file copies from the current checkout (matches installer
  output; survives future repo renames).

## Design

### 1. Relocate merge-base backups (`scripts/install-ironmem.sh`)

Give each install helper an explicit `base_root` argument instead of deriving the base from
`target_root`. Callers compute `base_root` as a sibling hidden directory under the harness home,
namespaced by kind:

| Kind | Real install (UNCHANGED) | New backup `base_root` |
|------|--------------------------|------------------------|
| Claude command | `$CLAUDE_COMMANDS_DIR/<name>.md` | `$CLAUDE_HOME/.ironmem-bases/commands` |
| Claude skill   | `$CLAUDE_SKILLS_DIR/<skill>/`    | `$CLAUDE_HOME/.ironmem-bases/skills` |
| Claude agent   | `$CLAUDE_AGENTS_DIR/<agent>.md`  | `$CLAUDE_HOME/.ironmem-bases/agents` |
| Claude prompt  | `$CLAUDE_PROMPTS_DIR/<name>.md`  | `$CLAUDE_HOME/.ironmem-bases/prompts` |
| Codex skill    | `$CODEX_SKILLS_DIR/<skill>/`     | `$CODEX_HOME/.ironmem-bases/skills` |
| Codex command  | `$CODEX_COMMANDS_DIR/<name>.md`  | `$CODEX_HOME/.ironmem-bases/commands` |
| Codex prompt   | `$CODEX_PROMPTS_DIR/<name>.md`   | `$CODEX_HOME/.ironmem-bases/prompts` |

The harness scans `~/.claude/commands`, `~/.claude/skills`, etc. — but NOT
`~/.claude/.ironmem-bases` — so the backups become invisible to discovery while the merge logic is
unchanged.

Current base-derivation sites to change:
- `install_skill_set` (~line 198): `local base="$target_root/.ironmem-bases/$skill"`
- `install_agent_set` (~line 233): `"$target_root/.ironmem-bases/$agent.md"`
- `install_md_set`     (~line 376): `"$target_root/.ironmem-bases/$name.md"`

Concrete edits:
1. `install_skill_set`: signature → `harness source_root target_root base_root skills...`
   (`shift 3` → `shift 4`); set `local base="$base_root/$skill"`.
2. `install_agent_set`: add a `base_root` parameter; use `"$base_root/$agent.md"` as the base file.
3. `install_md_set`: signature → `label source_root target_root base_root names...`
   (`shift 3` → `shift 4`); use `"$base_root/$name.md"` as the base file.
4. Update all call sites (currently ~lines 422–438) to pass the matching `base_root` from the
   table. `CLAUDE_HOME` and `CODEX_HOME` are already defined (~lines 406–407).

### 2. Migrate legacy in-discovery backups

Add a helper, called once per install helper before its install loop (after `mkdir -p
"$target_root"`), so upgraders self-heal:

```bash
migrate_legacy_base() {
  local old_base="$1/.ironmem-bases"   # $target_root/.ironmem-bases (legacy, in discovery dir)
  local new_base="$2"                  # $base_root (new, outside discovery)
  [[ -d "$old_base" ]] || return 0
  mkdir -p "$new_base"
  # copy legacy contents (preserve merge history) without clobbering already-migrated entries
  cp -R "$old_base/." "$new_base/" 2>/dev/null || true
  rm -rf "$old_base"
  echo "    migrated legacy install bases: $old_base → $new_base"
}
```

### 3. Test coverage (`scripts/test_install_ironmem.py`)

The existing tests all run with `--skip-skills`, so the skill/command install path is never
exercised — that is why this shipped uncaught. Add a test that runs the installer **with skills**
against a temp `HOME`, overriding the source-root and per-kind target-dir env vars to point at the
repo's `.claude-plugin` / `.codex-plugin` and temp dirs, then asserts:

- the real file exists at `<claude_commands_dir>/collab.md`;
- NO `.ironmem-bases` directory exists under ANY discovery root
  (`commands`, `skills`, `agents`, `prompts` for both harnesses);
- the backup exists at `<claude_home>/.ironmem-bases/commands/collab.md`;
- a pre-seeded legacy `<claude_commands_dir>/.ironmem-bases/collab.md` is removed (migrated) after
  the run.

Relevant install-script env overrides: `CLAUDE_HOME`, `CODEX_HOME`, `CLAUDE_SKILLS_DIR`,
`CLAUDE_AGENTS_DIR`, `CLAUDE_COMMANDS_DIR`, `CLAUDE_PROMPTS_DIR`, `CODEX_SKILLS_DIR`,
`CODEX_COMMANDS_DIR`, `CODEX_PROMPTS_DIR`. Keep `--skip-build`; do NOT pass `--skip-skills` in the
new test. Skip gracefully if `jq` is absent, matching the existing pattern.

### 4. Clean up the current machine (one-time, local)

The installer does not retroactively fix pre-existing pollution or broken symlinks, so also:

1. Remove the stray legacy backup dirs:
   ```bash
   rm -rf ~/.claude/commands/.ironmem-bases ~/.claude/skills/.ironmem-bases \
          ~/.claude/agents/.ironmem-bases  ~/.claude/prompts/.ironmem-bases \
          ~/.codex/prompts/.ironmem-bases  ~/.codex/commands/.ironmem-bases \
          ~/.codex/skills/.ironmem-bases
   ```
2. The flat `~/.claude/commands/collab.md` and `evaluate-issue.md` are BROKEN symlinks into the
   deleted `…/git-repos/ironrace-memory/` checkout. Replace them with real copies from THIS repo:
   ```bash
   rm -f ~/.claude/commands/collab.md ~/.claude/commands/evaluate-issue.md
   cp .claude-plugin/commands/collab.md         ~/.claude/commands/collab.md
   cp .claude-plugin/commands/evaluate-issue.md ~/.claude/commands/evaluate-issue.md
   ```

## Verification

- `bash -n scripts/install-ironmem.sh` (syntax) and `shellcheck` if available.
- `python3 scripts/test_install_ironmem.py` — all pass, including the new case.
- Run the installer once against a scratch HOME and confirm `find "$HOME/.claude" -name
  .ironmem-bases` returns only `$HOME/.claude/.ironmem-bases` (never one inside `commands/`,
  `skills/`, etc.).
- After Task 4, confirm `ls -l ~/.claude/commands/{collab,evaluate-issue}.md` are real files and
  `find ~/.claude ~/.codex -path '*/commands/.ironmem-bases' -o -path '*/skills/.ironmem-bases'`
  is empty.

## Constraints

- Do NOT change the real install destinations — only the backup/base location moves.
- Preserve all existing merge/symlink/idempotency behavior in `install_file_with_merge` and
  `install_dir_with_merge`; only the base path derivation changes.
- Match the script's existing bash style (`set -euo pipefail`, local vars, guarded `mkdir -p`).
- Keep the diff focused: `scripts/install-ironmem.sh` + `scripts/test_install_ironmem.py` only
  (plus the one-time local cleanup in Task 4). No Rust changes — `plugin_metadata.rs` asserts the
  real install path, which is unchanged.

## Blast radius

1 shell script + 1 Python test, plus one-time local filesystem cleanup. No Rust.

#!/usr/bin/env bash
# install-ironmem.sh — atomically install the ironmem binary to ~/.ironrace/bin/
#
# Why this script exists: plain `cp` overwrites bytes in place, same inode.
# macOS lets that happen even while an `ironmem serve` process is actively
# executing the file; the write corrupts the running code page and any new
# invocation loading the same inode silently hangs or exits. This script
# install(1)s the new binary under a temporary name in the same directory and
# renames it over the old one, so running processes keep their old copy, and a
# new invocation (a hook that starts mid-install, say) finds either the old or
# the new whole file, never a missing or partial one.
#
# The script builds release (unless --skip-build), atomically replaces
# ~/.ironrace/bin/ironmem, installs bundled Codex/Claude skill dependencies
# and Muse managed-store skills, registers the MCP server for Claude, Codex
# and Muse, and verifies the resulting binary runs.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

INSTALL_DIR="${IRONMEM_INSTALL_DIR:-$HOME/.ironrace/bin}"
# Made absolute: the MCP registrations and the Claude Code hooks name $TARGET,
# and a relative path there would resolve against whatever directory the
# client or hook happens to start in.
case "$INSTALL_DIR" in
  /*) ;;
  *) INSTALL_DIR="$PWD/$INSTALL_DIR" ;;
esac
TARGET="$INSTALL_DIR/ironmem"
SOURCE="$REPO_ROOT/target/release/ironmem"

# #190 shared-daemon default socket path — mirrors `Config::daemon_socket_path`
# exactly (`<state_dir>/daemon.sock`, honoring an `IRONMEM_DAEMON_SOCKET`
# override) so MCP registrations written by this script match what the
# `ironmem serve --connect`/`--listen` Rust launchers bind/connect against by
# default.
DAEMON_SOCKET_PATH="${IRONMEM_DAEMON_SOCKET:-$HOME/.ironrace-memory/hook_state/daemon.sock}"

# Files install_file_with_merge left untouched this run -- symlinks, merge
# conflicts, or local edits with no base snapshot to diff against. Collected
# here so the run ends with one summary a WARN buried mid-output can't hide.
UNCHANGED_FILES=()

REQUIRED_SHARED_SKILLS=(
  iron-spec
  iron-plan
  iron-build
  iron-tdd
)

# Skills earlier ironmem versions installed, superseded by the iron-* set.
# Removed on upgrade -- but ONLY where a base snapshot under
# <harness home>/.ironmem-bases/skills/<name> proves ironmem wrote that copy.
# ~/.claude/skills and ~/.codex/skills are flat namespaces shared with any
# globally-installed upstream Superpowers, so a missing snapshot means the
# directory is someone else's and must be left alone.
LEGACY_SHARED_SKILLS=(
  writing-plans
  subagent-driven-development
  finishing-a-development-branch
  executing-plans
  using-git-worktrees
  using-superpowers
  requesting-code-review
  test-driven-development
)

REQUIRED_CODEX_SKILLS=(
  pr-review-toolkit
)

REQUIRED_CLAUDE_SKILLS=()

# code-reviewer is collab's inline review agent; the other three are
# /ultrareview-local's core lenses (its pr-review-toolkit conditional agents
# degrade gracefully when that plugin is absent, so they stay unbundled).
REQUIRED_CLAUDE_AGENTS=(
  code-reviewer
  security-reviewer
  architect
  doc-reviewer
)

# /collab-only command + prompt files the install must place so that a fresh
# user can run `/collab` end-to-end. The lists are intentionally minimal —
# anything broader belongs to the plugin-marketplace install path, not this
# script.
REQUIRED_CLAUDE_COMMANDS=(
  collab
  evaluate-issue
  ultrareview-local
)

# /ultrareview-local's fan-out script. Installed as .js under
# <harness home>/workflows/ so the Workflow tool can load it by scriptPath.
# Keep in sync with the resolution order documented in the command file.
REQUIRED_CLAUDE_WORKFLOWS=(
  ultrareview
)

# Worker-per-turn prompt templates. /collab resolves these repo-relative
# (.claude-plugin/prompts/) when running inside an ironrace-memory checkout,
# and falls back to this installed copy for any other target repo.
REQUIRED_CLAUDE_PROMPTS=(
  collab-turn-plan-draft
  collab-turn-plan-synthesis
  collab-turn-plan-review
  collab-turn-plan-finalize
  collab-turn-task-list
  collab-turn-code-implement
  collab-turn-review-fix-global
  collab-turn-review-local
  collab-turn-final-review
  collab-turn-submit
)

REQUIRED_CODEX_COMMANDS=(
  collab
)

REQUIRED_CODEX_PROMPTS=(
  collab-plan-draft
  collab-plan-synthesis
  collab-plan-review
  collab-plan-finalize
  collab-task-list
  collab-batch-impl
  collab-global-review
  collab-review-local
  collab-final-review
  collab-recovery
  evaluate-issue
)

SKIP_BUILD=0
SKIP_SKILLS=0
SKIP_WIRING=0
SKIP_HOOKS=0
FORCE_SKILLS=0
FORCE_WIRING=0

usage() {
  cat <<'EOF'
Usage: scripts/install-ironmem.sh [--skip-build] [--skip-skills] [--skip-wiring]
                                  [--skip-hooks] [--force-skills] [--force-wiring]

Options:
  --skip-build     Install the existing target/release/ironmem binary.
  --skip-skills    Do not install bundled Codex/Claude/Muse skill, agent,
                   command, and prompt dependencies.
  --skip-wiring    Do not register the ironmem MCP server in Claude/Codex/Muse
                   config, and do not wire the Claude Code hooks into
                   $CLAUDE_HOME/settings.json.
  --skip-hooks     Do not wire the Claude Code hooks; still register the MCP
                   server. Without it, hooks are wired by default, and a
                   re-run adds back any of the four you deleted. No hook is
                   wired when the ironmem plugin is enabled in settings.json,
                   and without jq the entries are printed instead.
                   CLAUDE_SETTINGS_JSON overrides the settings path.
  --force-skills   Compatibility flag. Bundled skill/agent/command/prompt
                   files update by default (three-way merged for Codex/Claude,
                   overwritten in the Muse managed store); use --skip-skills
                   to leave existing copies untouched.
  --force-wiring   Replace an existing 'ironmem' MCP entry in ~/.claude.json,
                   ~/.codex/config.toml or ~/.config/muse/settings.json with
                   the bundled one (use only when the config has drifted from
                   a fresh install).
EOF
}

for arg in "$@"; do
  case "$arg" in
    --skip-build)
      SKIP_BUILD=1
      ;;
    --skip-skills)
      SKIP_SKILLS=1
      ;;
    --skip-wiring)
      SKIP_WIRING=1
      ;;
    --skip-hooks)
      SKIP_HOOKS=1
      ;;
    --force-skills)
      FORCE_SKILLS=1
      ;;
    --force-wiring)
      FORCE_WIRING=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "ERROR: unknown argument: $arg" >&2
      usage >&2
      exit 2
      ;;
  esac
done

validate_packaged_skills() {
  local harness="$1"
  local source_root="$2"
  shift 2
  local skills=("$@")
  local missing=0

  for skill in "${skills[@]}"; do
    if [[ ! -f "$source_root/$skill/SKILL.md" ]]; then
      echo "ERROR: bundled $harness skill missing: $source_root/$skill/SKILL.md" >&2
      missing=1
    fi
  done

  if [[ "$missing" -eq 1 ]]; then
    exit 1
  fi
}

validate_packaged_agents() {
  local harness="$1"
  local source_root="$2"
  local missing=0

  for agent in "${REQUIRED_CLAUDE_AGENTS[@]}"; do
    if [[ ! -f "$source_root/$agent.md" ]]; then
      echo "ERROR: bundled $harness agent missing: $source_root/$agent.md" >&2
      missing=1
    fi
  done

  if [[ "$missing" -eq 1 ]]; then
    exit 1
  fi
}

validate_packaged_workflows() {
  local harness="$1"
  local source_root="$2"
  local missing=0

  for workflow in "${REQUIRED_CLAUDE_WORKFLOWS[@]}"; do
    if [[ ! -f "$source_root/$workflow.js" ]]; then
      echo "ERROR: bundled $harness workflow missing: $source_root/$workflow.js" >&2
      missing=1
    fi
  done

  if [[ "$missing" -eq 1 ]]; then
    exit 1
  fi
}

migrate_legacy_base() {
  local old_base="$1/.ironmem-bases"
  local new_base="$2"
  local migration_stage

  [[ -d "$old_base" ]] || return 0
  mkdir -p "$new_base"
  migration_stage="$(mktemp -d "$new_base/.ironmem-migrate.XXXXXX")"
  if ! cp -R "$old_base/." "$migration_stage/"; then
    echo "ERROR: failed to migrate legacy install bases: $old_base → $new_base" >&2
    echo "       Leaving the legacy bases in place so they can be recovered." >&2
    rm -rf "$migration_stage"
    return 1
  fi

  while IFS= read -r staged_path; do
    local relative_path="${staged_path#"$migration_stage/"}"
    local destination="$new_base/$relative_path"
    [[ -e "$destination" || -L "$destination" ]] && continue
    mkdir -p "$(dirname "$destination")"
    mv "$staged_path" "$destination"
  done < <(find "$migration_stage" -mindepth 1 -depth -print)

  rm -rf "$migration_stage"
  rm -rf "$old_base"
  echo "    migrated legacy install bases: $old_base → $new_base"
}

install_skill_set() {
  local harness="$1"
  local source_root="$2"
  local target_root="$3"
  local base_root="$4"
  shift 4
  local skills=("$@")

  validate_packaged_skills "$harness" "$source_root" "${skills[@]}"
  mkdir -p "$target_root"
  migrate_legacy_base "$target_root" "$base_root"

  echo "==> Installing $harness skill dependencies → $target_root"

  for skill in "${skills[@]}"; do
    local source="$source_root/$skill"
    local target="$target_root/$skill"
    local base="$base_root/$skill"

    if [[ ! -e "$target" ]]; then
      cp -R "$source" "$target"
      mkdir -p "$(dirname "$base")"
      rm -rf "$base"
      cp -R "$source" "$base"
      echo "    installed $skill"
      continue
    fi

    if [[ ! -d "$target" ]]; then
      echo "    WARN: $target exists but is not a directory; leaving it unchanged" >&2
      UNCHANGED_FILES+=("$harness skill $skill ($target) — target is not a directory; nothing installed")
      continue
    fi

    install_dir_with_merge "$harness skill" "$source" "$target" "$base"
  done
}

# Remove skills superseded by the iron-* set, using the three-way-merge base
# snapshot as the provenance signal. Present => install_skill_set() wrote that
# copy, so removing it is removing our own file. Absent => it is the user's own
# copy (or an upstream Superpowers install sharing this flat namespace), and we
# warn instead of deleting.
#
# Runs AFTER install_skill_set() so migrate_legacy_base() has already relocated
# any older base layout -- otherwise a legitimately-ours skill whose snapshot
# had not yet been migrated would look user-owned and survive forever.
remove_legacy_skills() {
  local harness="$1"
  local target_root="$2"
  local base_root="$3"

  for skill in "${LEGACY_SHARED_SKILLS[@]}"; do
    local target="$target_root/$skill"
    local base="$base_root/$skill"

    [[ -e "$target" || -L "$target" ]] || continue

    if [[ ! -d "$base" ]]; then
      echo "    WARN: keeping $harness skill '$skill' at $target — no ironmem base snapshot, so it is not ours to remove" >&2
      continue
    fi

    rm -rf "$target"
    rm -rf "$base"
    echo "    removed superseded $harness skill $skill"
  done
}

# Install the iron-* skills into Muse's managed skills store. Unlike the
# Claude/Codex file-copy path, the Muse store owns the write: `muse skills
# install` records files + provenance in its lockfile, so a raw copy into
# $CONFIG_DIR/skills would leave files the store can neither update nor
# uninstall. --force covers both the fresh install and the upgrade (a plain
# reinstall exits skill-already-installed); bundled files update by default,
# and a managed copy the user edited by hand is overwritten -- the store has
# no three-way merge. Nothing is ever removed here: the LEGACY_SHARED_SKILLS
# were never installed to the Muse scope, so there is nothing to clean.
install_muse_skills() {
  local source_root="$1"
  shift
  local skills=("$@")

  # bash 3.2 (macOS's default /bin/bash) aborts on "${skills[@]}" for an empty
  # array under set -u. The call site always passes four skills today, but a
  # future zero-length list must be a clean no-op, not an unbound-variable
  # abort. Mirrors the guard in install_ext_set.
  (( ${#skills[@]} == 0 )) && return 0

  local muse_bin="${MUSE_BIN:-muse}"

  validate_packaged_skills "Muse" "$source_root" "${skills[@]}"

  if ! command -v "$muse_bin" >/dev/null 2>&1; then
    echo "==> WARN: $muse_bin not installed; skipping Muse skill registration." >&2
    echo "          Install Muse, then run for each iron-* skill:" >&2
    for skill in "${skills[@]}"; do
      echo "          $muse_bin skills install $source_root/$skill --scope user --force" >&2
    done
    UNCHANGED_FILES+=("Muse skills (${skills[*]}) — $muse_bin not installed; run the printed 'muse skills install' commands after installing Muse")
    return 0
  fi

  echo "==> Installing Muse skill dependencies (managed store)"

  for skill in "${skills[@]}"; do
    if ! "$muse_bin" skills install "$source_root/$skill" --scope user --force; then
      echo "ERROR: failed to install Muse skill $skill from $source_root/$skill" >&2
      echo "       Retry with: $muse_bin skills install $source_root/$skill --scope user --force" >&2
      echo "       (Re-running is safe: --force makes install idempotent. MCP wiring below did NOT run.)" >&2
      echo "       If the failure names an unknown flag, upgrade Muse: this installer needs 'skills install --force'." >&2
      exit 1
    fi
    echo "    installed $skill"
  done
}

install_agent_set() {
  local harness="$1"
  local source_root="$2"
  local target_root="$3"
  local base_root="$4"

  validate_packaged_agents "$harness" "$source_root"
  mkdir -p "$target_root"
  migrate_legacy_base "$target_root" "$base_root"

  echo "==> Installing $harness agent dependencies → $target_root"

  for agent in "${REQUIRED_CLAUDE_AGENTS[@]}"; do
    local source="$source_root/$agent.md"
    local target="$target_root/$agent.md"

    install_file_with_merge "$harness agent" "$agent" "$source" "$target" \
      "$base_root/$agent.md"
  done
}

install_file_with_merge() {
  local label="$1"
  local name="$2"
  local source="$3"
  local target="$4"
  local base="$5"

  mkdir -p "$(dirname "$target")" "$(dirname "$base")"

  if [[ ! -e "$target" ]]; then
    cp -p "$source" "$target"
    cp -p "$source" "$base"
    echo "    installed $name"
    return
  fi

  if [[ -L "$target" ]]; then
    if cmp -s "$source" "$target"; then
      cp -p "$source" "$base"
      echo "    $name already installed"
      return
    fi

    local packaged_symlink="$target.ironmem-packaged"
    cp -p "$source" "$packaged_symlink"
    echo "    WARN: $label $name is a symlink; left it unchanged" >&2
    echo "          packaged copy: $packaged_symlink" >&2
    UNCHANGED_FILES+=("$label $name ($target) — symlink; packaged copy: $packaged_symlink")
    return
  fi

  if [[ ! -f "$target" ]]; then
    echo "    WARN: $target exists but is not a regular file; leaving it unchanged" >&2
    UNCHANGED_FILES+=("$label $name ($target) — not a regular file; no packaged copy written")
    return
  fi

  if cmp -s "$source" "$target"; then
    cp -p "$source" "$base"
    echo "    $name already installed"
    return
  fi

  if [[ -f "$base" ]]; then
    if cmp -s "$target" "$base"; then
      cp -p "$source" "$target"
      cp -p "$source" "$base"
      echo "    updated $name"
      return
    fi

    if cmp -s "$source" "$base"; then
      echo "    kept local changes in $name (packaged copy unchanged)"
      return
    fi

    if command -v git >/dev/null 2>&1; then
      local merged
      merged="$(mktemp)"
      if git merge-file -p "$target" "$base" "$source" > "$merged"; then
        cp "$merged" "$target"
        cp -p "$source" "$base"
        rm -f "$merged"
        echo "    merged packaged updates into $name"
        return
      fi

      local conflict="$target.ironmem-merge-conflict"
      local packaged="$target.ironmem-packaged"
      cp "$merged" "$conflict"
      rm -f "$merged"
      cp -p "$source" "$packaged"
      echo "    WARN: $label $name has merge conflicts; left local file unchanged" >&2
      echo "          conflict draft: $conflict" >&2
      echo "          packaged copy:  $packaged" >&2
      UNCHANGED_FILES+=("$label $name ($target) — merge conflict; conflict draft: $conflict; packaged copy: $packaged")
      return
    fi

    local packaged_no_git="$target.ironmem-packaged"
    cp -p "$source" "$packaged_no_git"
    echo "    WARN: git not found; left local $label $name unchanged" >&2
    echo "          packaged copy: $packaged_no_git" >&2
    UNCHANGED_FILES+=("$label $name ($target) — git not found; packaged copy: $packaged_no_git")
    return
  fi

  local packaged_no_base="$target.ironmem-packaged"
  cp -p "$source" "$packaged_no_base"
  echo "    WARN: no install base for $label $name; left local file unchanged" >&2
  echo "          packaged copy: $packaged_no_base" >&2
  UNCHANGED_FILES+=("$label $name ($target) — no install base; packaged copy: $packaged_no_base")
}

install_dir_with_merge() {
  local label="$1"
  local source_root="$2"
  local target_root="$3"
  local base_root="$4"

  mkdir -p "$target_root" "$base_root"

  while IFS= read -r rel_dir; do
    rel_dir="${rel_dir#./}"
    [[ "$rel_dir" == "." ]] && continue
    mkdir -p "$target_root/$rel_dir" "$base_root/$rel_dir"
  done < <(cd "$source_root" && find . -type d -print)

  while IFS= read -r rel_file; do
    rel_file="${rel_file#./}"
    install_file_with_merge "$label" "$rel_file" \
      "$source_root/$rel_file" "$target_root/$rel_file" "$base_root/$rel_file"
  done < <(cd "$source_root" && find . -type f -print)
}

# Generic file-set installer used by commands, prompts, and workflows. We don't
# reuse install_agent_set because its name list and target naming are entangled
# with the agent loop; splitting keeps each loop readable.
install_ext_set() {
  local label="$1"      # human-readable kind (e.g. "Claude command")
  local ext="$2"        # extension without the dot (e.g. md, js)
  local source_root="$3"
  local target_root="$4"
  local base_root="$5"
  shift 5
  local names=("$@")

  # bash 3.2 (macOS's default /bin/bash) aborts on "${names[@]}" for an empty
  # array under set -u. REQUIRED_CLAUDE_WORKFLOWS has one entry today, but a
  # future zero-length list (as REQUIRED_CLAUDE_SKILLS already is) must not
  # blow up here.
  (( ${#names[@]} == 0 )) && return 0

  local missing=0
  for name in "${names[@]}"; do
    if [[ ! -f "$source_root/$name.$ext" ]]; then
      echo "ERROR: bundled $label missing: $source_root/$name.$ext" >&2
      missing=1
    fi
  done
  if [[ "$missing" -eq 1 ]]; then
    exit 1
  fi

  # Fatal, not a warning. This guard was added to make the condition
  # *reportable*, but returning 0 from it also made it survivable: before, the
  # `mkdir -p` below failed under `set -euo pipefail` and aborted the run
  # non-zero. Once install_md_set routed Claude commands, Claude prompts, Codex
  # commands and Codex prompts through here too, a single non-directory target
  # meant the script printed "==> Done", exited 0, and had installed an entire
  # kind of file — every command, or the workflow — not at all. An automated
  # setup or CI step gating on the exit code proceeds, and `/ultrareview-local`
  # is simply absent at runtime while the installer reported success.
  #
  # Reporting and exit status are independent: record it for the summary, then
  # fail. A partial install that claims success is the one outcome worse than a
  # loud stop.
  if [[ -e "$target_root" && ! -d "$target_root" ]]; then
    echo "ERROR: $label target $target_root exists but is not a directory; cannot install ${label}s" >&2
    echo "       Remove or rename that path and re-run. Nothing was installed for this kind." >&2
    exit 1
  fi

  mkdir -p "$target_root"
  migrate_legacy_base "$target_root" "$base_root"
  echo "==> Installing ${label}s → $target_root"

  for name in "${names[@]}"; do
    local source="$source_root/$name.$ext"
    local target="$target_root/$name.$ext"

    install_file_with_merge "$label" "$name" "$source" "$target" \
      "$base_root/$name.$ext"
  done
}

# Convenience wrapper over install_ext_set: the four .md call sites read
# better without an "md" literal threaded through each one.
install_md_set() {
  local label="$1" source_root="$2" target_root="$3" base_root="$4"
  shift 4
  install_ext_set "$label" md "$source_root" "$target_root" "$base_root" "$@"
}

# Register the ironmem MCP server in a harness config that keys its servers by
# id in a top-level `mcpServers` object -- Claude Code and Muse Code both do,
# so one writer serves both. This mirrors the Rust
# `mcp_setup::ensure_json_mcpservers_registered`, which routes Claude, Gemini
# CLI, Grok CLI and Muse through a single function; keeping the two writers in
# the same shape matters because either can create the entry (this script on
# install, `ironmem <harness>` on launch) and neither should surprise the other.
#
# Two per-harness knobs, named after their Rust counterparts:
#   seed   -- the document a missing config starts from
#             (`mcp_setup::fresh_file_seed`): `{}` for Claude, Muse's measured
#             `{"schema_version": 1}` envelope for Muse, so a file this script
#             creates is not a bare `mcpServers` in a schema known to be
#             incomplete.
#   extras -- keys a brand-new entry carries beyond command/args/env
#             (`mcp_setup::fresh_entry_extras`): none for Claude,
#             `{"mode": "optional"}` for Muse.
#
# Unlike the Rust side, absent `extras` keys are also filled in on an EXISTING
# ironmem entry. That asymmetry is deliberate and Muse-shaped: a Muse server
# entry with no `mode` defaults to `required`, and a required server whose
# command has gone stale aborts the whole Muse session before it answers
# anything. Entries predating the key -- hand-written ones, or a bare
# `args: ["serve"]` upgraded in place by an older `ironmem muse` -- would
# otherwise keep exactly the hazard `optional` exists to prevent. A `mode` the
# user set deliberately is left alone; only a missing key is filled.
register_json_mcpservers() {
  local label="$1" config="$2" seed="$3" extras="$4"
  local existing_cmd tmp missing repaired=0

  if [[ ! -f "$config" ]]; then
    # ~/.claude.json's parent is $HOME and always exists; Muse's
    # ~/.config/muse/ may not, on a machine where Muse has never run.
    mkdir -p "$(dirname "$config")"
    printf '%s\n' "$seed" > "$config"
  fi

  existing_cmd="$(jq -r '.mcpServers.ironmem.command // empty' "$config" 2>/dev/null || echo "")"

  if [[ -z "$existing_cmd" ]]; then
    echo "==> Registering 'ironmem' MCP server in $config"
    tmp="$(mktemp)"
    jq --arg cmd "$TARGET" --arg sock "$DAEMON_SOCKET_PATH" --argjson extras "$extras" \
      '.mcpServers = ((.mcpServers // {}) + {ironmem: ($extras + {command: $cmd, args: ["serve", "--connect", $sock], env: {IRONMEM_MCP_MODE: "trusted"}})})' \
      "$config" > "$tmp" && mv -f "$tmp" "$config"
  elif [[ "$existing_cmd" == "$TARGET" ]]; then
    # Mirror `mcp_setup::is_bare_serve_json`: an entry still carrying the
    # pre-daemon `args: ["serve"]` is upgraded in place to the shared-daemon
    # proxy command. Without this an install that repaired everything else
    # still left doctor reporting "wired with the legacy bare `serve` command
    # (upgrade available via `ironmem <harness>`)" -- sending the user to the
    # launcher this script exists to make unnecessary.
    if jq -e '.mcpServers.ironmem.args == ["serve"]' "$config" >/dev/null 2>&1; then
      echo "==> Upgrading the $label MCP registration to the shared-daemon proxy command"
      tmp="$(mktemp)"
      jq --arg sock "$DAEMON_SOCKET_PATH" \
        '.mcpServers.ironmem.args = ["serve", "--connect", $sock]' \
        "$config" > "$tmp" && mv -f "$tmp" "$config"
      repaired=1
    fi

    if jq -e '.mcpServers.ironmem.env.IRONMEM_MCP_MODE == null' \
      "$config" >/dev/null 2>&1; then
      echo "==> Adding trusted mode to the existing $label MCP registration"
      tmp="$(mktemp)"
      jq '.mcpServers.ironmem.env = ((.mcpServers.ironmem.env // {}) + {IRONMEM_MCP_MODE: "trusted"})' \
        "$config" > "$tmp" && mv -f "$tmp" "$config"
      repaired=1
    fi

    # `$extras + entry` keeps the entry's own value wherever both carry a key,
    # so this only ever fills gaps.
    missing="$(jq -r --argjson extras "$extras" \
      '[($extras | keys[])] - [(.mcpServers.ironmem | keys[])] | join(", ")' \
      "$config" 2>/dev/null || echo "")"
    if [[ -n "$missing" ]]; then
      echo "==> Adding $missing to the existing $label MCP registration"
      tmp="$(mktemp)"
      jq --argjson extras "$extras" \
        '.mcpServers.ironmem = ($extras + .mcpServers.ironmem)' \
        "$config" > "$tmp" && mv -f "$tmp" "$config"
      repaired=1
    fi

    if [[ "$repaired" -eq 0 ]]; then
      echo "    'ironmem' MCP server already registered for $label"
    fi
  elif [[ "$FORCE_WIRING" -eq 1 ]]; then
    echo "==> Replacing divergent 'ironmem' MCP entry (was: $existing_cmd)"
    tmp="$(mktemp)"
    jq --arg cmd "$TARGET" --arg sock "$DAEMON_SOCKET_PATH" --argjson extras "$extras" \
      '.mcpServers.ironmem = ($extras + {command: $cmd, args: ["serve", "--connect", $sock], env: {IRONMEM_MCP_MODE: "trusted"}})' \
      "$config" > "$tmp" && mv -f "$tmp" "$config"
  else
    echo "    WARN: 'ironmem' MCP entry already exists with command=$existing_cmd" >&2
    echo "          Expected: $TARGET. Re-run with --force-wiring to replace." >&2
  fi
}

# ---- Claude Code hooks (#377) ----------------------------------------------
# A plugin install gets its four hooks from .claude-plugin/hooks/hooks.json. A
# script install gets the same four here, in $CLAUDE_HOME/settings.json, each
# running the installed binary directly. They carry no IRONMEM_MCP_MODE, so
# unless settings.json's env sets one (or Claude Code inherits one) they run
# read-only: SessionStart context and UserPromptSubmit recall work, and Stop
# and PreCompact skip mining (a cheaper way to mine outside the plugin is
# #418). `ironmem doctor` reports that on its hook_mining line. The line
# printed after wiring reads the file's env.IRONMEM_MCP_MODE: trusted (trimmed,
# any case, as McpAccessMode::parse reads it) mines, any other string does
# not, and with none there it names the environment Claude Code is launched
# with, which decides and which this script cannot see.
#
# The merge only appends. An event that already runs `ironmem hook <name>`
# (from any path, or through the plugin's ironmem-hook.sh) is left alone, and
# so is every other hook and key. A run whose program or script word uses
# ${CLAUDE_PLUGIN_ROOT} does not count: Claude Code leaves that variable unset
# for a settings.json hook (measured on 2.1.329), so the run finds no script.
# (A shell command that gives the variable a default, as in
# ${CLAUDE_PLUGIN_ROOT:-/path}, runs the default and does count, when a shell
# expands that use: not in single quotes or after a backslash.)
# When an event holds such an entry and nothing else that runs the hook, the
# hook is added beside it and, once the file is written, a WARN names the
# event and says that entry does not run outside the plugin and can be
# deleted (never the entry itself).
# If a run of an event names a program by a literal path (absolute or ~/) that
# is not the same file as this install's program, and no run names that
# program, a WARN names the event and that path (never the whole entry, which
# can carry a token). A PATH lookup, a path the shell builds and a wrapper
# script run as `bash <script>` get no WARN; a wrapper named directly as the
# program does, as doctor checks it. An exec-form entry whose program starts
# with ~/ gets a WARN too, even when ~/ would expand to this install's
# program: doctor reads it as not expanded. With disableAllHooks true the
# hooks are still wired (a project's settings can turn them back on) and a
# WARN says none run; with the ironmem plugin enabled none are wired, and a
# WARN names any hand-wired copies. The result is built in temp files and
# written to settings.json once, after every event is decided. A failure
# before or during that write (an unreadable, unparseable, read-only or
# several-valued file, a dangling symlink, a directory that cannot take a new
# file, a jq error, a temp-file step, a file another program changed since it
# was read) prints a WARN, leaves settings.json untouched and lets the install
# go on. The write goes to a temp file in the directory of the file a symlink
# chain ends at, which is then renamed onto that file: the links stay links,
# a reader sees the old or the new whole file, and a write that fails partway
# (a full disk) leaves the old one. That costs a hard link to the file (a
# rename gives a new inode), and a new settings.json is created mode 0600. An
# interrupt removes the temp files. Every outcome returns 0.
#
# A hook entry is shell form (no `args`: a shell runs the `command` string) or
# exec form (`args` an array of strings: `command` is the program and the args
# its arguments, no shell). The installer writes shell form. A shell-form
# command is matched as described above _CLAUDE_HOOK_JQ_DEFS. An exec-form
# entry is read as an argument vector, the way doctor reads it: env (with its
# options and assignments), command and nohup are peeled off (not exec, which
# is a shell builtin with no program to spawn); then the program's basename
# must be ironmem or ironmem-mcp.sh followed by `hook [--harness <v>] <name>`
# (or `--harness=<v>`), or ironmem-hook.sh followed by `<name>`; bash or sh
# may run either script, and `bash -c <string>` (or sh) is matched with the
# shell-form rule. An entry whose `args` is anything else is skipped, as
# doctor skips it. scripts/fixtures/hook_stop_fixtures.json holds the Stop
# entries both scripts/test_install_ironmem.py and doctor's unit tests read,
# with one answer each. Keep CLAUDE_HOOK_EVENTS in step with PLUGIN_HOOKS in
# crates/ironmem/src/doctor/hooks.rs; the unit test
# plugin_hooks_table_matches_the_plugin_hooks_json pins that table to
# hooks.json, and scripts/test_install_ironmem.py pins this one to it.
CLAUDE_HOOK_EVENTS=(
  SessionStart:session-start
  UserPromptSubmit:user-prompt-submit
  Stop:stop
  PreCompact:precompact
)

_SHELL_SAFE_CHARS='abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_./+@%,:-'

# Prints $1 as one shell word for a hook command, which Claude Code runs
# through a shell. A word made only of _SHELL_SAFE_CHARS (spelled out, so no
# locale widens a range) is printed as it is; anything else is single-quoted,
# with each embedded ' written as '\''. Loops instead of ${1//\'/...}, whose
# replacement text bash 3.2 and 5.2 read differently.
_shell_word() {
  local rest="$1" quoted=""
  if [[ -n "$rest" && "$rest" != *[!$_SHELL_SAFE_CHARS]* ]]; then
    printf '%s' "$rest"
    return 0
  fi
  while [[ "$rest" == *\'* ]]; do
    quoted+="${rest%%\'*}'\\''"
    rest="${rest#*\'}"
  done
  printf "'%s'" "$quoted$rest"
}

# $TARGET as the first word of a hook command.
HOOK_PROGRAM="$(_shell_word "$TARGET")"

# Prints $1 escaped for the inside of a JSON string: \ and " get a backslash,
# newline, carriage return and tab become \n, \r and \t, and every other
# character below U+0020 (and DEL) becomes \u00XX. One pass over the bytes
# (LC_ALL=C, so the range is bytewise and a multibyte character passes through
# whole), so no escape is escaped again.
_json_string_body() {
  local LC_ALL=C
  local rest="$1" out="" c i
  for ((i = 0; i < ${#rest}; i++)); do
    c="${rest:i:1}"
    case "$c" in
      \\) out+="\\\\" ;;
      \") out+='\"' ;;
      $'\n') out+='\n' ;;
      $'\r') out+='\r' ;;
      $'\t') out+='\t' ;;
      [$'\x01'-$'\x1f'$'\x7f'])
        printf -v c '\\u%04x' "'$c"
        out+="$c"
        ;;
      *) out+="$c" ;;
    esac
  done
  printf '%s' "$out"
}

# The path `ironmem doctor` reads for its hooks and hook_mining lines.
_DOCTOR_SETTINGS="$HOME/.claude/settings.json"

# Is $1 the settings file `ironmem doctor` reads? Another spelling of the path
# counts (`-ef`), and so does one whose directory is doctor's while the file
# does not exist yet (`-ef` is false for a missing file).
_doctor_reads() {
  [[ "$1" == "$_DOCTOR_SETTINGS" || "$1" -ef "$_DOCTOR_SETTINGS" ]] \
    || [[ "${1##*/}" == settings.json && "${1%/*}" -ef "${_DOCTOR_SETTINGS%/*}" ]]
}

# Prints the first line of file $1 (nothing when it is missing or empty). A
# builtin read, so a stubbed `cat` or `head` in a test never runs here.
_first_line() {
  local line=""
  IFS= read -r line < "$1" 2>/dev/null
  printf '%s' "$line"
}

# Prints the WARN for a wiring attempt that wrote nothing. $1 says what failed,
# $2 is the settings file, and $3, when not empty, is jq's stderr: its first
# line is printed after the WARN.
_wire_claude_hooks_warn() {
  echo "    WARN: $1; wired no Claude hooks." >&2
  if [[ -n "${3:-}" ]]; then
    echo "          jq said: ${3%%$'\n'*}" >&2
  fi
  if _doctor_reads "$2"; then
    echo "          Fix that, then re-run this script. \`ironmem doctor\` shows what is missing." >&2
  else
    echo "          Fix that, then re-run this script. \`ironmem doctor\` reads only $_DOCTOR_SETTINGS, not this file." >&2
  fi
}

# jq definitions for reading hook entries, shared by the matcher and the
# program check. A hook entry is shell form when it has no `args` and exec
# form when `args` is an array of strings; any other `args` is neither, and
# such an entry is skipped (as doctor skips it). They use only builtins jq 1.5
# has (no `IN`, no `if` without `else`), so an older packaged jq still works.
#
# A shell-form command is read with regexes (re_*) that want ironmem where a
# shell starts a program, as doctor's `HookCommand::runs_hook` does: at the
# start, or after `;` `&` `|` `(` `)` or a line break (not after `$(`), then
# after any unquoted NAME=value words and any env (with its own options and
# assignments), exec, command or nohup (but not an exec right after env, nohup
# or exec, which run it as a program and find none; `command exec` runs the
# builtin), or at the start of a quoted `bash -c`
# or `sh -c` string. So `echo ironmem hook stop`, `# ironmem hook stop` and
# `"IRONMEM_MCP_MODE=trusted" ironmem hook stop` do not count, and `hook
# --harness <v> <name>`, `hook --harness=<v> <name>` and a quoted name do.
# Where the two still differ: here a `;` `&` `|` `(` `)` or line break inside
# quotes or a comment starts a command (`echo "x; ironmem hook stop"` counts),
# and in doctor it does not; doctor drops a redirection before the program
# (`2>/dev/null ironmem hook stop` counts there), and this does not. A program
# or script word that uses ${CLAUDE_PLUGIN_ROOT} (or $CLAUDE_PLUGIN_ROOT)
# does not match: Claude Code leaves it unset for a settings.json hook, so
# the run finds no script. In a word a shell reads (re_plugin_root), each use
# is judged where it stands in the word, as /bin/sh, bash and dash were
# measured to read it with the variable unset. A use the shell expands still
# matches when it is braced with a default that is not empty
# (${CLAUDE_PLUGIN_ROOT:-/path}, also `-`, `:=` and `=`), because the shell
# runs the default; any other expansion of the unset variable is empty and
# does not match. A default of only empty quote pairs is empty: `""` and `''`
# in an unquoted use, but only `""` in a double-quoted one, where `''` is two
# characters. A use the shell does not expand (in single quotes, or with a
# backslash before its `$`, unquoted or in double quotes) is a literal path
# that holds the text, so it does not match, default or not. Inside a
# double-quoted `bash -c` or `sh -c` string ($dq), in its first command or
# one after a `;` `&` `|` `(` `)` or line break (re_dq_later), two shells read
# the use. The outer one expands a `$` with no backslash before it, single
# quotes or not, and takes one backslash off a `\$`; the inner one then
# expands that `$` unless it gets it in single quotes or after a backslash of
# its own. So there a use is not expanded when it is `\$` in single quotes
# (any odd number of backslashes) or `\\\$` outside them (three, seven, ...),
# and does not match, default or not; every other use is expanded and only
# its default decides (`""` and `''` both empty). Not told apart: a default
# that is empty for another reason (${CLAUDE_PLUGIN_ROOT:-$UNSET} matches),
# and a -c string nested in a double-quoted -c string, which is read as the
# outer string is. No shell reads
# an exec-form element, so there every use is refused (plugin_root); what
# Claude Code does with a default in one was not measured. unroot puts a path
# where each use was, to ask whether the entry would run the hook but for the
# variable.
_CLAUDE_HOOK_JQ_DEFS='
def base: sub("^.*/"; "");
def is_one_of(words): . as $w | any(words; . == $w);
def exec_form: has("args") and (.args | type == "array" and all(.[]; type == "string"));
def shell_form: has("args") | not;
def re_seg: "(?:[^\\s\"\u0027\\\\;&|()<>]|\\\\.|\"(?:[^\"\\\\]|\\\\.)*\"|\u0027[^\u0027]*\u0027)";
def re_word: re_seg + "+";
def re_q: "[\"\u0027]?";
def re_start: "(?:^|[;&|)\\n]|(?<!\\$)\\()\\s*";
def re_assign: "[A-Za-z_][A-Za-z0-9_]*=" + re_seg + "*";
def re_named($n): re_q + "(?:\\S*/)?" + $n + re_q;
def re_env_arg:
  "(?:(?:-u|--unset|-C|--chdir)\\s+" + re_word
  + "|-(?!(?:u|-unset|C|-chdir)\\s)" + re_seg + "*"
  + "|" + re_q + "[A-Za-z_][A-Za-z0-9_]*=" + re_seg + "*" + re_q + ")";
def re_exec: re_named("exec") + "\\s+";
def re_wrappers:
  "(?:" + re_exec + ")?(?:(?:" + re_named("nohup")
  + "|" + re_named("env") + "(?:\\s+" + re_env_arg + ")*)\\s+|" + re_named("command") + "\\s+(?:" + re_exec + ")?)*";
def re_head: "(?:" + re_assign + "\\s+)*" + re_wrappers;
def re_shell:
  re_named("(?:bash|sh)")
  + "(?:\\s+(?:[-+][oO]\\s+" + re_word + "|\\+" + re_seg + "*|--" + re_seg + "*|-[^\\sc\"\u0027;&|()<>]*))*";
def re_bash_c($q): re_shell + "\\s+-(?!-)[^\\s\"\u0027;&|()<>]*c[^\\s\"\u0027;&|()<>]*\\s+" + $q + "\\s*";
def re_c_strings($q): "(?:" + re_bash_c($q) + re_head + ")*";
def re_dq_later: "(?:(?:[^\"\\\\]|\\\\.)*(?:[;&|)\\n]|(?<!\\$)\\()\\s*)?";
def re_lead($dq):
  re_start + re_head
  + (if $dq then
       re_c_strings("[\"\u0027]") + re_bash_c("\"") + re_dq_later + re_head + re_c_strings("[\"\u0027]")
     else re_c_strings("\u0027") end);
def re_root: "\\$\\{?CLAUDE_PLUGIN_ROOT(?![A-Za-z0-9_])";
def re_root_empty($pairs):
  "\\$(?:CLAUDE_PLUGIN_ROOT(?![A-Za-z0-9_])|\\{CLAUDE_PLUGIN_ROOT(?![A-Za-z0-9_]|:?[-=](?>(?:" + $pairs + ")*)[^}]))";
def re_in_dq: "\"(?:[^\"\\\\]|\\\\.)*";
def re_plugin_root($dq):
  re_seg + "*(?:"
  + (if $dq then
       "(?:" + re_in_dq + "|\u0027[^\u0027]*)?\\\\?" + re_root_empty("\"\"|\u0027\u0027")
       + "|(?<!\\\\)(?:\\\\{4})*\\\\{3}" + re_root
       + "|\u0027[^\u0027]*(?<!\\\\)(?:\\\\{2})*\\\\" + re_root
     else
       re_root_empty("\"\"|\u0027\u0027")
       + "|" + re_in_dq + re_root_empty("\"\"")
       + "|(?:" + re_in_dq + ")?\\\\" + re_root
       + "|\u0027[^\u0027]*" + re_root
     end)
  + ")";
def plugin_root: test(re_root);
def re_program($b; $dq):
  "(?<p>(?!" + re_plugin_root($dq) + ")(?:" + re_seg + "*(?:\"(?:[^\"\\\\]|\\\\.)*/|\u0027[^\u0027]*/|/))?" + re_q + $b + re_q + ")";
def re_name($name): "\\s+" + re_q + $name + re_q + "(?=[\\s;&|)<>]|$)";
def re_hook($name):
  "\\s+" + re_q + "hook" + re_q + "(?:\\s+--harness\\s+" + re_word + "|\\s+--harness=" + re_seg + "*)?"
  + re_name($name);
def shell_runs($name; $dq):
  test(re_lead($dq) + re_program("ironmem(?:-mcp\\.sh)?"; $dq) + re_hook($name))
  or test(re_lead($dq) + "(?:" + re_shell + "\\s+)?" + re_program("ironmem-hook\\.sh"; $dq) + re_name($name))
  or test(re_lead($dq) + re_shell + "\\s+" + re_program("ironmem-mcp\\.sh"; $dq) + re_hook($name));
def shell_runs($name): shell_runs($name; false) or shell_runs($name; true);
def env_args:
  if length == 0 then .
  elif (.[0] | is_one_of("-u", "--unset", "-C", "--chdir")) then .[2:] | env_args
  elif (.[0] | test("^[A-Za-z_][A-Za-z0-9_]*=") or startswith("-")) then .[1:] | env_args
  else . end;
def peel:
  if length == 0 then .
  elif (.[0] | base) == "env" then .[1:] | env_args | peel
  elif (.[0] | base | is_one_of("command", "nohup")) then .[1:] | peel
  else . end;
def after_hook($name):
  .[0] == "hook"
  and (if .[1] == "--harness" then .[3]
       elif ((.[1] // "") | startswith("--harness=")) then .[2]
       else .[1] end) == $name;
def shell_script($name):
  (.[0] | base) as $b
  | if (.[0] | plugin_root) then false
    elif $b == "ironmem-hook.sh" then .[1] == $name
    elif $b == "ironmem-mcp.sh" then .[1:] | after_hook($name)
    else false end;
def shell_args($name):
  if length == 0 then false
  elif (.[0] | startswith("-") and (startswith("--") | not) and contains("c")) then (.[1] // "") | shell_runs($name)
  elif (.[0] | is_one_of("-o", "+o", "-O", "+O")) then .[2:] | shell_args($name)
  elif (.[0] | test("^[-+]")) then .[1:] | shell_args($name)
  else shell_script($name) end;
def exec_runs($name):
  peel
  | ((.[0] // "") | base) as $b
  | if ((.[0] // "") | plugin_root) then false
    elif $b == "ironmem" or $b == "ironmem-mcp.sh" then .[1:] | after_hook($name)
    elif $b == "ironmem-hook.sh" then .[1] == $name
    elif $b == "bash" or $b == "sh" then .[1:] | shell_args($name)
    else false end;
def entries($ev):
  (.hooks[$ev] // [])[] | objects | (.hooks // []) | arrays | .[] | objects
  | select(.command | type == "string");
def runs_hook($name):
  if shell_form then .command | shell_runs($name)
  elif exec_form then [.command] + .args | exec_runs($name)
  else false end;
def unroot:
  gsub("\\$\\{CLAUDE_PLUGIN_ROOT(?![A-Za-z0-9_])[^}]*\\}"; "/claude-plugin-root")
  | gsub("\\$CLAUDE_PLUGIN_ROOT(?![A-Za-z0-9_])"; "/claude-plugin-root");
def plugin_root_copy($name):
  ([.command] + (if exec_form then .args else [] end) | any(.[]; plugin_root))
  and (.command |= unroot | (if exec_form then .args |= map(unroot) else . end) | runs_hook($name));
def esc_line: split("\\") | join("\\\\") | split("\n") | join("\\n") | split("\t") | join("\\t");
def unquote:
  [scan("\"(?:[^\"\\\\]|\\\\.)*\"|\u0027[^\u0027]*\u0027|\\\\.|[^\"\u0027\\\\]+")] as $s
  | {text: ($s | map(
        if startswith("\u0027") then .[1:-1]
        elif startswith("\"") then .[1:-1] | [scan("\\\\[$`\"\\\\\\n]|[^\\\\]|\\\\")]
          | map(if length == 2 then .[1:] else . end) | join("")
        elif startswith("\\") then .[1:]
        else . end) | join("")),
     dynamic: any($s[]; ((startswith("\u0027") or startswith("\\")) | not) and test("[$`]")),
     tilde: (($s[0] // "") | startswith("~/"))};
def shell_programs($name):
  [(false, true) as $dq
   | (match(re_lead($dq) + re_program("ironmem(?:-mcp\\.sh)?"; $dq) + re_hook($name); "g"),
      match(re_lead($dq) + re_program("ironmem-hook\\.sh"; $dq) + re_name($name); "g"))
   | .captures[] | select(.name == "p") | .string | unquote];
def c_string:
  if length == 0 then empty
  elif (.[0] | startswith("-") and (startswith("--") | not) and contains("c")) then .[1] // empty
  elif (.[0] | is_one_of("-o", "+o", "-O", "+O")) then .[2:] | c_string
  elif (.[0] | test("^[-+]")) then .[1:] | c_string
  else empty end;
def exec_programs($name):
  peel
  | ((.[0] // "") | base) as $b
  | if (($b == "ironmem" or $b == "ironmem-mcp.sh") and (.[1:] | after_hook($name)))
       or ($b == "ironmem-hook.sh" and .[1] == $name) then
      [{text: .[0], dynamic: (.[0] | contains("${")), tilde: (.[0] | startswith("~/")),
        exec_tilde: (.[0] | startswith("~/"))}]
    elif $b == "bash" or $b == "sh" then [.[1:] | c_string | shell_programs($name)[]]
    else [] end;
def literal_path($home):
  select((.dynamic | not) and (.text | test("[*?\\[{]") | not))
  | if .tilde then $home + .text[1:] elif (.text | startswith("/")) then .text else empty end;
def program_paths($name; $home):
  (if shell_form then .command | shell_programs($name)
   elif exec_form then [.command] + .args | exec_programs($name)
   else [] end)
  | .[] | if .exec_tilde then ["~", .text] else (literal_path($home) as $p | ["path", $p]) end;
def visible: explode | map(if . < 32 or . == 127 then 63 else . end) | implode;
'

# Does hooks.<event> already run `ironmem hook <name>`? Exit 0 yes, 1 no, 2+ a
# jq error (the caller warns on that rather than reading it as "no"); jq's
# stderr is passed through for the caller to capture.
_claude_hook_is_wired() {
  jq -e --arg ev "$2" --arg name "$3" "$_CLAUDE_HOOK_JQ_DEFS"'
      [entries($ev) | select(runs_hook($name))] | length > 0' \
    "$1" >/dev/null
}

# Does hooks.<event> ($2) in work file $1 hold an entry that would run
# `ironmem hook <name>` ($3) but for its ${CLAUDE_PLUGIN_ROOT}, which Claude
# Code leaves unset for a settings.json hook? Exit 0 yes, 1 no, 2+ a jq error.
_claude_hook_has_plugin_root_copy() {
  jq -e --arg ev "$2" --arg name "$3" "$_CLAUDE_HOOK_JQ_DEFS"'
      [entries($ev) | select(plugin_root_copy($name))] | length > 0' \
    "$1" >/dev/null
}

# For hooks.<event> ($2) in work file $1, one line for each run of `ironmem
# hook <name>` ($3) whose program word is a literal path: absolute, or an
# unquoted ~/ (read as under $HOME). The program may be ironmem, or the
# plugin's ironmem-mcp.sh or ironmem-hook.sh named directly as the program,
# since doctor checks each of those. Each line is a kind (`path`), a tab, the
# path, a tab, and the path as a WARN shows it (each control character as
# `?`), both with \ written as \\ and a line break or tab as \n or \t, so
# `printf %b` reads them back. The program word may be single- or
# double-quoted, and the run may sit in a bash/sh -c string of an exec-form
# entry. A PATH lookup (a bare `ironmem`), a path the shell builds (`$HOME`,
# `${HOME}`, a glob) and a wrapper script run as `bash <script>` give no
# line, as doctor's missing-program check skips them. An exec-form program
# that starts with ~/ gives the kind `~` and the program word as written:
# doctor reads it as not expanded (no shell reads an exec-form command), so it
# does not run whatever ~/ would expand to. A jq error exits non-zero.
_claude_hook_program_paths() {
  jq -r --arg ev "$2" --arg name "$3" --arg home "$HOME" "$_CLAUDE_HOOK_JQ_DEFS"'
      entries($ev) | select(runs_hook($name)) | program_paths($name; $home)
      | "\(.[0])\t\(.[1] | esc_line)\t\(.[1] | visible | esc_line)"' \
    "$1"
}

# For an event ($3, hook $4) that is already wired in work file $1 (settings
# file $2): WARN when a run of it names a program by a literal path that is not
# the same file as $TARGET (bash -ef, so a symlink to it or another spelling of
# its path counts as $TARGET), and WARN when an exec-form entry starts its
# program with ~/ (doctor reads it as not running), unless another run names
# $TARGET. Such an entry is still counted as wired, as doctor counts it (its
# hooks line names it as wired to a program that does not run, not missing),
# and appending a copy would run the hook twice if Claude Code did expand ~.
# Never changes the entry; a jq error here is a WARN too. A WARN names only
# the event and the program path, never the whole entry, which can carry a
# token in a NAME=value word or an argument.
_warn_other_program() {
  local work="$1" settings="$2" event="$3" name="$4" err_file="${1%/*}/program-paths.err"
  local records kind epath eshown path shown doctor_line
  local -a others=() tildes=()
  if ! records="$(_claude_hook_program_paths "$work" "$event" "$name" 2>"$err_file")"; then
    echo "    WARN: jq failed checking which program the $event hook in $settings runs; left it as it is." >&2
    echo "          jq said: $(_first_line "$err_file")" >&2
    return 0
  fi
  [[ -z "$records" ]] && return 0
  while IFS=$'\t' read -r kind epath eshown; do
    printf -v path '%b' "$epath"
    printf -v shown '%b' "$eshown"
    if [[ "$kind" == "~" ]]; then
      _in_list "$shown" "${tildes[@]+"${tildes[@]}"}" || tildes+=("$shown")
      continue
    fi
    if [[ "$path" == "$TARGET" || "$path" -ef "$TARGET" ]]; then
      return 0
    fi
    _in_list "$shown" "${others[@]+"${others[@]}"}" || others+=("$shown")
  done <<< "$records"
  case "$event" in Stop | PreCompact) doctor_line="hook_mining" ;; *) doctor_line="hooks" ;; esac
  if (( ${#others[@]} > 0 )); then
    echo "    WARN: $event in $settings is already wired, so this install left it as it is, but the program it names by path is not $TARGET, the program this install put in place. It names:" >&2
    _print_indented "${others[@]}"
    if _doctor_reads "$settings"; then
      echo "          If no other command for $event starts and that program is not an executable file, \`ironmem doctor\` warns on its $doctor_line line." >&2
    else
      echo "          \`ironmem doctor\` reads only $_DOCTOR_SETTINGS, not this file." >&2
    fi
  fi
  if (( ${#tildes[@]} > 0 )); then
    echo "    WARN: $event in $settings is already wired, so this install left it as it is, but its exec-form entry (args set) starts its program with ~/, which doctor reads as not expanded, as no shell reads an exec-form command. It names:" >&2
    _print_indented "${tildes[@]}"
    if _doctor_reads "$settings"; then
      echo "          Write the program's full path in that entry, or remove the entry and re-run this script. If no other command for $event starts, \`ironmem doctor\` reports it as not running on its $doctor_line line." >&2
    else
      echo "          Write the program's full path in that entry, or remove the entry and re-run this script. \`ironmem doctor\` reads only $_DOCTOR_SETTINGS, not this file." >&2
    fi
  fi
}

# Is $1 one of the other arguments?
_in_list() {
  local value="$1" item
  shift
  for item in "$@"; do
    [[ "$item" == "$value" ]] && return 0
  done
  return 1
}

# Prints each argument to stderr, every line of it indented under a WARN.
_print_indented() {
  local shown line
  for shown in "$@"; do
    while IFS= read -r line; do
      echo "            $line" >&2
    done <<< "$shown"
  done
}

# Says whether the mining hooks just added ($3, e.g. "Stop PreCompact") mine.
# The commands carry no mode. settings file $2 (work copy $1) is read first,
# as doctor reads it: an env.IRONMEM_MCP_MODE that reads as trusted (trimmed,
# any case, as McpAccessMode::parse reads it) reaches them, and any other
# string there beats the environment, so they do not mine. With no string
# there, the environment Claude Code is launched with decides, which this
# script cannot see, so the line names it. A jq error prints a WARN saying the
# script cannot tell, with jq's first line.
_print_mining_line() {
  local work="$1" settings="$2" mining="$3" see="" mode err_file="${1%/*}/mining.err"
  if _doctor_reads "$settings"; then
    see=" (see \`ironmem doctor\`)"
  fi
  if ! mode="$(jq -r '[.env | objects | .IRONMEM_MCP_MODE | strings]
        | if length == 0 then "unset"
          elif (.[0] | sub("^\\s+"; "") | sub("\\s+$"; "") | ascii_downcase) == "trusted" then "trusted"
          else "other" end' "$work" 2>"$err_file")"; then
    echo "    WARN: jq failed reading env.IRONMEM_MCP_MODE in $settings, so this script cannot say whether $mining mine$see." >&2
    echo "          jq said: $(_first_line "$err_file")" >&2
    return 0
  fi
  case "$mode" in
    trusted) echo "    $mining get a trusted IRONMEM_MCP_MODE from env in $settings, so they mine$see." ;;
    other) echo "    $mining do not mine: env in $settings sets IRONMEM_MCP_MODE to a value other than trusted$see." ;;
    unset) echo "    $mining run read-only and skip mining (prompt recall still works) unless the environment Claude Code is launched with sets IRONMEM_MCP_MODE; trusted there makes them mine$see." ;;
  esac
}

# Prints the WARN for a settings.json that sets disableAllHooks to true.
_print_all_hooks_disabled() {
  echo "    WARN: $1 sets disableAllHooks to true, so Claude Code runs none of these hooks unless a project's settings set it back to false. To turn them on, remove disableAllHooks from settings.json or set it to false." >&2
}

# Prints the file symlink $1 finally points at, or $1 itself when it is not a
# symlink. A relative link target is read against the link's own directory.
# Follows at most 40 links; exit 1 past that or on a readlink error.
_resolve_link() {
  local path="$1" link hops=0
  while [[ -L "$path" ]]; do
    if (( hops >= 40 )); then
      return 1
    fi
    hops=$((hops + 1))
    link="$(readlink "$path")" || return 1
    case "$link" in
      /*) path="$link" ;;
      *)
        if [[ "$path" == */* ]]; then
          path="${path%/*}/$link"
        else
          path="$link"
        fi
        ;;
    esac
  done
  printf '%s' "$path"
}

# The temp directory and the temp file next to settings.json that a wiring
# run has open, global so the signal traps in wire_claude_hooks can remove
# them after the functions that made them have returned.
_HOOKS_TMP_DIR=""
_HOOKS_SETTINGS_TMP=""

_wire_claude_hooks_cleanup() {
  if [[ -n "$_HOOKS_SETTINGS_TMP" ]]; then
    rm -f "$_HOOKS_SETTINGS_TMP"
  fi
  if [[ -n "$_HOOKS_TMP_DIR" ]]; then
    rm -rf "$_HOOKS_TMP_DIR"
  fi
}

# Appends the plugin hooks missing from settings file $1, skipping entries it
# cannot read (only a file that is not a JSON object, or whose hooks or an
# event's value has the wrong type, gets a WARN). Never fails the install:
# every problem prints a WARN and returns 0. The temp files live in a private
# mktemp -d directory, so no other user can plant a file at their names. An
# interrupt, TERM or HUP removes them (and the temp file next to settings.json)
# and ends the install, as the signal would have.
wire_claude_hooks() {
  local settings="$1"
  trap '_wire_claude_hooks_cleanup; exit 130' INT
  trap '_wire_claude_hooks_cleanup; exit 143' TERM
  trap '_wire_claude_hooks_cleanup; exit 129' HUP
  if ! _HOOKS_TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ironmem-hooks.XXXXXX")"; then
    _HOOKS_TMP_DIR=""
    trap - INT TERM HUP
    _wire_claude_hooks_warn "could not create a temp directory" "$settings"
    return 0
  fi
  _wire_claude_hooks_into "$settings" "$_HOOKS_TMP_DIR/settings.json" "$_HOOKS_TMP_DIR/next.json" || true
  rm -rf "$_HOOKS_TMP_DIR" 2>/dev/null || echo "    WARN: could not remove the temp directory $_HOOKS_TMP_DIR" >&2
  _HOOKS_TMP_DIR=""
  trap - INT TERM HUP
  return 0
}

# Builds the whole result in $work (each step via $next), then writes
# $settings once.
#
# errexit is OFF in here: wire_claude_hooks calls this under `|| true`, which
# disables `set -e` for this function and everything it calls. A failing
# command does not stop it, so check the status of every command that can
# fail, and on failure WARN and return 0.
_wire_claude_hooks_into() {
  local settings="$1" work="$2" next="$3" err_file="${2%/*}/jq.err"
  local dir real real_dir before now pair event name rc err shape
  local added="" kept="" mining="" wired_too="" root_copies="" disabled=0

  if [[ -L "$settings" && ! -e "$settings" ]]; then
    _wire_claude_hooks_warn "$settings is a dangling symlink" "$settings"
    return 0
  fi
  if [[ -e "$settings" ]]; then
    if [[ ! -f "$settings" || ! -w "$settings" || ! -r "$settings" ]]; then
      _wire_claude_hooks_warn "$settings is not a readable, writable file" "$settings"
      return 0
    fi
    # The new file is renamed onto the file a symlink chain ends at, so the
    # links stay links and that file's directory must take a new name.
    # `-ef`: the file found must be the one $settings opens. A link target
    # that ends in a newline loses it in the command substitutions, and the
    # rename would then land on another file.
    if ! real="$(_resolve_link "$settings")" || [[ -z "$real" || ! "$real" -ef "$settings" ]]; then
      _wire_claude_hooks_warn "could not follow the symlinks from $settings" "$settings"
      return 0
    fi
    if ! real_dir="$(dirname "$real")" || [[ ! -w "$real_dir" ]]; then
      _wire_claude_hooks_warn "cannot write a new file in $real_dir, the directory holding $real" "$settings"
      return 0
    fi
    cp "$settings" "$work" 2>/dev/null || { _wire_claude_hooks_warn "could not read $settings" "$settings"; return 0; }
    if ! before="$(cksum < "$work")"; then
      _wire_claude_hooks_warn "could not checksum $settings" "$settings"
      return 0
    fi
  else
    if ! dir="$(dirname "$settings")" || ! mkdir -p "$dir" 2>/dev/null || [[ ! -w "$dir" ]]; then
      _wire_claude_hooks_warn "cannot create $settings" "$settings"
      return 0
    fi
    real="$settings"
    real_dir="$dir"
    before="missing"
    if ! printf '{}\n' > "$work"; then
      _wire_claude_hooks_warn "could not write a temp file for $settings" "$settings"
      return 0
    fi
  fi

  # One pass over every JSON value in the file (-s), so a file holding two
  # values is refused rather than read as two objects and rewritten as two.
  # Not `.hooks // {}`: `//` reads false as absent, and the matcher below then
  # fails on `"hooks": false`; doctor reports that as not an object.
  shape="$(jq -r -s 'if length == 0 then "empty"
        elif length > 1 then "many"
        elif (.[0] | type == "object" and ((if .hooks == null then {} else .hooks end)
              | type == "object" and all(.[]; type == "array"))) then "ok"
        else "shape" end' "$work" 2>"$err_file")" && rc=0 || rc=$?
  if [[ "$rc" -ne 0 ]]; then
    _wire_claude_hooks_warn "jq could not read $settings as JSON" "$settings" "$(_first_line "$err_file")"
    return 0
  fi
  case "$shape" in
    ok) ;;
    empty)
      _wire_claude_hooks_warn "$settings holds no JSON value (it is empty or only whitespace)" "$settings"
      return 0
      ;;
    many)
      _wire_claude_hooks_warn "$settings holds more than one JSON value" "$settings"
      return 0
      ;;
    *)
      _wire_claude_hooks_warn "$settings is not a JSON object with array-valued hooks" "$settings"
      return 0
      ;;
  esac

  # Exit 0 when disableAllHooks is true. The hooks are still wired: a
  # project's settings can set it back to false, and then these run.
  jq -e '.disableAllHooks == true' "$work" >/dev/null 2>"$err_file" && rc=0 || rc=$?
  if [[ "$rc" -eq 0 ]]; then
    disabled=1
  elif [[ "$rc" -ne 1 ]]; then
    _wire_claude_hooks_warn "jq failed reading disableAllHooks in $settings" "$settings" "$(_first_line "$err_file")"
    return 0
  fi

  # Exit 0 enabled, 1 not (a non-object enabledPlugins counts as not), else a
  # jq error, which must not be read as "not enabled" and wire a duplicate set.
  err="$(jq -e '[(.enabledPlugins // {}) | objects | to_entries[]
          | select((.key == "ironmem" or (.key | startswith("ironmem@"))) and .value == true)]
         | length > 0' "$work" 2>&1 >/dev/null)" && rc=0 || rc=$?
  if [[ "$rc" -eq 0 ]]; then
    echo "    Claude hooks come from the enabled ironmem plugin; wired none in $settings"
    # Hand-wired copies run beside the plugin's where both start; this cannot
    # tell whether they do, so the WARN is hedged and doctor says which.
    for pair in "${CLAUDE_HOOK_EVENTS[@]}"; do
      event="${pair%%:*}"
      name="${pair#*:}"
      err="$(_claude_hook_is_wired "$work" "$event" "$name" 2>&1)" && rc=0 || rc=$?
      if [[ "$rc" -eq 0 ]]; then
        wired_too+="$event "
      elif [[ "$rc" -ne 1 ]]; then
        _wire_claude_hooks_warn "jq failed reading the $event hooks in $settings" "$settings" "$err"
        return 0
      fi
    done
    if [[ -n "$wired_too" ]]; then
      if _doctor_reads "$settings"; then
        echo "    WARN: $settings also hand-wires ${wired_too% }, which the enabled ironmem plugin supplies; where both copies start, the hook runs twice. Remove the hand-wired ironmem entries from settings.json. \`ironmem doctor\` says which." >&2
      else
        echo "    WARN: $settings also hand-wires ${wired_too% }, which the enabled ironmem plugin supplies; where both copies start, the hook runs twice. Remove the hand-wired ironmem entries from settings.json." >&2
      fi
    fi
    if [[ "$disabled" -eq 1 ]]; then
      _print_all_hooks_disabled "$settings"
    fi
    return 0
  elif [[ "$rc" -ne 1 ]]; then
    _wire_claude_hooks_warn "jq failed reading enabledPlugins in $settings" "$settings" "$err"
    return 0
  fi

  for pair in "${CLAUDE_HOOK_EVENTS[@]}"; do
    event="${pair%%:*}"
    name="${pair#*:}"
    err="$(_claude_hook_is_wired "$work" "$event" "$name" 2>&1)" && rc=0 || rc=$?
    if [[ "$rc" -eq 0 ]]; then
      kept+="$event "
      _warn_other_program "$work" "$settings" "$event" "$name"
      continue
    elif [[ "$rc" -ne 1 ]]; then
      _wire_claude_hooks_warn "jq failed reading the $event hooks in $settings" "$settings" "$err"
      return 0
    fi
    err="$(_claude_hook_has_plugin_root_copy "$work" "$event" "$name" 2>&1)" && rc=0 || rc=$?
    if [[ "$rc" -eq 0 ]]; then
      root_copies+="$event "
    elif [[ "$rc" -ne 1 ]]; then
      echo "    WARN: jq failed checking the $event hooks in $settings for a \${CLAUDE_PLUGIN_ROOT} command; adding the hook anyway." >&2
      echo "          jq said: ${err%%$'\n'*}" >&2
    fi
    if ! err="$(jq --arg ev "$event" --arg cmd "$HOOK_PROGRAM hook $name --harness claude-code" \
      '.hooks = (.hooks // {}) | .hooks[$ev] = ((.hooks[$ev] // []) + [{hooks: [{type: "command", command: $cmd}]}])' \
      "$work" 2>&1 > "$next")" || [[ ! -s "$next" ]]; then
      _wire_claude_hooks_warn "jq failed adding the $event hook to $settings" "$settings" "$err"
      return 0
    fi
    if ! mv -f "$next" "$work" 2>/dev/null; then
      _wire_claude_hooks_warn "could not stage the $event hook for $settings" "$settings"
      return 0
    fi
    added+="$event "
    case "$event" in Stop | PreCompact) mining+="$event " ;; esac
  done

  if [[ -n "$added" ]]; then
    if ! err="$(jq -e 'type == "object"' "$work" 2>&1 >/dev/null)" || [[ ! -s "$work" ]]; then
      _wire_claude_hooks_warn "the merged $settings was not a JSON object" "$settings" "$err"
      return 0
    fi
    # Another program may have written the file since it was read; writing
    # now would drop that change. Not a lock: a write between this check and
    # the rename below is still lost.
    if [[ "$before" == "missing" ]]; then
      if [[ -e "$settings" || -L "$settings" ]]; then
        now="present"
      else
        now="missing"
      fi
    elif ! now="$(cksum < "$settings" 2>/dev/null)"; then
      now=""
    fi
    if [[ "$now" != "$before" ]]; then
      _wire_claude_hooks_warn "$settings changed while this script ran" "$settings"
      return 0
    fi
    # A temp file in the target's own directory, renamed onto it: a reader
    # sees the old or the new whole file, and a failed write leaves the file
    # as it was. cp -p first carries the old file's mode (and, as far as cp
    # preserves them, owner and ACLs) onto it; a new file keeps mktemp's 0600.
    if ! _HOOKS_SETTINGS_TMP="$(mktemp "$real_dir/.settings.json.XXXXXX" 2>/dev/null)"; then
      _HOOKS_SETTINGS_TMP=""
      _wire_claude_hooks_warn "could not create a temp file in $real_dir" "$settings"
      return 0
    fi
    if { [[ ! -e "$real" ]] || cp -p "$real" "$_HOOKS_SETTINGS_TMP" 2>/dev/null; } \
      && cat "$work" > "$_HOOKS_SETTINGS_TMP" 2>/dev/null \
      && mv -f "$_HOOKS_SETTINGS_TMP" "$real" 2>/dev/null; then
      _HOOKS_SETTINGS_TMP=""
    else
      rm -f "$_HOOKS_SETTINGS_TMP" 2>/dev/null
      _HOOKS_SETTINGS_TMP=""
      _wire_claude_hooks_warn "could not write $settings" "$settings"
      return 0
    fi
    echo "==> Wired Claude hooks in $settings: ${added% }"
    for event in $root_copies; do
      echo "    WARN: $event in $settings already has an ironmem hook entry that uses \${CLAUDE_PLUGIN_ROOT}, a variable Claude Code sets only for plugin hooks, so that entry does not run outside the plugin. This install added its own $event hook beside it; you can delete the old entry." >&2
    done
    if [[ -n "$mining" && "$disabled" -eq 0 ]]; then
      _print_mining_line "$work" "$settings" "${mining% }"
    fi
  fi
  if [[ -n "$kept" ]]; then
    echo "    Claude hooks already wired, left as they are: ${kept% }"
  fi
  if [[ "$disabled" -eq 1 ]]; then
    _print_all_hooks_disabled "$settings"
  fi
}

if [[ "$SKIP_BUILD" -eq 0 ]]; then
  echo "==> Building ironmem release"
  (cd "$REPO_ROOT" && cargo build --release -p ironmem --bin ironmem)
fi

if [[ ! -x "$SOURCE" ]]; then
  echo "ERROR: release binary not found at $SOURCE" >&2
  echo "Run without --skip-build, or build manually first." >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"

echo "==> Installing $SOURCE → $TARGET (atomic)"
# install(1) writes a fresh inode under a temporary name in $INSTALL_DIR
# (`-m 755` sets the executable bits), and the rename onto $TARGET is atomic
# because both names are in one directory: a process already running the old
# file keeps it, and one starting now execs the old or the new whole binary.
# How install(1) itself replaces an existing file differs between BSD and GNU,
# so it is never pointed at $TARGET directly.
TARGET_NEW="$TARGET.new.$$"
if ! install -m 755 "$SOURCE" "$TARGET_NEW" || ! mv -f "$TARGET_NEW" "$TARGET"; then
  rm -f "$TARGET_NEW"
  echo "ERROR: could not install $SOURCE as $TARGET" >&2
  exit 1
fi

echo "==> Verifying installed binary"
if ! VERSION_OUTPUT=$("$TARGET" --version 2>&1); then
  echo "ERROR: installed binary at $TARGET failed to run" >&2
  echo "$VERSION_OUTPUT" >&2
  exit 1
fi
echo "    $VERSION_OUTPUT"

CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"
CLAUDE_HOME="${CLAUDE_HOME:-$HOME/.claude}"

if [[ "$SKIP_SKILLS" -eq 0 ]]; then
  if [[ "$FORCE_SKILLS" -eq 1 ]]; then
    echo "==> --force-skills is no longer required; bundled files update by default"
  fi

  CODEX_SKILLS_DIR="${CODEX_SKILLS_DIR:-$CODEX_HOME/skills}"
  CLAUDE_SKILLS_DIR="${CLAUDE_SKILLS_DIR:-$CLAUDE_HOME/skills}"
  CLAUDE_AGENTS_DIR="${CLAUDE_AGENTS_DIR:-$CLAUDE_HOME/agents}"
  CLAUDE_COMMANDS_DIR="${CLAUDE_COMMANDS_DIR:-$CLAUDE_HOME/commands}"
  CLAUDE_PROMPTS_DIR="${CLAUDE_PROMPTS_DIR:-$CLAUDE_HOME/prompts}"
  CLAUDE_WORKFLOWS_DIR="${CLAUDE_WORKFLOWS_DIR:-$CLAUDE_HOME/workflows}"
  CODEX_COMMANDS_DIR="${CODEX_COMMANDS_DIR:-$CODEX_HOME/commands}"
  CODEX_PROMPTS_DIR="${CODEX_PROMPTS_DIR:-$CODEX_HOME/prompts}"

  # Validated before any of this block's installers run (rather than relying
  # solely on install_ext_set's own preflight) so a missing bundled workflow
  # aborts before the Codex install and MCP wiring below ever start, instead
  # of leaving the run half-applied at whichever stage happened to hit it.
  validate_packaged_workflows "Claude" "$REPO_ROOT/.claude-plugin/workflows"

  install_skill_set "Codex" "$REPO_ROOT/.codex-plugin/skills" "$CODEX_SKILLS_DIR" \
    "$CODEX_HOME/.ironmem-bases/skills" \
    "${REQUIRED_SHARED_SKILLS[@]}" "${REQUIRED_CODEX_SKILLS[@]}"
  install_skill_set "Claude" "$REPO_ROOT/.claude-plugin/skills" "$CLAUDE_SKILLS_DIR" \
    "$CLAUDE_HOME/.ironmem-bases/skills" \
    "${REQUIRED_SHARED_SKILLS[@]}"
  if (( ${#REQUIRED_CLAUDE_SKILLS[@]} > 0 )); then
    install_skill_set "Claude" "$REPO_ROOT/.claude-plugin/skills" "$CLAUDE_SKILLS_DIR" \
      "$CLAUDE_HOME/.ironmem-bases/skills" \
      "${REQUIRED_CLAUDE_SKILLS[@]}"
  fi
  remove_legacy_skills "Codex" "$CODEX_SKILLS_DIR" "$CODEX_HOME/.ironmem-bases/skills"
  remove_legacy_skills "Claude" "$CLAUDE_SKILLS_DIR" "$CLAUDE_HOME/.ironmem-bases/skills"
  install_agent_set "Claude" "$REPO_ROOT/.claude-plugin/agents" "$CLAUDE_AGENTS_DIR" \
    "$CLAUDE_HOME/.ironmem-bases/agents"
  install_md_set "Claude command" "$REPO_ROOT/.claude-plugin/commands" \
    "$CLAUDE_COMMANDS_DIR" "$CLAUDE_HOME/.ironmem-bases/commands" \
    "${REQUIRED_CLAUDE_COMMANDS[@]}"
  install_md_set "Claude prompt" "$REPO_ROOT/.claude-plugin/prompts" \
    "$CLAUDE_PROMPTS_DIR" "$CLAUDE_HOME/.ironmem-bases/prompts" \
    "${REQUIRED_CLAUDE_PROMPTS[@]}"
  install_ext_set "Claude workflow" js "$REPO_ROOT/.claude-plugin/workflows" \
    "$CLAUDE_WORKFLOWS_DIR" "$CLAUDE_HOME/.ironmem-bases/workflows" \
    "${REQUIRED_CLAUDE_WORKFLOWS[@]}"
  install_md_set "Codex command" "$REPO_ROOT/.codex-plugin/commands" \
    "$CODEX_COMMANDS_DIR" "$CODEX_HOME/.ironmem-bases/commands" \
    "${REQUIRED_CODEX_COMMANDS[@]}"
  install_md_set "Codex prompt" "$REPO_ROOT/.codex-plugin/prompts" \
    "$CODEX_PROMPTS_DIR" "$CODEX_HOME/.ironmem-bases/prompts" \
    "${REQUIRED_CODEX_PROMPTS[@]}"
  # Muse last: a failed managed-store install exits 1, and running it here
  # keeps that failure from skipping any other harness's file installs. MCP
  # wiring below is still skipped on failure -- a kind that installed
  # nothing must fail the run.
  install_muse_skills "$REPO_ROOT/.muse-plugin/skills" \
    "${REQUIRED_SHARED_SKILLS[@]}"
else
  echo "==> Skipping skill / command / prompt install"
fi

# MCP server registration. Split from skills because a sysadmin may want to
# install the files but wire MCP themselves.
if [[ "$SKIP_WIRING" -eq 0 ]]; then
  CLAUDE_CONFIG_JSON="${CLAUDE_CONFIG_JSON:-$HOME/.claude.json}"
  CLAUDE_SETTINGS_JSON="${CLAUDE_SETTINGS_JSON:-$CLAUDE_HOME/settings.json}"
  CODEX_CONFIG_TOML="${CODEX_CONFIG_TOML:-$CODEX_HOME/config.toml}"

  # Muse reads $XDG_CONFIG_HOME/muse/settings.json when that variable is set
  # and non-empty, and ~/.config/muse/settings.json otherwise -- never both, so
  # writing the wrong one registers nothing. Mirrors the same env-override-wins
  # table as `launcher::json_mcpservers_config_path_for`, which was measured
  # live: an isolated-XDG_CONFIG_HOME Muse run never consulted ~/.config/muse.
  if [[ -n "${XDG_CONFIG_HOME:-}" ]]; then
    MUSE_CONFIG_DEFAULT="$XDG_CONFIG_HOME/muse/settings.json"
  else
    MUSE_CONFIG_DEFAULT="$HOME/.config/muse/settings.json"
  fi
  MUSE_CONFIG_JSON="${MUSE_CONFIG_JSON:-$MUSE_CONFIG_DEFAULT}"

  # ---- Claude Code and Muse Code: object-shaped mcpServers ------------------
  if ! command -v jq >/dev/null 2>&1; then
    echo "==> WARN: jq not installed; skipping Claude and Muse MCP registration checks." >&2
    echo "          Install jq, or add this manually to $CLAUDE_CONFIG_JSON:" >&2
    echo "          { \"mcpServers\": { \"ironmem\": { \"command\": \"$TARGET\", \"args\": [\"serve\", \"--connect\", \"$DAEMON_SOCKET_PATH\"], \"env\": { \"IRONMEM_MCP_MODE\": \"trusted\" } } } }" >&2
    echo "          ...and the same to $MUSE_CONFIG_JSON, with \"mode\": \"optional\" added to" >&2
    echo "          the ironmem entry so a stale command cannot abort a Muse session, and" >&2
    echo "          \"schema_version\": 1 alongside \"mcpServers\" if you are creating that file:" >&2
    echo "          { \"schema_version\": 1, \"mcpServers\": { \"ironmem\": { \"command\": \"$TARGET\", \"args\": [\"serve\", \"--connect\", \"$DAEMON_SOCKET_PATH\"], \"env\": { \"IRONMEM_MCP_MODE\": \"trusted\" }, \"mode\": \"optional\" } } }" >&2
    if [[ "$SKIP_HOOKS" -eq 0 ]]; then
      echo "          Claude Code hooks were not wired either. Add these four to $CLAUDE_SETTINGS_JSON (skip this if the ironmem Claude Code plugin is enabled)" >&2
      echo "          under \"hooks\", one key each for SessionStart, UserPromptSubmit, Stop and PreCompact, shaped as" >&2
      echo "          \"<Event>\": [ { \"hooks\": [ { \"type\": \"command\", \"command\": \"$(_json_string_body "$HOOK_PROGRAM") hook <session-start|user-prompt-submit|stop|precompact> --harness claude-code\" } ] } ]" >&2
    fi
  else
    register_json_mcpservers "Claude" "$CLAUDE_CONFIG_JSON" '{}' '{}'
    register_json_mcpservers "Muse" "$MUSE_CONFIG_JSON" \
      '{"schema_version": 1}' '{"mode": "optional"}'
    if [[ "$SKIP_HOOKS" -eq 0 ]]; then
      wire_claude_hooks "$CLAUDE_SETTINGS_JSON"
    fi
  fi
  if [[ "$SKIP_HOOKS" -eq 1 ]]; then
    echo "==> Skipping Claude Code hook wiring (--skip-hooks)"
  fi

  # ---- Codex: ~/.codex/config.toml [mcp_servers.ironmem] --------------------
  # TOML editing without a TOML parser is fragile, so we only ever append a
  # missing block — we never rewrite an existing one. --force-wiring is a
  # no-op here and prints a manual-edit hint instead.
  if [[ ! -f "$CODEX_CONFIG_TOML" ]]; then
    mkdir -p "$(dirname "$CODEX_CONFIG_TOML")"
    : > "$CODEX_CONFIG_TOML"
  fi

  if grep -q '^\[mcp_servers\.ironmem\]' "$CODEX_CONFIG_TOML"; then
    if [[ "$FORCE_WIRING" -eq 1 ]]; then
      echo "    WARN: --force-wiring cannot safely rewrite TOML in place." >&2
      echo "          Edit $CODEX_CONFIG_TOML by hand to point command = \"$TARGET\"." >&2
    else
      echo "    'ironmem' MCP server already registered for Codex"
    fi
  else
    echo "==> Appending [mcp_servers.ironmem] to $CODEX_CONFIG_TOML"
    {
      echo ""
      echo "[mcp_servers.ironmem]"
      echo "command = \"$TARGET\""
      echo "args = [\"serve\", \"--connect\", \"$DAEMON_SOCKET_PATH\"]"
      echo ""
      echo "[mcp_servers.ironmem.env]"
      echo "IRONMEM_MCP_MODE = \"trusted\""
    } >> "$CODEX_CONFIG_TOML"
  fi

else
  echo "==> Skipping MCP wiring"
fi

# Retire the shared daemon so the next MCP client runs the binary we just
# installed.
#
# The atomic install above deliberately does not disturb running processes, but
# the shared daemon (`ironmem serve --listen <socket>`) is long-lived: it keeps
# executing the OLD binary for as long as it stays up, and its only self-exit is
# the idle timer, which fires solely at ZERO active connections. An always-on
# Claude Code session holds one open continuously, so that timer never fires --
# daemons have been observed still serving pre-upgrade code 23 hours after an
# install. Printing an FYI does not fix that; signalling does.
#
# SIGTERM is a GRACEFUL RETIRE, not a kill -- IN A DAEMON THAT HAS THE RETIRE
# HANDLER (see `retire_on_signal` / `serve_accept_loop` in
# crates/ironmem/src/mcp/daemon.rs). Such a daemon stops admitting new clients
# immediately, but every connection already attached when the signal lands is
# served to its natural end and the process only exits once the last one
# disconnects. So it does NOT break anyone's live session: an attached client
# keeps working (on the old build, until it is restarted), while
# any NEW client is refused, auto-spawns a fresh daemon, and gets this build.
# Repeat signals are no-ops there, so re-signalling a daemon that is still
# draining a days-old session across several installs is safe.
#
# THE ONE-TIME CUTOVER. A daemon that PREDATES the retire handler has no
# handler at all, so SIGTERM takes its default disposition and terminates it on
# the spot, cutting off whatever was attached. That is guaranteed on the first
# install of this change -- the daemon running right then is by definition the
# old binary -- and the script cannot tell the two cases apart: the running
# process's argv, socket, and pid are identical either way, and nothing the
# daemon exposes reports its build. Probing indirectly (marker files vs process
# start times, version guesses) would be fragile and could only ever change the
# wording, never the outcome, since signalling is the point. So the message
# below states both cases plainly instead of claiming the good one. Cost of the
# cutover: restart the affected client session once. Every install
# after it is graceful for real.
#
# No "is it idle?" pre-check is needed here -- and none is possible anyway,
# since nothing the daemon exposes reports its connection count.
#
# Only `--listen` processes are the upgrade-staleness concern. `serve --connect`
# processes are per-client shims holding no daemon state; they die with their
# MCP client on their own, and reporting them was actively misleading (nine
# "running" processes, seven of them transient shims). They are neither
# signalled nor mentioned.

# PIDs of daemons listening on THIS install's socket -- never someone else's
# daemon on an unrelated socket. `pgrep -f` matches a regex against the whole
# command line, so the exact `--listen` / socket-path discrimination is done in
# bash against `ps` output, where "$DAEMON_SOCKET_PATH" is matched literally
# rather than as a pattern.
daemon_pids_on_our_socket() {
  local pid args
  # No matches makes pgrep exit 1; `|| true` keeps `set -e` from aborting.
  for pid in $(pgrep -f 'ironmem serve' 2>/dev/null || true); do
    args="$(ps -o args= -p "$pid" 2>/dev/null || true)"
    # A `--connect` shim never carries `--listen`, so this is the whole
    # daemon-vs-shim test.
    case "$args" in
      *--listen*) ;;
      *) continue ;;
    esac
    case "$args" in
      *"$DAEMON_SOCKET_PATH"*) printf '%s\n' "$pid" ;;
    esac
  done
  # Explicit success: the loop's last command may have been a non-matching
  # `case`, and a non-zero return here would abort the command substitution.
  return 0
}

DAEMON_PIDS="$(daemon_pids_on_our_socket)"
if [[ -n "$DAEMON_PIDS" ]]; then
  echo ""
  echo "==> Retiring the shared ironmem daemon so new clients get this build"
  for pid in $DAEMON_PIDS; do
    if kill -TERM "$pid" 2>/dev/null; then
      echo "    Signalled daemon PID $pid."
    else
      # Permission denied (a daemon owned by another user), or it exited
      # between the pgrep and here. Never fatal: the binary is already
      # installed and correct, only the daemon refresh was missed.
      echo "    WARNING: could not signal daemon PID $pid (already gone, or"
      echo "             owned by another user). It keeps serving the previous"
      echo "             binary until it exits on its own."
    fi
  done
  echo ""
  echo "    New Claude Code / Codex / Muse sessions auto-spawn a daemon running"
  echo "    the freshly installed binary. What happens to sessions attached"
  echo "    RIGHT NOW depends on which build the signalled daemon was running,"
  echo "    and this script cannot tell from the outside:"
  echo "      - Built from this change onward: SIGTERM is a graceful retire."
  echo "        Every attached session is served to its end (on the previous"
  echo "        build) and the daemon exits only once the last one detaches."
  echo "      - Built before it: there is no signal handler, so SIGTERM takes"
  echo "        its default disposition and the daemon exits immediately,"
  echo "        DROPPING any attached session. This is a one-time cost on the"
  echo "        first install of graceful retire -- restart the affected"
  echo "        client and it reconnects to a fresh daemon."
fi

# Detect legacy MCP server registrations left over from the pre-rename era
# (ironrace-memory → ironmem). We do NOT edit these files — a legacy entry
# may point at a forked or staging binary deliberately. Warn only, with the
# exact command to remove it.
CLAUDE_CONFIG="$HOME/.claude.json"
CODEX_CONFIG="$HOME/.codex/config.toml"
LEGACY_FOUND=0

if [[ -f "$CLAUDE_CONFIG" ]] && command -v jq >/dev/null 2>&1; then
  if jq -e '.mcpServers["ironrace-memory"]' "$CLAUDE_CONFIG" >/dev/null 2>&1; then
    if [[ "$LEGACY_FOUND" -eq 0 ]]; then echo ""; echo "Legacy MCP registrations detected:"; fi
    LEGACY_FOUND=1
    echo "  - Claude Code ($CLAUDE_CONFIG) has an 'ironrace-memory' server."
    echo "      Remove with: claude mcp remove ironrace-memory"
  fi
fi

if [[ -f "$CODEX_CONFIG" ]] && grep -q '^\[mcp_servers\.ironrace_memory\]' "$CODEX_CONFIG" 2>/dev/null; then
  if [[ "$LEGACY_FOUND" -eq 0 ]]; then echo ""; echo "Legacy MCP registrations detected:"; fi
  LEGACY_FOUND=1
  echo "  - Codex ($CODEX_CONFIG) has an [mcp_servers.ironrace_memory] section."
  echo "      Remove it by hand — delete the [mcp_servers.ironrace_memory] block"
  echo "      and any [mcp_servers.ironrace_memory.*] subsections."
fi

if [[ "$LEGACY_FOUND" -eq 1 ]]; then
  echo ""
  echo "  Why this matters: the plugin registers itself as 'ironmem'. When a"
  echo "  legacy 'ironrace-memory' server is also registered, tool calls render"
  echo "  under the old name and both servers run against the same SQLite DB."
fi

# Loud, unmissable summary of every packaged file install_file_with_merge left
# untouched -- a lone WARN buried in this output is easy to scroll past, but a
# stale file paired against an upgraded counterpart (e.g. ultrareview.js
# against a newer ultrareview-local.md) can silently misbehave. Not a failure
# (exit code is unaffected); just something the operator must go look at.
if (( ${#UNCHANGED_FILES[@]} > 0 )); then
  echo ""
  echo "################################################################"
  echo "# ${#UNCHANGED_FILES[@]} packaged file(s) left unchanged -- review before relying on them:"
  echo "################################################################"
  for entry in "${UNCHANGED_FILES[@]}"; do
    echo "  - $entry"
  done
  echo "################################################################"
fi

echo "==> Done"

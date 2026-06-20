#!/usr/bin/env bash
# install-ironmem.sh — atomically install the ironmem binary to ~/.ironrace/bin/
#
# Why this script exists: plain `cp` overwrites bytes in place, same inode.
# macOS lets that happen even while an `ironmem serve` process is actively
# executing the file; the write corrupts the running code page and any new
# invocation loading the same inode silently hangs or exits. Using install(1)
# unlinks the old file and creates a new one, so running processes keep their
# old copy and new invocations get a clean binary.
#
# The script builds release (unless --skip-build), atomically replaces
# ~/.ironrace/bin/ironmem, installs bundled Codex/Claude skill dependencies,
# and verifies the resulting binary runs.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

INSTALL_DIR="${IRONMEM_INSTALL_DIR:-$HOME/.ironrace/bin}"
TARGET="$INSTALL_DIR/ironmem"
SOURCE="$REPO_ROOT/target/release/ironmem"

REQUIRED_SHARED_SKILLS=(
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

REQUIRED_CLAUDE_AGENTS=(
  code-reviewer
)

# /collab-only command + prompt files the install must place so that a fresh
# user can run `/collab` end-to-end. The lists are intentionally minimal —
# anything broader belongs to the plugin-marketplace install path, not this
# script.
REQUIRED_CLAUDE_COMMANDS=(
  collab
  evaluate-issue
)

REQUIRED_CODEX_PROMPTS=(
  collab
  collab-batch-impl
  evaluate-issue
)

SKIP_BUILD=0
SKIP_SKILLS=0
SKIP_WIRING=0
FORCE_SKILLS=0
FORCE_WIRING=0

usage() {
  cat <<'EOF'
Usage: scripts/install-ironmem.sh [--skip-build] [--skip-skills] [--skip-wiring]
                                  [--force-skills] [--force-wiring]

Options:
  --skip-build     Install the existing target/release/ironmem binary.
  --skip-skills    Do not install bundled Codex/Claude skill, agent, command,
                   and prompt dependencies.
  --skip-wiring    Do not register the ironmem MCP server in Claude/Codex
                   config or warn about missing /ultrareview-local.
  --force-skills   Compatibility flag. Bundled skill/agent/command/prompt
                   files are updated by default; use --skip-skills to leave
                   existing copies untouched.
  --force-wiring   Replace an existing 'ironmem' MCP entry in
                   ~/.claude.json or ~/.codex/config.toml with the bundled one
                   (use only when the config has drifted from a fresh install).
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

install_skill_set() {
  local harness="$1"
  local source_root="$2"
  local target_root="$3"
  shift 3
  local skills=("$@")

  validate_packaged_skills "$harness" "$source_root" "${skills[@]}"
  mkdir -p "$target_root"

  echo "==> Installing $harness skill dependencies → $target_root"

  for skill in "${skills[@]}"; do
    local source="$source_root/$skill"
    local target="$target_root/$skill"

    if [[ ! -e "$target" ]]; then
      cp -R "$source" "$target"
      echo "    installed $skill"
      continue
    fi

    if [[ ! -d "$target" ]]; then
      echo "    WARN: $target exists but is not a directory; leaving it unchanged" >&2
      continue
    fi

    if diff -qr "$source" "$target" >/dev/null 2>&1; then
      echo "    $skill already installed"
      continue
    fi

    rm -rf "$target"
    cp -R "$source" "$target"
    echo "    updated $skill"
  done
}

install_agent_set() {
  local harness="$1"
  local source_root="$2"
  local target_root="$3"

  validate_packaged_agents "$harness" "$source_root"
  mkdir -p "$target_root"

  echo "==> Installing $harness agent dependencies → $target_root"

  for agent in "${REQUIRED_CLAUDE_AGENTS[@]}"; do
    local source="$source_root/$agent.md"
    local target="$target_root/$agent.md"

    if [[ ! -e "$target" ]]; then
      cp "$source" "$target"
      echo "    installed $agent"
      continue
    fi

    if diff -q "$source" "$target" >/dev/null 2>&1; then
      echo "    $agent already installed"
      continue
    fi

    cp "$source" "$target"
    echo "    updated $agent"
  done
}

# Generic .md installer used by commands and prompts. We don't reuse
# install_agent_set because its name list and target naming are entangled with
# the agent loop; splitting keeps each loop readable.
install_md_set() {
  local label="$1"      # human-readable kind (e.g. "Claude command")
  local source_root="$2"
  local target_root="$3"
  shift 3
  local names=("$@")

  local missing=0
  for name in "${names[@]}"; do
    if [[ ! -f "$source_root/$name.md" ]]; then
      echo "ERROR: bundled $label missing: $source_root/$name.md" >&2
      missing=1
    fi
  done
  if [[ "$missing" -eq 1 ]]; then
    exit 1
  fi

  mkdir -p "$target_root"
  echo "==> Installing ${label}s → $target_root"

  for name in "${names[@]}"; do
    local source="$source_root/$name.md"
    local target="$target_root/$name.md"

    if [[ ! -e "$target" ]]; then
      cp "$source" "$target"
      echo "    installed $name"
      continue
    fi

    if diff -q "$source" "$target" >/dev/null 2>&1; then
      echo "    $name already installed"
      continue
    fi

    cp "$source" "$target"
    echo "    updated $name"
  done
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
# install(1) unlinks the target and creates a fresh inode, safe for running
# processes. `-m 755` sets executable bits; `-C` is a no-op copy if identical.
install -m 755 "$SOURCE" "$TARGET"

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
  CODEX_PROMPTS_DIR="${CODEX_PROMPTS_DIR:-$CODEX_HOME/prompts}"

  install_skill_set "Codex" "$REPO_ROOT/.codex-plugin/skills" "$CODEX_SKILLS_DIR" \
    "${REQUIRED_SHARED_SKILLS[@]}" "${REQUIRED_CODEX_SKILLS[@]}"
  install_skill_set "Claude" "$REPO_ROOT/.claude-plugin/skills" "$CLAUDE_SKILLS_DIR" \
    "${REQUIRED_SHARED_SKILLS[@]}" "${REQUIRED_CLAUDE_SKILLS[@]}"
  install_agent_set "Claude" "$REPO_ROOT/.claude-plugin/agents" "$CLAUDE_AGENTS_DIR"
  install_md_set "Claude command" "$REPO_ROOT/.claude-plugin/commands" \
    "$CLAUDE_COMMANDS_DIR" "${REQUIRED_CLAUDE_COMMANDS[@]}"
  install_md_set "Codex prompt" "$REPO_ROOT/.codex-plugin/prompts" \
    "$CODEX_PROMPTS_DIR" "${REQUIRED_CODEX_PROMPTS[@]}"
else
  echo "==> Skipping skill / command / prompt install"
fi

# MCP server registration + /ultrareview-local preflight. Split from skills
# because a sysadmin may want to install the files but wire MCP themselves.
if [[ "$SKIP_WIRING" -eq 0 ]]; then
  CLAUDE_CONFIG_JSON="${CLAUDE_CONFIG_JSON:-$HOME/.claude.json}"
  CODEX_CONFIG_TOML="${CODEX_CONFIG_TOML:-$CODEX_HOME/config.toml}"

  # ---- Claude Code: ~/.claude.json mcpServers.ironmem -----------------------
  if ! command -v jq >/dev/null 2>&1; then
    echo "==> WARN: jq not installed; skipping Claude MCP registration check." >&2
    echo "          Install jq, or add this manually to $CLAUDE_CONFIG_JSON:" >&2
    echo "          { \"mcpServers\": { \"ironmem\": { \"command\": \"$TARGET\", \"args\": [\"serve\"] } } }" >&2
  else
    if [[ ! -f "$CLAUDE_CONFIG_JSON" ]]; then
      echo "{}" > "$CLAUDE_CONFIG_JSON"
    fi

    EXISTING_CMD="$(jq -r '.mcpServers.ironmem.command // empty' "$CLAUDE_CONFIG_JSON" 2>/dev/null || echo "")"
    if [[ -z "$EXISTING_CMD" ]]; then
      echo "==> Registering 'ironmem' MCP server in $CLAUDE_CONFIG_JSON"
      TMP="$(mktemp)"
      jq --arg cmd "$TARGET" \
        '.mcpServers = ((.mcpServers // {}) + {ironmem: {command: $cmd, args: ["serve"]}})' \
        "$CLAUDE_CONFIG_JSON" > "$TMP" && mv -f "$TMP" "$CLAUDE_CONFIG_JSON"
    elif [[ "$EXISTING_CMD" == "$TARGET" ]]; then
      echo "    'ironmem' MCP server already registered for Claude"
    elif [[ "$FORCE_WIRING" -eq 1 ]]; then
      echo "==> Replacing divergent 'ironmem' MCP entry (was: $EXISTING_CMD)"
      TMP="$(mktemp)"
      jq --arg cmd "$TARGET" \
        '.mcpServers.ironmem = {command: $cmd, args: ["serve"]}' \
        "$CLAUDE_CONFIG_JSON" > "$TMP" && mv -f "$TMP" "$CLAUDE_CONFIG_JSON"
    else
      echo "    WARN: 'ironmem' MCP entry already exists with command=$EXISTING_CMD" >&2
      echo "          Expected: $TARGET. Re-run with --force-wiring to replace." >&2
    fi
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
      echo "args = [\"serve\"]"
      echo ""
      echo "[mcp_servers.ironmem.env]"
      echo "IRONMEM_MCP_MODE = \"trusted\""
    } >> "$CODEX_CONFIG_TOML"
  fi

  # ---- /ultrareview-local preflight -----------------------------------------
  # /collab's CodeReviewLocalPending phase shells out to /ultrareview-local.
  # It ships in a separate plugin, so we only warn (don't fail) if absent.
  ULTRAREVIEW_CMD="$CLAUDE_HOME/commands/ultrareview-local.md"
  if [[ ! -f "$ULTRAREVIEW_CMD" ]]; then
    echo ""
    echo "WARN: /ultrareview-local is not installed at $ULTRAREVIEW_CMD." >&2
    echo "      /collab's CodeReviewLocalPending phase invokes it; without it the" >&2
    echo "      flow will halt there. Install it from your /ultrareview-local source" >&2
    echo "      (it is not bundled with ironmem)." >&2
  fi
else
  echo "==> Skipping MCP wiring and /ultrareview-local preflight"
fi

# Surface running `ironmem serve` instances as an FYI — the atomic install
# does not disturb them, but callers that want new clients to hit the fresh
# binary must restart their MCP client (Claude Code, Codex, etc).
RUNNING="$(pgrep -f 'ironmem serve' 2>/dev/null || true)"
if [[ -n "$RUNNING" ]]; then
  echo ""
  echo "Note: running ironmem serve process(es) detected (PIDs: $RUNNING)."
  echo "      They continue on the previous binary. Restart your MCP client"
  echo "      (Claude Code / Codex) to reconnect to the freshly installed one."
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

echo "==> Done"

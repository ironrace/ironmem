#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$PLUGIN_ROOT/.." && pwd)"

resolve_workspace_root() {
  if git -C "${PWD}" rev-parse --show-toplevel >/dev/null 2>&1; then
    git -C "${PWD}" rev-parse --show-toplevel
  else
    pwd
  fi
}

export IRONMEM_WORKSPACE_ROOT="${IRONMEM_WORKSPACE_ROOT:-$(resolve_workspace_root)}"
# Plugin users get trusted mode by default; bare `ironmem serve` defaults to read-only.
export IRONMEM_MCP_MODE="${IRONMEM_MCP_MODE:-trusted}"

# Resolve binary in priority order: installed → release build → debug build → cargo build
IRONMEM_BIN=""
if [[ -x "$HOME/.ironrace/bin/ironmem" ]]; then
  IRONMEM_BIN="$HOME/.ironrace/bin/ironmem"
elif [[ -x "$REPO_ROOT/target/release/ironmem" ]]; then
  IRONMEM_BIN="$REPO_ROOT/target/release/ironmem"
elif [[ -x "$REPO_ROOT/target/debug/ironmem" ]]; then
  IRONMEM_BIN="$REPO_ROOT/target/debug/ironmem"
fi

if [[ -z "$IRONMEM_BIN" ]]; then
  cargo build -q --manifest-path "$REPO_ROOT/Cargo.toml" -p ironmem --bin ironmem >&2
  IRONMEM_BIN="$REPO_ROOT/target/debug/ironmem"
fi

# Version consistency check: warn (but don't block) if binary version doesn't match plugin metadata.
PLUGIN_VERSION="$(python3 -c "import json; print(json.load(open('$PLUGIN_ROOT/plugin.json'))['version'])" 2>/dev/null || echo "")"
if [[ -n "$PLUGIN_VERSION" ]]; then
  BIN_VERSION="$("$IRONMEM_BIN" --version 2>/dev/null | awk '{print $2}' || echo "")"
  if [[ -n "$BIN_VERSION" && "$BIN_VERSION" != "$PLUGIN_VERSION" ]]; then
    echo "ironmem version mismatch: binary is $BIN_VERSION, plugin expects $PLUGIN_VERSION" >&2
    echo "Run: cargo build --release -p ironmem --bin ironmem && cp target/release/ironmem ~/.ironrace/bin/ironmem" >&2
    exit 1
  fi
fi

exec "$IRONMEM_BIN" "$@"

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

# Resolve binary in priority order:
#   1. Repo release build, if it is newer than the installed binary (local
#      dev convenience: a `cargo build --release` picks up without needing
#      to re-run install-ironmem.sh).
#   2. Installed binary at ~/.ironrace/bin/ironmem.
#   3. Repo release or debug build.
#   4. Fresh debug build via cargo.
INSTALLED_BIN="$HOME/.ironrace/bin/ironmem"
REPO_RELEASE_BIN="$REPO_ROOT/target/release/ironmem"
REPO_DEBUG_BIN="$REPO_ROOT/target/debug/ironmem"

IRONMEM_BIN=""
if [[ -x "$REPO_RELEASE_BIN" && -x "$INSTALLED_BIN" && "$REPO_RELEASE_BIN" -nt "$INSTALLED_BIN" ]]; then
  IRONMEM_BIN="$REPO_RELEASE_BIN"
elif [[ -x "$INSTALLED_BIN" ]]; then
  IRONMEM_BIN="$INSTALLED_BIN"
elif [[ -x "$REPO_RELEASE_BIN" ]]; then
  IRONMEM_BIN="$REPO_RELEASE_BIN"
elif [[ -x "$REPO_DEBUG_BIN" ]]; then
  IRONMEM_BIN="$REPO_DEBUG_BIN"
fi

if [[ -z "$IRONMEM_BIN" ]]; then
  cargo build -q --manifest-path "$REPO_ROOT/Cargo.toml" -p ironmem --bin ironmem >&2
  IRONMEM_BIN="$REPO_DEBUG_BIN"
fi

# Version consistency check: warn (but don't block) if binary version doesn't match plugin metadata.
PLUGIN_VERSION="$(python3 -c "import json; print(json.load(open('$PLUGIN_ROOT/plugin.json'))['version'])" 2>/dev/null || echo "")"
if [[ -n "$PLUGIN_VERSION" ]]; then
  BIN_VERSION="$("$IRONMEM_BIN" --version 2>/dev/null | awk '{print $2}' || echo "")"
  if [[ -n "$BIN_VERSION" && "$BIN_VERSION" != "$PLUGIN_VERSION" ]]; then
    echo "ironmem version mismatch: binary is $BIN_VERSION, plugin expects $PLUGIN_VERSION" >&2
    # Never recommend `cp` over a running binary: macOS overwrites bytes in
    # place (same inode), which corrupts the running code page and causes
    # new invocations to hang. scripts/install-ironmem.sh uses install(1)
    # to replace the inode atomically.
    echo "Run: $REPO_ROOT/scripts/install-ironmem.sh" >&2
    exit 1
  fi
fi

exec "$IRONMEM_BIN" "$@"

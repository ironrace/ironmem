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

if [[ -x "$HOME/.ironrace/bin/ironmem" ]]; then
  exec "$HOME/.ironrace/bin/ironmem" "$@"
fi

if [[ -x "$REPO_ROOT/target/release/ironmem" ]]; then
  exec "$REPO_ROOT/target/release/ironmem" "$@"
fi

if [[ -x "$REPO_ROOT/target/debug/ironmem" ]]; then
  exec "$REPO_ROOT/target/debug/ironmem" "$@"
fi

cargo build -q --manifest-path "$REPO_ROOT/Cargo.toml" -p ironrace-memory --bin ironmem >&2
exec "$REPO_ROOT/target/debug/ironmem" "$@"

#!/usr/bin/env bash
set -euo pipefail

HOOK_NAME="${1:?Usage: ironmem-hook.sh <hook-name>}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

INPUT_FILE="$(mktemp)"
trap 'rm -f "$INPUT_FILE"' EXIT
cat > "$INPUT_FILE"

cat "$INPUT_FILE" | "$PLUGIN_ROOT/bin/ironmem-mcp.sh" hook "$HOOK_NAME" --harness gemini

#!/usr/bin/env bash
# check-versions.sh — verify that all plugin.json version fields match
# the canonical version in crates/ironrace-memory/Cargo.toml.
#
# Usage:
#   scripts/check-versions.sh          # exits 0 if all match, 1 on mismatch
#   scripts/check-versions.sh --fix    # rewrite plugin.json files to match Cargo.toml
#
# Run this in CI and before publishing a release.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

CARGO_TOML="${REPO_ROOT}/crates/ironrace-memory/Cargo.toml"
CODEX_PLUGIN="${REPO_ROOT}/.codex-plugin/plugin.json"
CLAUDE_PLUGIN="${REPO_ROOT}/.claude-plugin/plugin.json"

read_cargo_version() {
    python3 - "$1" <<'PY'
import pathlib
import sys

try:
    import tomllib
except ModuleNotFoundError:
    print("ERROR: Python 3.11+ with tomllib is required", file=sys.stderr)
    sys.exit(1)

path = pathlib.Path(sys.argv[1])
data = tomllib.loads(path.read_text())
version = data.get("package", {}).get("version", "")
if not version:
    print(f"ERROR: could not parse package.version from {path}", file=sys.stderr)
    sys.exit(1)
print(version)
PY
}

read_plugin_version() {
    python3 - "$1" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
data = json.loads(path.read_text())
version = data.get("version", "")
if not version:
    print(f"ERROR: could not read version from {path}", file=sys.stderr)
    sys.exit(1)
print(version)
PY
}

write_plugin_version() {
    python3 - "$1" "$2" <<'PY'
import json
import pathlib
import re
import sys

path = pathlib.Path(sys.argv[1])
version = sys.argv[2]
text = path.read_text()
# Detect original indent width from the first indented line (default 2).
m = re.search(r'^( +)', text, re.MULTILINE)
indent = len(m.group(1)) if m else 2
data = json.loads(text)
data["version"] = version
path.write_text(json.dumps(data, indent=indent) + "\n")
PY
}

CARGO_VERSION=$(read_cargo_version "${CARGO_TOML}")

if [[ -z "${CARGO_VERSION}" ]]; then
    echo "ERROR: could not parse version from ${CARGO_TOML}" >&2
    exit 1
fi

echo "Canonical version (Cargo.toml): ${CARGO_VERSION}"

# ---------------------------------------------------------------------------
# Check / fix each plugin.json
# ---------------------------------------------------------------------------
MISMATCH=0

check_or_fix() {
    local file="$1"
    local plugin_version
    plugin_version=$(read_plugin_version "${file}")

    if [[ "${plugin_version}" == "${CARGO_VERSION}" ]]; then
        echo "  OK  ${file} (${plugin_version})"
    elif [[ "${FIX_MODE:-0}" == "1" ]]; then
        write_plugin_version "${file}" "${CARGO_VERSION}"
        echo " FIXED ${file}: ${plugin_version} → ${CARGO_VERSION}"
    else
        echo "MISMATCH ${file}: found \"${plugin_version}\", expected \"${CARGO_VERSION}\"" >&2
        MISMATCH=1
    fi
}

FIX_MODE=0
if [[ "${1:-}" == "--fix" ]]; then
    FIX_MODE=1
fi

check_or_fix "${CODEX_PLUGIN}"
check_or_fix "${CLAUDE_PLUGIN}"

if [[ "${MISMATCH}" -ne 0 ]]; then
    echo ""
    echo "Run  scripts/check-versions.sh --fix  to update plugin.json files." >&2
    exit 1
fi

echo "All version fields are in sync."

#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

CARGO_VERSION="$(python3 - crates/ironmem/Cargo.toml <<'PY'
import sys
try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib  # type: ignore[no-redef]
import pathlib
data = tomllib.loads(pathlib.Path(sys.argv[1]).read_text())
print(data["package"]["version"])
PY
)"

echo "Cargo.toml version: $CARGO_VERSION"

for plugin_file in .codex-plugin/plugin.json .claude-plugin/plugin.json .muse-plugin/plugin.json .grok-plugin/plugin.json .gemini-plugin/plugin.json; do
  plugin_version="$(
    python3 - "$plugin_file" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], "r", encoding="utf-8") as handle:
        data = json.load(handle)
except (OSError, json.JSONDecodeError) as exc:
    print(f"ERROR: cannot read version from {sys.argv[1]}: {exc}", file=sys.stderr)
    sys.exit(1)
try:
    print(data["version"])
except (KeyError, TypeError) as exc:
    print(f"ERROR: {sys.argv[1]} has no version key: {exc}", file=sys.stderr)
    sys.exit(1)
PY
  )"

  echo "$plugin_file version: $plugin_version"

  if [[ "$plugin_version" != "$CARGO_VERSION" ]]; then
    echo "ERROR: $plugin_file version ($plugin_version) does not match Cargo.toml ($CARGO_VERSION)"
    exit 1
  fi
done

echo "All plugin versions match Cargo.toml."

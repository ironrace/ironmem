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

for plugin_file in .codex-plugin/plugin.json .claude-plugin/plugin.json; do
  plugin_version="$(
    python3 - "$plugin_file" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    print(json.load(handle)["version"])
PY
  )"

  echo "$plugin_file version: $plugin_version"

  if [[ "$plugin_version" != "$CARGO_VERSION" ]]; then
    echo "ERROR: $plugin_file version ($plugin_version) does not match Cargo.toml ($CARGO_VERSION)"
    exit 1
  fi
done

echo "All plugin versions match Cargo.toml."

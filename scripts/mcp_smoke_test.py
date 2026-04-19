#!/usr/bin/env python3
"""Run a live MCP initialize smoke test against an ironmem server process."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        help="Path to an already-built ironmem binary. If omitted, uses cargo run.",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=180,
        help="Process timeout in seconds (default: 180).",
    )
    return parser.parse_args()


def build_command(binary: str | None) -> list[str]:
    if binary:
        return [binary, "serve"]
    return ["cargo", "run", "-q", "-p", "ironmem", "--bin", "ironmem", "--", "serve"]


def main() -> int:
    args = parse_args()
    command = build_command(args.binary)

    request = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {},
    }

    with tempfile.TemporaryDirectory(prefix="ironmem-smoke-") as temp_dir:
        env = os.environ.copy()
        env.update(
            {
                "IRONMEM_EMBED_MODE": "noop",
                "IRONMEM_AUTO_BOOTSTRAP": "0",
                "IRONMEM_DISABLE_MIGRATION": "1",
                "IRONMEM_MCP_MODE": "trusted",
                "IRONMEM_DB_PATH": str(Path(temp_dir) / "memory.sqlite3"),
            }
        )

        try:
            completed = subprocess.run(
                command,
                input=json.dumps(request) + "\n",
                text=True,
                capture_output=True,
                env=env,
                timeout=args.timeout,
                check=False,
            )
        except subprocess.TimeoutExpired:
            sys.stderr.write(f"ironmem process timed out after {args.timeout}s\n")
            return 1

    if completed.returncode != 0:
        sys.stderr.write("ironmem process failed\n")
        sys.stderr.write(completed.stderr)
        return completed.returncode

    response_line = next(
        (line.strip() for line in completed.stdout.splitlines() if line.strip()),
        None,
    )
    if not response_line:
        sys.stderr.write("No JSON-RPC response received from ironmem\n")
        sys.stderr.write(completed.stderr)
        return 1

    response = json.loads(response_line)
    result = response.get("result", {})

    if response.get("error") is not None:
        sys.stderr.write(f"FAIL: initialize returned an error: {response['error']}\n")
        return 1
    if result.get("protocolVersion") != "2024-11-05":
        sys.stderr.write(f"FAIL: unexpected protocolVersion: {result.get('protocolVersion')!r}\n")
        return 1
    if result.get("serverInfo", {}).get("name") != "ironmem":
        sys.stderr.write(f"FAIL: unexpected server name: {result.get('serverInfo')!r}\n")
        return 1
    if "tools" not in result.get("capabilities", {}):
        sys.stderr.write(f"FAIL: missing tools capabilities: {result.get('capabilities')!r}\n")
        return 1

    print("MCP initialize smoke test passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

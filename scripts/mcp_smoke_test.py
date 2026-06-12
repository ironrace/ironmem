#!/usr/bin/env python3
"""Run a live MCP initialize smoke test against an ironmem server process."""

from __future__ import annotations

import argparse
import json
import os
import sqlite3
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


def build_hook_command(binary: str | None) -> list[str]:
    if binary:
        return [binary, "hook", "session-start", "--harness", "claude-code"]
    return [
        "cargo", "run", "-q", "-p", "ironmem", "--bin", "ironmem", "--",
        "hook", "session-start", "--harness", "claude-code",
    ]


def run_mcp_requests(
    command: list[str],
    requests: list[dict],
    env: dict[str, str],
    timeout: int,
) -> list[dict]:
    """Send multiple JSON-RPC requests to the server over stdio; return parsed responses."""
    stdin_payload = "\n".join(json.dumps(r) for r in requests) + "\n"
    try:
        completed = subprocess.run(
            command,
            input=stdin_payload,
            text=True,
            capture_output=True,
            env=env,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired:
        sys.stderr.write(f"ironmem process timed out after {timeout}s\n")
        sys.exit(1)

    if completed.returncode != 0:
        sys.stderr.write("ironmem process failed\n")
        sys.stderr.write(completed.stderr)
        sys.exit(completed.returncode)

    responses = []
    for line in completed.stdout.splitlines():
        line = line.strip()
        if line:
            responses.append(json.loads(line))
    return responses


def main() -> int:
    args = parse_args()
    command = build_command(args.binary)

    with tempfile.TemporaryDirectory(prefix="ironmem-smoke-") as temp_dir:
        db_path = str(Path(temp_dir) / "memory.sqlite3")
        session_id = "smoke-session-001"

        base_env = {
            **os.environ,
            "IRONMEM_EMBED_MODE": "noop",
            "IRONMEM_AUTO_BOOTSTRAP": "0",
            "IRONMEM_DISABLE_MIGRATION": "1",
            "IRONMEM_MCP_MODE": "trusted",
            "IRONMEM_METRICS": "1",
            "IRONMEM_DB_PATH": db_path,
            "IRONMEM_SESSION_ID": session_id,
        }

        # ------------------------------------------------------------------ #
        # Phase 1: initialize + collab_start                                  #
        # (Two separate server invocations so we can thread the session_id    #
        # returned by collab_start into the collab_status call.)              #
        # ------------------------------------------------------------------ #
        phase1_requests = [
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {"sessionId": session_id},
            },
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "collab_start",
                    "arguments": {
                        "repo_path": "/tmp/smoke",
                        "branch": "smoke",
                        "initiator": "claude",
                        "task": "smoke",
                    },
                },
            },
        ]
        phase1_responses = run_mcp_requests(
            command, phase1_requests, base_env, args.timeout
        )

        if len(phase1_responses) < 2:
            sys.stderr.write(
                f"FAIL: expected 2 responses, got {len(phase1_responses)}\n"
            )
            return 1

        init_resp = phase1_responses[0]
        collab_start_resp = phase1_responses[1]

        # Validate initialize response
        if init_resp.get("error") is not None:
            sys.stderr.write(
                f"FAIL: initialize returned an error: {init_resp['error']}\n"
            )
            return 1
        result = init_resp.get("result", {})
        if result.get("protocolVersion") != "2024-11-05":
            sys.stderr.write(
                f"FAIL: unexpected protocolVersion: {result.get('protocolVersion')!r}\n"
            )
            return 1
        if result.get("serverInfo", {}).get("name") != "ironmem":
            sys.stderr.write(
                f"FAIL: unexpected server name: {result.get('serverInfo')!r}\n"
            )
            return 1
        if "tools" not in result.get("capabilities", {}):
            sys.stderr.write(
                f"FAIL: missing tools capabilities: {result.get('capabilities')!r}\n"
            )
            return 1

        print("PASS: MCP initialize")

        # Extract collab session_id from collab_start response
        if collab_start_resp.get("error") is not None:
            sys.stderr.write(
                f"FAIL: collab_start returned an error: {collab_start_resp['error']}\n"
            )
            return 1
        cs_result = collab_start_resp.get("result", {})
        cs_content = cs_result.get("content", [{}])
        cs_text = cs_content[0].get("text", "{}") if cs_content else "{}"
        cs_data = json.loads(cs_text)
        collab_sid = cs_data.get("session_id")
        if not collab_sid:
            sys.stderr.write(
                f"FAIL: collab_start did not return a session_id: {cs_text!r}\n"
            )
            return 1

        # ------------------------------------------------------------------ #
        # Phase 2: collab_status (needs the returned collab session id)       #
        # ------------------------------------------------------------------ #
        phase2_requests = [
            {
                "jsonrpc": "2.0",
                "id": 3,
                "method": "initialize",
                "params": {"sessionId": session_id},
            },
            {
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "collab_status",
                    "arguments": {"session_id": collab_sid},
                },
            },
        ]
        phase2_responses = run_mcp_requests(
            command, phase2_requests, base_env, args.timeout
        )

        if len(phase2_responses) < 2:
            sys.stderr.write(
                f"FAIL: expected 2 responses in phase 2, got {len(phase2_responses)}\n"
            )
            return 1

        collab_status_resp = phase2_responses[1]
        if collab_status_resp.get("error") is not None:
            sys.stderr.write(
                f"FAIL: collab_status returned an error: {collab_status_resp['error']}\n"
            )
            return 1
        # Check it's not an isError result (tool-level error)
        st_result = collab_status_resp.get("result", {})
        if st_result.get("isError"):
            st_content = st_result.get("content", [{}])
            st_text = st_content[0].get("text", "") if st_content else ""
            sys.stderr.write(f"FAIL: collab_status tool error: {st_text!r}\n")
            return 1

        print("PASS: collab_start + collab_status")

        # ------------------------------------------------------------------ #
        # Phase 3: hook invocation for occupancy_samples                      #
        # ------------------------------------------------------------------ #
        # Write a minimal transcript JSONL with an assistant usage object so
        # the hook's extract_last_assistant_usage can find real token counts.
        transcript_path = Path(temp_dir) / "smoke_transcript.jsonl"
        transcript_line = json.dumps({
            "type": "assistant",
            "message": {
                "usage": {
                    "input_tokens": 5000,
                    "output_tokens": 200,
                    "cache_read_input_tokens": 1000,
                    "cache_creation_input_tokens": 0,
                }
            },
        })
        transcript_path.write_text(transcript_line + "\n")

        hook_env = {
            **base_env,
            "IRONMEM_CONTEXT_WINDOW": "200000",
        }
        hook_input = json.dumps({
            "session_id": session_id,
            "cwd": "/tmp/smoke",
            "transcript_path": str(transcript_path),
        })
        hook_command = build_hook_command(args.binary)
        try:
            hook_completed = subprocess.run(
                hook_command,
                input=hook_input,
                text=True,
                capture_output=True,
                env=hook_env,
                timeout=args.timeout,
                check=False,
            )
        except subprocess.TimeoutExpired:
            sys.stderr.write(f"FAIL: hook process timed out after {args.timeout}s\n")
            return 1

        if hook_completed.returncode != 0:
            sys.stderr.write("FAIL: hook process returned non-zero\n")
            sys.stderr.write(hook_completed.stderr)
            return 1

        # ------------------------------------------------------------------ #
        # Phase 4: SQLite assertions over all four metrics tables             #
        # ------------------------------------------------------------------ #
        conn = sqlite3.connect(db_path)
        conn.row_factory = sqlite3.Row

        # token_usage: ≥1 row with source='mcp_response' AND collab_session_id set
        rows = conn.execute(
            "SELECT COUNT(*) AS n FROM token_usage "
            "WHERE source = 'mcp_response' AND collab_session_id IS NOT NULL"
        ).fetchone()
        if rows["n"] < 1:
            sys.stderr.write(
                "FAIL: token_usage — no row with source='mcp_response' and collab_session_id set\n"
            )
            return 1
        print(f"PASS: token_usage ({rows['n']} mcp_response row(s) with collab_session_id)")

        # task_outcomes: session row with started_at set
        rows = conn.execute(
            "SELECT COUNT(*) AS n FROM task_outcomes "
            "WHERE collab_session_id = ? AND started_at IS NOT NULL",
            (collab_sid,),
        ).fetchone()
        if rows["n"] < 1:
            sys.stderr.write(
                f"FAIL: task_outcomes — no row for collab session {collab_sid!r} with started_at\n"
            )
            return 1
        print(f"PASS: task_outcomes ({rows['n']} row(s) with started_at for collab session)")

        # session_summary: ≥1 row
        rows = conn.execute("SELECT COUNT(*) AS n FROM session_summary").fetchone()
        if rows["n"] < 1:
            sys.stderr.write("FAIL: session_summary — no rows\n")
            return 1
        print(f"PASS: session_summary ({rows['n']} row(s))")

        # occupancy_samples: ≥1 row from the hook invocation
        rows = conn.execute(
            "SELECT COUNT(*) AS n FROM occupancy_samples WHERE session_id = ?",
            (session_id,),
        ).fetchone()
        if rows["n"] < 1:
            sys.stderr.write(
                "FAIL: occupancy_samples — no rows for hook session\n"
            )
            return 1
        print(f"PASS: occupancy_samples ({rows['n']} row(s) from hook)")

        conn.close()

    print("\nAll four metrics tables populated. Smoke test PASSED.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

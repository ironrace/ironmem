#!/usr/bin/env python3
"""Fill the #95/#96 Phase-6 baseline gate.

Drives a flags-ON `ironmem serve` over line-delimited JSON-RPC (stdio) to
generate >=10 distinct measured `llm_rerank` task_keys (#95) and >=10 distinct
measured `pref_extract` task_keys (#96) in the production `token_usage` table,
so `ironmem report` sees `baseline_ready` for both LLM-call-reduction PRs.

Mechanism (see feedback_baseline_runs_need_flags_on memory):
  - The gate counts distinct task_keys (collab_session_id else task_tag) that
    have >=1 measured (estimated=0) token row for the operation.
  - task_tag is set per-call via the `status` tool's `set_task_tag` arg, held
    in server app state, so one long-lived server mints many distinct keys.
  - rerank/pref are OFF by default; enabled here via env on the server process.
  - Both LLM paths use the CLI backend (`claude -p` Haiku) = subscription
    tokens, $0 metered.

Side effects on the production DB:
  - 20 token_usage rows (the desired baseline).
  - ~10 content drawers under wing `abeval-baseline` (the pref input drawer is
    stored by add_drawer). Delete after with:
      DELETE FROM ... / or `delete_drawer` on wing=abeval-baseline.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path

IRONMEM = os.environ.get("IRONMEM_BIN", str(Path.home() / ".ironrace/bin/ironmem"))
DB_PATH = os.environ.get(
    "IRONMEM_DB_PATH", str(Path.home() / ".ironrace-memory/memory.sqlite3")
)
N = int(os.environ.get("BASELINE_N", "10"))
BASELINE_WING = "abeval-baseline"

# Varied real-ish recall queries so each search hits a different candidate set;
# any query over the ~48k-drawer store returns >=2 candidates -> rerank fires.
RERANK_QUERIES = [
    "how does the collab phase state machine dispatch turns",
    "rerank confidence margin gate token usage",
    "abeval campaign tokens to done thesis confound",
    "provbench labeler rule R4 line presence over-fit",
    "symbol import graph incremental index sqlite",
    "metrics occupancy sample hook access mode",
    "harness registry HarnessId newtype migration",
    "pref extract synthetic preference sibling drawer",
    "code map lazy per area git diff freshness",
    "session handoff generation lease inertness",
    "knowledge graph score boost entity recall",
    "drawer staging get_drawer by id recovery",
]

# Conversational content -> looks_conversational() true (needle " i "/"i'"/"my").
# Each LLM pref-extract call records a token row even if 0 phrases are mined.
PREF_DOCS = [
    "I prefer concise commit messages and I'm careful to run tests before I push my changes.",
    "I like small focused PRs. My rule is one logical change per branch, and I review my own diff first.",
    "I'm a fan of immutable patterns. I avoid mutating shared state in my code whenever I can.",
    "I always validate input at my system boundaries. I never trust data from external APIs in my handlers.",
    "I'd rather fix the implementation than weaken a test. My testing discipline is non-negotiable for me.",
    "I default to Rust for my hot paths, but I reach for Python when I'm scripting one-off analysis for myself.",
    "I keep my files small, around 200 to 400 lines. My preference is many small modules over a few big ones.",
    "I'm cautious with secrets. I keep my API keys in env vars and I never hardcode them in my source.",
    "I like to research first. Before I write new code I search for a battle-tested library that fits my need.",
    "I prefer explicit error handling. I never silently swallow an error in my code; I surface it to me clearly.",
    "I'm careful about performance. I profile before I optimize so my effort lands where it matters.",
    "I value readable names. My functions stay under fifty lines so I can hold each one in my head.",
]


class Server:
    """Line-delimited JSON-RPC client over an `ironmem serve` subprocess."""

    def __init__(self) -> None:
        # "api" -> direct Anthropic Messages API (metered, leaner/faster, needs
        # IRONMEM_ANTHROPIC_API_KEY); "cli" -> `claude -p` subprocess (subscription
        # quota, but ~157k tokens/call of MCP-schema overhead in this environment).
        backend = os.environ.get("BASELINE_BACKEND", "cli")
        env = dict(os.environ)
        env.update(
            {
                "IRONMEM_DB_PATH": DB_PATH,
                "IRONMEM_MCP_MODE": "trusted",  # add_drawer needs writes (Trusted only)
                "IRONMEM_RERANK": "llm_haiku",  # #95 path on
                "IRONMEM_PREF_ENRICH": "1",  # #96 path on
                "IRONMEM_PREF_EXTRACTOR": "llm",  # use LLM extractor, not regex
                "IRONMEM_LLM_RERANK_BACKEND": backend,
                "IRONMEM_PREF_LLM_BACKEND": backend,
                "IRONMEM_METRICS": "1",
            },
        )
        self.stderr = open("/tmp/baseline_driver.serve.log", "w")
        self.p = subprocess.Popen(
            [IRONMEM, "serve"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr,
            env=env,
            text=True,
            bufsize=1,
        )
        self._id = 0

    def _send(self, obj: dict) -> None:
        assert self.p.stdin is not None
        self.p.stdin.write(json.dumps(obj) + "\n")
        self.p.stdin.flush()

    def _read_result(self, want_id: int) -> dict:
        assert self.p.stdout is not None
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("server closed stdout (see /tmp/baseline_driver.serve.log)")
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue  # skip any stray non-JSON line
            if msg.get("id") == want_id:
                if "error" in msg:
                    raise RuntimeError(f"rpc error: {msg['error']}")
                return msg.get("result", {})

    def call(self, method: str, params: dict | None = None) -> dict:
        self._id += 1
        rid = self._id
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}})
        return self._read_result(rid)

    def notify(self, method: str, params: dict | None = None) -> None:
        self._send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def tool(self, name: str, arguments: dict) -> dict:
        res = self.call("tools/call", {"name": name, "arguments": arguments})
        if res.get("isError"):
            raise RuntimeError(f"tool {name} error: {res.get('content')}")
        # Unwrap the MCP text-content envelope into the tool's JSON payload.
        for block in res.get("content", []):
            if block.get("type") == "text":
                try:
                    return json.loads(block["text"])
                except (json.JSONDecodeError, KeyError):
                    return {"text": block.get("text", "")}
        return res

    def wait_ready(self, timeout_s: int = 300) -> None:
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            st = self.tool("status", {})
            if not st.get("warming_up", False):
                print(f"  warm: {st.get('total_drawers')} drawers ready")
                return
            time.sleep(2)
        raise RuntimeError("server never finished warming up")

    def close(self) -> None:
        try:
            if self.p.stdin:
                self.p.stdin.close()
            self.p.wait(timeout=10)
        except Exception:
            self.p.kill()
        finally:
            self.stderr.close()


def main() -> int:
    if not Path(IRONMEM).exists():
        print(f"ironmem binary not found at {IRONMEM}", file=sys.stderr)
        return 2
    print(f"binary : {IRONMEM}")
    print(f"db     : {DB_PATH}")
    print(f"tasks  : {N} rerank + {N} pref\n")

    srv = Server()
    try:
        srv.call(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "baseline-driver", "version": "1"},
            },
        )
        srv.notify("notifications/initialized")
        print("waiting for memory warmup...")
        srv.wait_ready()

        t0 = time.time()
        print("== #95 llm_rerank ==")
        for i in range(N):
            tag = f"baseline-rerank-{i + 1:02d}"
            srv.tool("status", {"set_task_tag": tag})
            q = RERANK_QUERIES[i % len(RERANK_QUERIES)]
            srv.tool("search", {"query": q})
            print(f"  [{i + 1:02d}/{N}] {tag}  q={q[:48]!r}  ({time.time() - t0:5.1f}s)")

        print("\n== #96 pref_extract ==")
        for i in range(N):
            tag = f"baseline-pref-{i + 1:02d}"
            srv.tool("status", {"set_task_tag": tag})
            doc = PREF_DOCS[i % len(PREF_DOCS)]
            srv.tool(
                "add_drawer",
                {"content": doc, "wing": BASELINE_WING, "room": f"pref-{i + 1:02d}"},
            )
            print(f"  [{i + 1:02d}/{N}] {tag}  ({time.time() - t0:5.1f}s)")

        print(f"\ndone in {time.time() - t0:.1f}s")
    finally:
        srv.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

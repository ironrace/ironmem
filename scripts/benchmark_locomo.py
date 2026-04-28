#!/usr/bin/env python3
"""LoCoMo long-term conversation memory benchmark for ironmem.

Tests session-level retrieval recall, matching mempalace's LoCoMo benchmark.

Mempalace published baselines (session-level, top-10, no rerank):
  session:   60.3% R@10  (1,986 questions)
  hybrid v5: 88.9% R@10  (same set)

Dataset: locomo10.json — 10 multi-session conversations, ~1,986 QA pairs with
evidence_dialog_ids mapping back to sessions.

The dataset is hosted on GitHub (snap-research/locomo) with git-lfs.
Download it once:
    git clone https://github.com/snap-research/locomo ~/.cache/ironrace/locomo-repo
    # OR just the data file:
    cd ~/.cache/ironrace && git init locomo-repo && cd locomo-repo && \\
        git lfs install && \\
        git remote add origin https://github.com/snap-research/locomo && \\
        git lfs pull --include="data/locomo10.json"

Then run:
    python3 scripts/benchmark_locomo.py ~/.cache/ironrace/locomo-repo/data/locomo10.json

Usage:
    python3 scripts/benchmark_locomo.py /path/to/locomo10.json
    python3 scripts/benchmark_locomo.py /path/to/locomo10.json --limit 200
    python3 scripts/benchmark_locomo.py /path/to/locomo10.json --ef-search 400
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from collections import defaultdict
from pathlib import Path

_CACHE_DEFAULT = Path.home() / ".cache" / "ironrace" / "locomo-repo" / "data" / "locomo10.json"

_DOWNLOAD_HINT = """\
  git clone https://github.com/snap-research/locomo ~/.cache/ironrace/locomo-repo

Then rerun:
  python3 scripts/benchmark_locomo.py ~/.cache/ironrace/locomo-repo/data/locomo10.json
"""


def _ensure_dataset(explicit_path: str | None) -> Path:
    # Prefer the explicit path
    if explicit_path:
        p = Path(explicit_path).expanduser().resolve()
        if not p.exists():
            print(f"error: file not found: {p}", file=sys.stderr)
            sys.exit(1)
        return p

    # Check the default cache location from a prior clone
    if _CACHE_DEFAULT.exists():
        return _CACHE_DEFAULT

    # Try to download the raw file directly from GitHub
    import urllib.request

    url = "https://raw.githubusercontent.com/snap-research/locomo/main/data/locomo10.json"
    dest = _CACHE_DEFAULT
    dest.parent.mkdir(parents=True, exist_ok=True)

    print(f"Trying to download locomo10.json from GitHub...", flush=True)
    try:
        with urllib.request.urlopen(url, timeout=30) as resp:
            data = resp.read()
        # Detect git-lfs pointer — starts with "version https://git-lfs"
        if data[:7] == b"version" and b"git-lfs" in data[:120]:
            raise RuntimeError("file is git-lfs pointer, not the actual JSON")
        dest.write_bytes(data)
        print(f"  Cached to {dest}", flush=True)
        return dest
    except Exception as exc:
        print(
            f"  Could not auto-download ({exc}).\n\n"
            "Clone the repo manually:\n" + _DOWNLOAD_HINT,
            file=sys.stderr,
        )
        sys.exit(1)


# ── Dataset helpers ───────────────────────────────────────────────────────────
#
# Actual locomo10.json schema (verified against the file):
#
#   entry = {
#     "sample_id": "conv-26",
#     "conversation": {
#       "speaker_a": "...", "speaker_b": "...",
#       "session_1_date_time": "...",
#       "session_1": [
#         {"speaker": "...", "dia_id": "D1:1", "text": "..."},
#         ...
#       ],
#       "session_2": [...],
#       ...
#     },
#     "qa": [
#       {"question": "...", "answer": "...", "evidence": ["D1:3"], "category": 2},
#       ...
#     ],
#     ...
#   }
#
# dia_id strings use the format "D<session>:<turn>", e.g. "D1:3" → session_1 turn 3.
# The "evidence" field (list of dia_id strings) identifies which sessions contain
# the answer — that's the ground truth for retrieval scoring.

def _extract_sessions(entry: dict) -> list[tuple[str, str]]:
    """Return [(session_key, session_text)] for all sessions in one entry."""
    sessions: list[tuple[str, str]] = []
    conv = entry.get("conversation", {})
    n = 1
    while True:
        key = f"session_{n}"
        if key not in conv:
            break
        turns = conv[key]
        if isinstance(turns, list):
            lines = [
                f"{t.get('speaker', 'User')}: {t.get('text', '')}"
                for t in turns
                if isinstance(t, dict) and t.get("text")
            ]
            if lines:
                sessions.append((key, "\n".join(lines)))
        n += 1
    return sessions


def _build_dia_to_session(entry: dict) -> dict[str, str]:
    """Map dia_id string (e.g. 'D1:3') -> session_key (e.g. 'session_1')."""
    mapping: dict[str, str] = {}
    conv = entry.get("conversation", {})
    n = 1
    while True:
        key = f"session_{n}"
        if key not in conv:
            break
        turns = conv[key]
        if isinstance(turns, list):
            for turn in turns:
                if isinstance(turn, dict) and "dia_id" in turn:
                    mapping[str(turn["dia_id"])] = key
        n += 1
    return mapping


def _evidence_sessions(qa: dict, dia_to_session: dict[str, str]) -> set[str]:
    """Resolve QA evidence references to session keys."""
    evidence = qa.get("evidence", qa.get("evidence_dialog_ids", []))
    result: set[str] = set()
    for ref in evidence:
        key = dia_to_session.get(str(ref))
        if key:
            result.add(key)
    return result


def _entry_id(entry: dict, idx: int) -> str:
    return str(entry.get("sample_id", entry.get("conversation_id", idx)))


def recall_at_k(ranked: list[str], evidence: set[str], k: int) -> float:
    return 1.0 if any(s in evidence for s in ranked[:k]) else 0.0


# ── MCP JSON-RPC client ───────────────────────────────────────────────────────

class McpClient:
    def __init__(self, name: str, cmd: list[str], env: dict[str, str]) -> None:
        self.name = name
        self.cmd = cmd
        self.env = env
        self._proc: subprocess.Popen | None = None
        self._req_id = 0

    def start(self, wait_for_embedder: bool = False) -> None:
        self._proc = subprocess.Popen(
            self.cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env={**os.environ, **self.env},
            text=True,
        )
        self._call("initialize", {})
        if wait_for_embedder:
            deadline = time.monotonic() + 120.0
            while time.monotonic() < deadline:
                try:
                    r = self.call_tool("status", {})
                    if not r.get("warming_up", False):
                        break
                except Exception:
                    pass
                time.sleep(0.25)

    def stop(self) -> None:
        if self._proc:
            try:
                self._proc.stdin.close()  # type: ignore[union-attr]
                self._proc.wait(timeout=5)
            except Exception:
                self._proc.kill()
            self._proc = None

    def _call(self, method: str, params: dict) -> dict:
        self._req_id += 1
        req = json.dumps({"jsonrpc": "2.0", "id": self._req_id, "method": method, "params": params})
        assert self._proc and self._proc.stdin and self._proc.stdout
        self._proc.stdin.write(req + "\n")
        self._proc.stdin.flush()
        line = self._proc.stdout.readline()
        if not line:
            raise RuntimeError(f"{self.name}: server closed stdout")
        return json.loads(line)

    def call_tool(self, name: str, arguments: dict) -> dict:
        resp = self._call("tools/call", {"name": name, "arguments": arguments})
        if "error" in resp:
            raise RuntimeError(f"{self.name} tool error: {resp['error']}")
        content = resp.get("result", {}).get("content", [])
        if content and content[0].get("type") == "text":
            try:
                return json.loads(content[0]["text"])
            except json.JSONDecodeError:
                return {"raw": content[0]["text"]}
        return {}


# ── Benchmark runner ──────────────────────────────────────────────────────────

def run_locomo_benchmark(
    conversations: list[dict],
    binary: str,
    limit: int,
    n_results: int,
    ef_search: int | None,
) -> dict:
    """Run LoCoMo retrieval benchmark against ironmem.

    One wing per conversation; all sessions ingested before queries run.
    Each QA pair's evidence_dialog_ids are resolved to session keys and used
    as the ground-truth answer for R@k scoring.
    """
    ks = [1, 3, 5, 10]
    recalls: dict[int, list[float]] = {k: [] for k in ks}
    per_category: dict[str, dict[int, list[float]]] = defaultdict(lambda: {k: [] for k in ks})
    search_latencies: list[float] = []

    tmp = Path(tempfile.mkdtemp(prefix="ironmem-locomo-"))
    db_path = tmp / "memory.sqlite3"

    env: dict[str, str] = {
        "IRONMEM_DB_PATH": str(db_path),
        "IRONMEM_EMBED_MODE": "real",
        "IRONMEM_MCP_MODE": "trusted",
        "IRONMEM_AUTO_BOOTSTRAP": "0",
    }
    if ef_search is not None:
        env["IRONMEM_EF_SEARCH"] = str(ef_search)

    client = McpClient(
        name="ironmem",
        cmd=[binary, "serve"],
        env=env,
    )

    questions_done = 0

    try:
        client.start(wait_for_embedder=True)
        print("  Model loaded.", flush=True)

        for conv_idx, entry in enumerate(conversations):
            if questions_done >= limit:
                break

            conv_id = _entry_id(entry, conv_idx)
            sessions = _extract_sessions(entry)
            dia_to_session = _build_dia_to_session(entry)
            qa_pairs = entry.get("qa", [])

            if not sessions or not qa_pairs:
                print(f"  Conv {conv_id}: no sessions or QA pairs, skipping.", flush=True)
                continue

            wing = f"conv{conv_idx}"  # numeric index avoids non-ASCII wing names

            # Ingest all sessions for this conversation
            content_to_session: dict[str, str] = {}
            for sess_key, sess_text in sessions:
                content_to_session[sess_text[:120]] = sess_key
                client.call_tool("add_drawer", {
                    "content": sess_text,
                    "wing": wing,
                    "room": "session",
                })

            print(
                f"  Conv {conv_id}: {len(sessions)} sessions, "
                f"{len(qa_pairs)} QA pairs...",
                flush=True,
            )

            for qa in qa_pairs:
                if questions_done >= limit:
                    break

                question = qa.get("question", "")
                category = str(qa.get("category", "unknown"))

                if not question:
                    continue

                ev_sessions = _evidence_sessions(qa, dia_to_session)
                if not ev_sessions:
                    continue

                t0 = time.perf_counter()
                payload = client.call_tool("search", {
                    "query": question,
                    "limit": n_results,
                    "wing": wing,
                })
                elapsed_ms = (time.perf_counter() - t0) * 1000
                search_latencies.append(elapsed_ms)

                results = payload.get("results", [])
                ranked: list[str] = []
                seen: set[str] = set()
                for r in results:
                    prefix = r.get("content", "")[:120]
                    sess_key = content_to_session.get(prefix)
                    if sess_key and sess_key not in seen:
                        ranked.append(sess_key)
                        seen.add(sess_key)
                for sess_key, _ in sessions:
                    if sess_key not in seen:
                        ranked.append(sess_key)

                for k in ks:
                    score = recall_at_k(ranked, ev_sessions, k)
                    recalls[k].append(score)
                    per_category[category][k].append(score)

                questions_done += 1

            r10 = sum(recalls[10]) / max(len(recalls[10]), 1)
            med = sorted(search_latencies)[len(search_latencies) // 2] if search_latencies else 0
            print(
                f"    done  total_q={questions_done}  R@10={r10:.1%}  med={med:.1f}ms",
                flush=True,
            )

    finally:
        client.stop()
        shutil.rmtree(tmp, ignore_errors=True)

    sl = sorted(search_latencies)
    return {
        "backend": "ironmem",
        "questions": len(recalls[10]),
        "recall": {k: sum(v) / max(len(v), 1) for k, v in recalls.items()},
        "per_category": {
            cat: {k: sum(v) / max(len(v), 1) for k, v in kd.items()}
            for cat, kd in per_category.items()
        },
        "latency_p50_ms": sl[len(sl) // 2] if sl else 0,
        "latency_p95_ms": sl[int(len(sl) * 0.95)] if sl else 0,
    }


# ── Output ────────────────────────────────────────────────────────────────────

def print_results(results: list[dict]) -> None:
    ks = [1, 3, 5, 10]
    print()
    print("LoCoMo Benchmark Results")
    print("=" * 70)
    print(f"{'Backend':<22}  {'R@1':>6}  {'R@3':>6}  {'R@5':>6}  {'R@10':>6}  {'p50':>8}  {'p95':>8}")
    print("-" * 70)
    for r in results:
        if not r:
            continue
        rec = r["recall"]
        print(
            f"{r['backend']:<22}  "
            f"{rec[1]:>6.1%}  "
            f"{rec[3]:>6.1%}  "
            f"{rec[5]:>6.1%}  "
            f"{rec[10]:>6.1%}  "
            f"{r['latency_p50_ms']:>7.1f}ms  "
            f"{r['latency_p95_ms']:>7.1f}ms"
        )
    print()

    if any(r.get("per_category") for r in results):
        all_cats = sorted({cat for r in results if r for cat in r.get("per_category", {})})
        print("R@10 by question category:")
        for cat in all_cats:
            print(f"  {cat:<45}", end="")
            for r in results:
                if not r:
                    continue
                v = r["per_category"].get(cat, {}).get(10)
                print(f"  {v:.1%}" if v is not None else "  —", end="")
            print()
        print()

    print("mempalace baseline (session, no rerank):   60.3% R@10")
    print("mempalace baseline (hybrid v5, no rerank): 88.9% R@10")
    print()


# ── CLI ───────────────────────────────────────────────────────────────────────

def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="LoCoMo benchmark: ironmem session-level retrieval.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument(
        "data",
        nargs="?",
        default=None,
        help="Path to locomo10.json (auto-downloaded to ~/.cache/ironrace/ if omitted)",
    )
    p.add_argument("--limit", type=int, default=2000, help="Max questions (default: 2000)")
    p.add_argument("--n-results", type=int, default=10, help="Results per query (default: 10)")
    p.add_argument(
        "--ironmem-binary",
        default="./target/release/ironmem",
        help="Path to ironmem binary (default: ./target/release/ironmem)",
    )
    p.add_argument("--ef-search", type=int, default=None, help="Override HNSW ef_search")
    p.add_argument("--output-json", default=None, help="Write results to JSON file")
    return p.parse_args()


def main() -> int:
    args = parse_args()
    data_path = _ensure_dataset(args.data)

    print(f"Loading {data_path.name}...", flush=True)
    with open(data_path) as f:
        data = json.load(f)

    total_qa = sum(len(c.get("qa", [])) for c in data)
    print(f"  {len(data)} conversations, {total_qa} QA pairs total.", flush=True)

    binary = Path(args.ironmem_binary).expanduser().resolve()
    if not binary.exists():
        print(f"ironmem binary not found: {binary}", file=sys.stderr)
        print("Build it with: cargo build --release -p ironmem --bin ironmem", file=sys.stderr)
        return 1

    ef_label = f"  ef_search={args.ef_search}" if args.ef_search else ""
    print(f"\nironmem{ef_label}:", flush=True)
    result = run_locomo_benchmark(
        conversations=data,
        binary=str(binary),
        limit=args.limit,
        n_results=args.n_results,
        ef_search=args.ef_search,
    )
    print_results([result])

    if args.output_json:
        Path(args.output_json).write_text(json.dumps([result], indent=2))
        print(f"Results written to {args.output_json}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())

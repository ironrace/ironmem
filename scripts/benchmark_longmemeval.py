#!/usr/bin/env python3
"""LongMemEval benchmark — ironrace-memory vs mempalace, apples-to-apples.

Both systems ingest the same haystack sessions from LongMemEval and answer
the same 500 questions. Scoring is identical to mempalace's own benchmark
script: Recall@k (any answer session in top-k results).

The key difference in this benchmark vs mempalace's raw mode:
  - mempalace: one ChromaDB doc per session, searched via ephemeral in-process client
  - ironrace:  one ironmem_add_drawer per session, searched via MCP server

Both use all-MiniLM-L6-v2 embeddings, so this isolates retrieval infrastructure
quality, not model quality.

The dataset (longmemeval_s_cleaned.json) is downloaded automatically on first
run from HuggingFace (xiaowu0162/longmemeval) and cached at
~/.cache/ironrace/longmemeval_s_cleaned.json.

Usage:
    python3 scripts/benchmark_longmemeval.py               # auto-download dataset
    python3 scripts/benchmark_longmemeval.py --limit 50    # quick 50-question run
    python3 scripts/benchmark_longmemeval.py --backend both
    python3 scripts/benchmark_longmemeval.py /path/to/longmemeval_s_cleaned.json
"""

from __future__ import annotations

import argparse
import json
import math
import os
import shutil
import subprocess
import sys
import tempfile
import time
from collections import defaultdict
from pathlib import Path

_HF_REPO = "xiaowu0162/longmemeval"
_HF_FILENAME = "longmemeval_s"
_CACHE_PATH = Path.home() / ".cache" / "ironrace" / _HF_FILENAME


def _ensure_dataset(explicit_path: str | None) -> Path:
    """Return path to the dataset, downloading from HuggingFace if needed."""
    if explicit_path:
        p = Path(explicit_path)
        if not p.exists():
            print(f"error: data file not found: {p}", file=sys.stderr)
            sys.exit(1)
        return p

    if _CACHE_PATH.exists():
        return _CACHE_PATH

    print(f"Dataset not found locally. Downloading from HuggingFace ({_HF_REPO})...", flush=True)
    try:
        from huggingface_hub import hf_hub_download
    except ImportError:
        print("error: huggingface_hub not installed. Run: pip install huggingface_hub", file=sys.stderr)
        sys.exit(1)

    _CACHE_PATH.parent.mkdir(parents=True, exist_ok=True)
    downloaded = hf_hub_download(
        repo_id=_HF_REPO,
        filename=_HF_FILENAME,
        repo_type="dataset",
        local_dir=str(_CACHE_PATH.parent),
    )
    dest = Path(downloaded)
    if dest != _CACHE_PATH:
        dest.rename(_CACHE_PATH)
    print(f"  Cached to {_CACHE_PATH}", flush=True)
    return _CACHE_PATH


# ── Metrics (same as mempalace's implementation) ─────────────────────────────

def recall_any_at_k(rankings: list[int], answer_sids: set[str], corpus_ids: list[str], k: int) -> float:
    top_k = {corpus_ids[i] for i in rankings[:k]}
    return 1.0 if any(sid in top_k for sid in answer_sids) else 0.0


# ── MCP JSON-RPC client (same as benchmark_recall.py) ────────────────────────

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
                    r = self.call_tool("ironmem_status", {})
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


# ── Data helpers ──────────────────────────────────────────────────────────────

def build_corpus(entry: dict, granularity: str = "session") -> tuple[list[str], list[str]]:
    """Return (documents, corpus_ids) for one LongMemEval entry."""
    docs: list[str] = []
    ids: list[str] = []
    sessions = entry["haystack_sessions"]
    session_ids = entry["haystack_session_ids"]

    for session, sess_id in zip(sessions, session_ids):
        if granularity == "session":
            user_turns = [t["content"] for t in session if t["role"] == "user"]
            if user_turns:
                docs.append("\n".join(user_turns))
                ids.append(sess_id)
        else:
            for turn_num, turn in enumerate(t for t in session if t["role"] == "user"):
                docs.append(turn["content"])
                ids.append(f"{sess_id}_turn_{turn_num}")

    return docs, ids


# ── ironrace-memory retriever ─────────────────────────────────────────────────

def run_ironrace_benchmark(
    data: list[dict],
    ironmem_binary: str,
    limit: int,
    n_results: int,
    granularity: str,
    ef_search: int | None,
) -> dict:
    """Run LongMemEval against ironrace-memory, one fresh server per question.

    Each question gets its own DB and MCP server, matching mempalace's
    per-collection methodology. This ensures the HNSW index only contains
    the ~50 sessions for the current question, so overfetch is not diluted
    by cross-question documents.

    Trade-off: ~N server startups (each loads the ONNX model from disk).
    With the model already cached in the OS page cache after the first
    question, subsequent loads are fast (~0.2s on M-series hardware).
    """
    ks = [1, 3, 5, 10]
    recalls: dict[int, list[float]] = {k: [] for k in ks}
    per_type: dict[str, dict[int, list[float]]] = defaultdict(lambda: {k: [] for k in ks})
    search_latencies: list[float] = []

    data = data[:limit]
    total = len(data)

    base_env: dict[str, str] = {
        "IRONMEM_EMBED_MODE": "real",
        "IRONMEM_MCP_MODE": "trusted",
        "IRONMEM_AUTO_BOOTSTRAP": "0",
    }
    if ef_search is not None:
        base_env["IRONMEM_EF_SEARCH"] = str(ef_search)

    for i, entry in enumerate(data):
        qtype = entry["question_type"]
        question = entry["question"]
        answer_sids = set(entry["answer_session_ids"])
        docs, corpus_ids = build_corpus(entry, granularity)

        if not docs:
            continue

        tmp = Path(tempfile.mkdtemp(prefix="ironmem-lme-q-"))
        db_path = tmp / "memory.sqlite3"

        env = {**base_env, "IRONMEM_DB_PATH": str(db_path)}

        client = McpClient(
            name="ironrace-memory",
            cmd=[ironmem_binary, "serve"],
            env=env,
        )

        try:
            client.start(wait_for_embedder=True)

            # Capture drawer IDs during ingest for exact ID-based matching.
            # Fingerprinting doc[:120] was fragile: sanitize_content() trims
            # stored content, so stored[:120] ≠ original[:120] on any doc with
            # leading whitespace, silently demoting that result to rank 50+.
            drawer_id_to_idx: dict[str, int] = {}
            for j, doc in enumerate(docs):
                resp = client.call_tool("ironmem_add_drawer", {
                    "content": doc,
                    "wing": "session",
                    "room": "haystack",
                })
                drawer_id = resp.get("id")
                if drawer_id:
                    drawer_id_to_idx[drawer_id] = j

            t0 = time.perf_counter()
            payload = client.call_tool("ironmem_search", {
                "query": question,
                "limit": n_results,
            })
            elapsed_ms = (time.perf_counter() - t0) * 1000
            search_latencies.append(elapsed_ms)

            results = payload.get("results", [])
            ranked: list[int] = []
            seen: set[int] = set()
            for r in results:
                drawer_id = r.get("id", "")
                idx = drawer_id_to_idx.get(drawer_id)
                if idx is not None and idx not in seen:
                    ranked.append(idx)
                    seen.add(idx)
            for j in range(len(docs)):
                if j not in seen:
                    ranked.append(j)

            for k in ks:
                score = recall_any_at_k(ranked, answer_sids, corpus_ids, k)
                recalls[k].append(score)
                per_type[qtype][k].append(score)

        finally:
            client.stop()
            shutil.rmtree(tmp, ignore_errors=True)

        if (i + 1) % 50 == 0 or i == total - 1:
            r5 = sum(recalls[5]) / max(len(recalls[5]), 1)
            med = sorted(search_latencies)[len(search_latencies) // 2]
            print(f"  [{i+1:>3}/{total}]  R@5={r5:.1%}  med_search={med:.1f}ms", flush=True)

    sl = sorted(search_latencies)
    return {
        "backend": "ironrace-memory",
        "questions": len(recalls[5]),
        "recall": {k: sum(v) / max(len(v), 1) for k, v in recalls.items()},
        "per_type": {
            qt: {k: sum(v) / max(len(v), 1) for k, v in kd.items()}
            for qt, kd in per_type.items()
        },
        "latency_p50_ms": sl[len(sl) // 2] if sl else 0,
        "latency_p95_ms": sl[int(len(sl) * 0.95)] if sl else 0,
    }


# ── mempalace retriever ───────────────────────────────────────────────────────

def run_mempalace_benchmark(
    data: list[dict],
    limit: int,
    n_results: int,
    granularity: str,
) -> dict:
    """Run LongMemEval against mempalace using the same logic as their own script."""
    try:
        import chromadb
    except ImportError:
        print("chromadb not available in this Python — skipping mempalace", file=sys.stderr)
        return {}

    ks = [1, 3, 5, 10]
    recalls: dict[int, list[float]] = {k: [] for k in ks}
    per_type: dict[str, dict[int, list[float]]] = defaultdict(lambda: {k: [] for k in ks})
    latencies: list[float] = []

    data = data[:limit]
    total = len(data)

    client = chromadb.EphemeralClient()

    def fresh_collection():
        try:
            client.delete_collection("mempalace_drawers")
        except Exception:
            pass
        return client.create_collection("mempalace_drawers")

    for i, entry in enumerate(data):
        qtype = entry["question_type"]
        question = entry["question"]
        answer_sids = set(entry["answer_session_ids"])

        docs, corpus_ids = build_corpus(entry, granularity)
        if not docs:
            continue

        col = fresh_collection()
        col.add(
            documents=docs,
            ids=[f"doc_{j}" for j in range(len(docs))],
            metadatas=[{"corpus_id": cid} for cid in corpus_ids],
        )

        t0 = time.perf_counter()
        results = col.query(
            query_texts=[question],
            n_results=min(n_results, len(docs)),
            include=["metadatas"],
        )
        elapsed_ms = (time.perf_counter() - t0) * 1000
        latencies.append(elapsed_ms)

        result_doc_ids = results["ids"][0]
        doc_id_to_idx = {f"doc_{j}": j for j in range(len(docs))}
        ranked = [doc_id_to_idx[rid] for rid in result_doc_ids]
        seen = set(ranked)
        for j in range(len(docs)):
            if j not in seen:
                ranked.append(j)

        for k in ks:
            score = recall_any_at_k(ranked, answer_sids, corpus_ids, k)
            recalls[k].append(score)
            per_type[qtype][k].append(score)

        if (i + 1) % 25 == 0 or i == total - 1:
            r5 = sum(recalls[5]) / max(len(recalls[5]), 1)
            print(f"  [{i+1:>3}/{total}]  R@5={r5:.1%}  med_lat={sorted(latencies)[len(latencies)//2]:.1f}ms", flush=True)

    return {
        "backend": "mempalace",
        "questions": len(recalls[5]),
        "recall": {k: sum(v) / max(len(v), 1) for k, v in recalls.items()},
        "per_type": {
            qt: {k: sum(v) / max(len(v), 1) for k, v in kd.items()}
            for qt, kd in per_type.items()
        },
        "latency_p50_ms": sorted(latencies)[len(latencies) // 2] if latencies else 0,
        "latency_p95_ms": sorted(latencies)[int(len(latencies) * 0.95)] if latencies else 0,
    }


# ── Output ────────────────────────────────────────────────────────────────────

def print_results(results: list[dict]) -> None:
    ks = [1, 3, 5, 10]
    print()
    print("LongMemEval Benchmark Results")
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

    # Per question-type breakdown
    if any(r.get("per_type") for r in results):
        all_types = sorted({qt for r in results if r for qt in r.get("per_type", {})})
        print(f"R@5 by question type:")
        print(f"  {'Type':<35}", end="")
        for r in results:
            if r:
                print(f"  {r['backend'][:14]:>14}", end="")
        print()
        print(f"  {'-'*35}", end="")
        for r in results:
            if r:
                print(f"  {'':>14}", end="")
        print()
        for qt in all_types:
            print(f"  {qt:<35}", end="")
            for r in results:
                if not r:
                    continue
                v = r["per_type"].get(qt, {}).get(5, None)
                s = f"{v:.1%}" if v is not None else "—"
                print(f"  {s:>14}", end="")
            print()
        print()

    print(f"mempalace published baseline (raw mode): 96.6% R@5")
    print()


# ── CLI ───────────────────────────────────────────────────────────────────────

def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="LongMemEval benchmark: ironrace-memory vs mempalace.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument(
        "data",
        nargs="?",
        default=None,
        help="Path to longmemeval_s_cleaned.json (auto-downloaded to ~/.cache/ironrace/ if omitted)",
    )
    p.add_argument(
        "--backend",
        choices=["ironrace", "mempalace", "both"],
        default="ironrace",
        help="Which backend(s) to benchmark (default: ironrace)",
    )
    p.add_argument(
        "--limit",
        type=int,
        default=500,
        help="Max questions to evaluate (default: 500, use 50 for a quick check)",
    )
    p.add_argument(
        "--n-results",
        type=int,
        default=10,
        help="Number of search results to retrieve per query (default: 10)",
    )
    p.add_argument(
        "--granularity",
        choices=["session", "turn"],
        default="session",
        help="Indexing granularity: session (default) or turn",
    )
    p.add_argument(
        "--ironmem-binary",
        default="./target/release/ironmem",
        help="Path to ironmem binary",
    )
    p.add_argument(
        "--ef-search",
        type=int,
        default=None,
        help="Override HNSW ef_search (default: auto formula)",
    )
    p.add_argument(
        "--mempalace-python",
        default="/opt/homebrew/bin/python3.11",
        help="Python interpreter for mempalace (needs chromadb)",
    )
    p.add_argument(
        "--output-json",
        default=None,
        help="Write results to JSON file",
    )
    return p.parse_args()


def main() -> int:
    args = parse_args()

    data_path = _ensure_dataset(args.data)

    print(f"Loading {data_path.name}...", flush=True)
    with open(data_path) as f:
        data = json.load(f)
    print(f"  {len(data)} questions loaded. Running first {min(args.limit, len(data))}.", flush=True)

    results: list[dict] = []

    if args.backend in ("ironrace", "both"):
        ironmem_binary = Path(args.ironmem_binary).expanduser().resolve()
        if not ironmem_binary.exists():
            print(f"ironmem binary not found: {ironmem_binary}", file=sys.stderr)
            return 1
        ef_label = f"  ef_search={args.ef_search}" if args.ef_search else ""
        print(f"\nironrace-memory{ef_label}:", flush=True)
        r = run_ironrace_benchmark(
            data=data,
            ironmem_binary=str(ironmem_binary),
            limit=args.limit,
            n_results=args.n_results,
            granularity=args.granularity,
            ef_search=args.ef_search,
        )
        results.append(r)

    if args.backend in ("mempalace", "both"):
        # Switch to python3.11 subprocess to import chromadb
        if args.mempalace_python != sys.executable:
            # Re-invoke this script under python3.11 for the mempalace run
            print(f"\nmempalace (via {args.mempalace_python}):", flush=True)
            mp_result = subprocess.run(
                [args.mempalace_python, __file__,
                 str(data_path),  # always pass resolved path to sub-process
                 "--backend", "mempalace",
                 "--limit", str(args.limit),
                 "--n-results", str(args.n_results),
                 "--granularity", args.granularity,
                 "--output-json", "/tmp/_lme_mempalace_result.json"],
                capture_output=False,
            )
            if mp_result.returncode == 0 and Path("/tmp/_lme_mempalace_result.json").exists():
                with open("/tmp/_lme_mempalace_result.json") as f:
                    results.extend(json.load(f))
        else:
            print(f"\nmempalace:", flush=True)
            r = run_mempalace_benchmark(
                data=data,
                limit=args.limit,
                n_results=args.n_results,
                granularity=args.granularity,
            )
            results.append(r)

    print_results(results)

    if args.output_json:
        Path(args.output_json).write_text(json.dumps(results, indent=2))
        print(f"Results written to {args.output_json}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())

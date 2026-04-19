#!/usr/bin/env python3
"""MemBench (ACL 2025) benchmark for ironmem.

Tests memory retrieval across factual and reflective scenarios, from both
participation (first-person) and observation (third-person) perspectives.

Mempalace published baseline:
  80.3% R@5  (all categories)

Dataset: import-myself/Membench on GitHub — ~8,500 items at multiple context
lengths. Pre-sampled datasets at 0-10k token context are used by default.

Download instructions (run once before benchmarking):
    git clone https://github.com/import-myself/Membench ~/.cache/ironrace/Membench
    # OR download the data2test/ directory and pass --data-dir

The pre-sampled files used:
    data2test/participation_factual_0_10k.json
    data2test/participation_reflective_0_10k.json
    data2test/observation_factual_0_10k.json
    data2test/observation_reflective_0_10k.json

Usage:
    python3 scripts/benchmark_membench.py --data-dir ~/.cache/ironrace/Membench
    python3 scripts/benchmark_membench.py --data-dir /path/to/Membench --limit 500
    python3 scripts/benchmark_membench.py --data-dir /path/to/Membench --n-results 5
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

_DEFAULT_CACHE = Path.home() / ".cache" / "ironrace" / "Membench"

# Raw Membench repo structure: MemData/{FirstAgent,ThirdAgent}/*.json
_AGENT_PERSPECTIVE = {"FirstAgent": "participation", "ThirdAgent": "observation"}

# Pre-sampled files at 0-10k token context (one per scenario × memory level)
_DATA_FILES = [
    ("participation", "factual",    "data2test/participation_factual_0_10k.json"),
    ("participation", "reflective", "data2test/participation_reflective_0_10k.json"),
    ("observation",   "factual",    "data2test/observation_factual_0_10k.json"),
    ("observation",   "reflective", "data2test/observation_reflective_0_10k.json"),
]

# Fallback: try top-level data directory filenames (repo may differ)
_ALT_DATA_FILES = [
    ("participation", "factual",    "data/participation_factual.json"),
    ("participation", "reflective", "data/participation_reflective.json"),
    ("observation",   "factual",    "data/observation_factual.json"),
    ("observation",   "reflective", "data/observation_reflective.json"),
]


def _find_data_files(data_dir: Path) -> list[tuple[str, str, Path]]:
    """Return [(perspective, memory_level, path)] for each found data file."""
    # Prefer raw Membench repo structure: MemData/{FirstAgent,ThirdAgent}/*.json
    raw_found: list[tuple[str, str, Path]] = []
    for agent_dir, perspective in _AGENT_PERSPECTIVE.items():
        agent_path = data_dir / "MemData" / agent_dir
        if agent_path.is_dir():
            for json_file in sorted(agent_path.glob("*.json")):
                raw_found.append((perspective, json_file.stem, json_file))
    if raw_found:
        return raw_found

    # Fall back to pre-processed data2test/ or data/ files
    found: list[tuple[str, str, Path]] = []
    for perspective, level, rel in _DATA_FILES + _ALT_DATA_FILES:
        p = data_dir / rel
        if p.exists():
            if not any(x[0] == perspective and x[1] == level for x in found):
                found.append((perspective, level, p))
    return found


def _ensure_data_dir(data_dir: Path) -> list[tuple[str, str, Path]]:
    found = _find_data_files(data_dir)
    if found:
        return found

    print(
        f"MemBench data not found in {data_dir}.\n"
        "\nTo download the dataset, run:\n"
        "    git clone https://github.com/import-myself/Membench "
        f"{_DEFAULT_CACHE}\n\n"
        "Then rerun with:\n"
        f"    python3 scripts/benchmark_membench.py --data-dir {_DEFAULT_CACHE}",
        file=sys.stderr,
    )
    sys.exit(1)


# ── Dataset helpers ───────────────────────────────────────────────────────────

def _build_dialogue(message_list: list) -> str:
    """Flatten a Membench message_list (list-of-sessions or list-of-turns) into a string."""
    turns: list[str] = []
    for item in message_list:
        if isinstance(item, list):
            # List of turns within a session
            for turn in item:
                if not isinstance(turn, dict):
                    continue
                u = turn.get("user_message", turn.get("user", ""))
                a = turn.get("assistant_message", turn.get("assistant", ""))
                t = turn.get("time", "")
                pl = turn.get("place", "")
                meta = f" (time: {t}, place: {pl})" if t or pl else ""
                if u:
                    turns.append(f"User: {u}{meta}")
                if a:
                    turns.append(f"Assistant: {a}")
        elif isinstance(item, dict):
            u = item.get("user_message", item.get("user", ""))
            a = item.get("assistant_message", item.get("assistant", ""))
            t = item.get("time", "")
            pl = item.get("place", "")
            meta = f" (time: {t}, place: {pl})" if t or pl else ""
            if u:
                turns.append(f"User: {u}{meta}")
            if a:
                turns.append(f"Assistant: {a}")
        elif isinstance(item, str):
            turns.append(item)
    return "\n".join(turns)


def _turn_text(turn: dict) -> str:
    """Render one MemBench turn the same way MemPal's benchmark does."""
    user = turn.get("user") or turn.get("user_message", "")
    asst = turn.get("assistant") or turn.get("assistant_message", "")
    t = turn.get("time", "")
    text = f"[User] {user} [Assistant] {asst}"
    if t:
        text = f"[{t}] {text}"
    return text


def _iter_turns(message_list: list) -> list[dict]:
    """Flatten MemBench message_list into indexed turns with sid/global metadata."""
    if not message_list:
        return []

    sessions = [message_list] if isinstance(message_list[0], dict) else message_list
    turns: list[dict] = []
    global_idx = 0

    for s_idx, session in enumerate(sessions):
        if not isinstance(session, list):
            continue
        for t_idx, turn in enumerate(session):
            if not isinstance(turn, dict):
                continue
            sid = turn.get("sid", turn.get("mid"))
            turns.append({
                "sid": int(sid) if isinstance(sid, (int, float)) else global_idx,
                "global_idx": global_idx,
                "s_idx": s_idx,
                "t_idx": t_idx,
                "content": _turn_text(turn),
            })
            global_idx += 1

    return turns


def _load_items(path: Path, perspective: str, level: str) -> list[dict]:
    """Load and tag items from one MemBench data file."""
    with open(path) as f:
        raw = json.load(f)

    items: list[dict] = []

    def _add_entry(entry: dict) -> None:
        if not isinstance(entry, dict):
            return
        qa = entry.get("QA", {})
        msg_list = entry.get("message_list", [])
        dialogue = (
            _build_dialogue(msg_list)
            if msg_list
            else entry.get("dialogue", entry.get("context", entry.get("conversation", "")))
        )
        def _str(v: object) -> str:
            if isinstance(v, list):
                return ", ".join(str(x) for x in v)
            return str(v) if v is not None else ""

        items.append({
            "perspective": perspective,
            "memory_level": level,
            "question": _str(qa.get("question", entry.get("question", entry.get("query", "")))),
            "answer": _str(qa.get("answer", entry.get("answer", ""))),
            "evidence": _str(qa.get("evidence", entry.get("evidence", entry.get("evidence_text", "")))),
            "ground_truth": _str(qa.get("ground_truth", entry.get("ground_truth", ""))),
            "choices": qa.get("choices", entry.get("choices", {})),
            "target_step_ids": qa.get("target_step_id", entry.get("target_step_id", [])),
            "message_list": msg_list,
            "dialogue": dialogue,
        })

    if isinstance(raw, list):
        for entry in raw:
            _add_entry(entry)
    elif isinstance(raw, dict):
        if "data" in raw:
            for entry in raw["data"]:
                _add_entry(entry)
        else:
            # Raw Membench format: {"roles": [...], "events": [...]}
            for sublist in raw.values():
                if isinstance(sublist, list):
                    for entry in sublist:
                        _add_entry(entry)
    else:
        print(f"  warning: unexpected format in {path}, skipping.", file=sys.stderr)

    return items


def _split_dialogue_into_chunks(dialogue: str, max_chunk_chars: int = 500) -> list[str]:
    """Split a dialogue string into ~500-char chunks at turn boundaries."""
    if not dialogue:
        return []

    # Split on common turn markers
    markers = ["\nUser:", "\nAssistant:", "\nSpeaker", "\nA:", "\nB:", "\n\n"]
    chunks: list[str] = []
    current = ""

    lines = dialogue.splitlines(keepends=True)
    for line in lines:
        if len(current) + len(line) > max_chunk_chars and current.strip():
            chunks.append(current.strip())
            current = line
        else:
            current += line

    if current.strip():
        chunks.append(current.strip())

    return chunks if chunks else [dialogue[:max_chunk_chars]]


def _target_turn_ids(target_step_ids: object) -> set[int]:
    """Extract the primary sid/global index from MemBench target_step_id records."""
    target_ids: set[int] = set()
    if not isinstance(target_step_ids, list):
        return target_ids

    for step in target_step_ids:
        if isinstance(step, list) and step:
            value = step[0]
        else:
            value = step

        if isinstance(value, (int, float)):
            target_ids.add(int(value))

    return target_ids


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
        if resp.get("result", {}).get("isError"):
            content = resp.get("result", {}).get("content", [])
            message = content[0].get("text", "unknown tool error") if content else "unknown tool error"
            raise RuntimeError(f"{self.name} tool error: {message}")
        content = resp.get("result", {}).get("content", [])
        if content and content[0].get("type") == "text":
            try:
                return json.loads(content[0]["text"])
            except json.JSONDecodeError:
                return {"raw": content[0]["text"]}
        return {}


# ── Benchmark runner ──────────────────────────────────────────────────────────

def run_membench_benchmark(
    items: list[dict],
    binary: str,
    limit: int,
    n_results: int,
    top_k: int,
    ef_search: int | None,
) -> dict:
    """Run MemBench retrieval benchmark against ironmem.

    Each item's turns are ingested one drawer per turn into its own wing.
    A hit is scored when any retrieved turn matches the MemBench
    `target_step_id` sid/global index, mirroring MemPal's benchmark logic.
    """
    tmp = Path(tempfile.mkdtemp(prefix="ironmem-membench-"))
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

    items = items[:limit]
    total = len(items)

    hits: list[float] = []
    per_scenario: dict[str, list[float]] = defaultdict(list)
    search_latencies: list[float] = []

    try:
        client.start(wait_for_embedder=True)
        print(f"  Model loaded. Running {total} items...", flush=True)

        for i, item in enumerate(items):
            question = item.get("question", "")
            message_list = item.get("message_list", [])
            perspective = item.get("perspective", "unknown")
            level = item.get("memory_level", "unknown")
            scenario = f"{perspective}/{level}"
            target_ids = _target_turn_ids(item.get("target_step_ids", []))

            if not question:
                continue

            turns = _iter_turns(message_list) if isinstance(message_list, list) else []
            if not turns:
                continue

            wing = f"item{i}"
            drawer_targets: dict[str, tuple[int, int]] = {}
            for turn in turns:
                payload = client.call_tool("add_drawer", {
                    "content": turn["content"],
                    "wing": wing,
                    "room": "turn",
                })
                drawer_id = payload.get("id")
                if isinstance(drawer_id, str):
                    drawer_targets[drawer_id] = (turn["sid"], turn["global_idx"])

            t0 = time.perf_counter()
            payload = client.call_tool("search", {
                "query": question,
                "limit": n_results,
                "wing": wing,
            })
            elapsed_ms = (time.perf_counter() - t0) * 1000
            search_latencies.append(elapsed_ms)

            results = payload.get("results", [])
            retrieved_sids: set[int] = set()
            retrieved_global: set[int] = set()
            for result in results[:top_k]:
                drawer_id = result.get("id")
                if not isinstance(drawer_id, str):
                    continue
                target = drawer_targets.get(drawer_id)
                if target is None:
                    continue
                sid, global_idx = target
                retrieved_sids.add(sid)
                retrieved_global.add(global_idx)

            hit = bool(target_ids & retrieved_sids) or bool(target_ids & retrieved_global)
            hits.append(1.0 if hit else 0.0)
            per_scenario[scenario].append(1.0 if hit else 0.0)

            if (i + 1) % 100 == 0 or i == total - 1:
                r5 = sum(hits) / max(len(hits), 1)
                med = sorted(search_latencies)[len(search_latencies) // 2]
                print(
                    f"  [{i+1:>4}/{total}]  R@{top_k}={r5:.1%}  med_search={med:.1f}ms",
                    flush=True,
                )

    finally:
        client.stop()
        shutil.rmtree(tmp, ignore_errors=True)

    sl = sorted(search_latencies)
    return {
        "backend": "ironmem",
        "items_scored": len(hits),
        f"recall_at_{top_k}": sum(hits) / max(len(hits), 1),
        "per_scenario": {
            sc: sum(v) / max(len(v), 1) for sc, v in per_scenario.items()
        },
        "per_scenario_total": {sc: len(v) for sc, v in per_scenario.items()},
        "latency_p50_ms": sl[len(sl) // 2] if sl else 0,
        "latency_p95_ms": sl[int(len(sl) * 0.95)] if sl else 0,
    }


# ── Output ────────────────────────────────────────────────────────────────────

def print_results(results: list[dict], top_k: int) -> None:
    key = f"recall_at_{top_k}"
    print()
    print("MemBench Benchmark Results")
    print("=" * 65)
    print(f"{'Backend':<22}  {f'R@{top_k}':>8}  {'p50':>8}  {'p95':>8}  {'n':>6}")
    print("-" * 65)
    for r in results:
        if not r:
            continue
        print(
            f"{r['backend']:<22}  "
            f"{r[key]:>8.1%}  "
            f"{r['latency_p50_ms']:>7.1f}ms  "
            f"{r['latency_p95_ms']:>7.1f}ms  "
            f"{r['items_scored']:>6}"
        )
    print()

    if any(r.get("per_scenario") for r in results):
        all_sc = sorted({sc for r in results if r for sc in r.get("per_scenario", {})})
        print(f"R@{top_k} by scenario:")
        for sc in all_sc:
            print(f"  {sc:<30}", end="")
            for r in results:
                if not r:
                    continue
                v = r["per_scenario"].get(sc)
                n = r.get("per_scenario_total", {}).get(sc, 0)
                s = f"{v:.1%}" if v is not None else "—"
                print(f"  {s:>8}  (n={n})", end="")
            print()
        print()

    print(f"mempalace baseline (all categories): 80.3% R@5")
    print()


# ── CLI ───────────────────────────────────────────────────────────────────────

def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="MemBench (ACL 2025) retrieval benchmark for ironmem.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Download the dataset once:
    git clone https://github.com/import-myself/Membench ~/.cache/ironrace/Membench

Then run:
    python3 scripts/benchmark_membench.py --data-dir ~/.cache/ironrace/Membench
""",
    )
    p.add_argument(
        "--data-dir",
        default=str(_DEFAULT_CACHE),
        help=f"Path to cloned Membench repo (default: {_DEFAULT_CACHE})",
    )
    p.add_argument("--limit", type=int, default=8500, help="Max items to evaluate (default: 8500)")
    p.add_argument("--n-results", type=int, default=10, help="Results to retrieve per query (default: 10)")
    p.add_argument("--top-k", type=int, default=5, help="Top-k threshold for R@k scoring (default: 5)")
    p.add_argument(
        "--ironmem-binary",
        default="./target/release/ironmem",
        help="Path to ironmem binary",
    )
    p.add_argument("--ef-search", type=int, default=None, help="Override HNSW ef_search")
    p.add_argument("--output-json", default=None, help="Write results to JSON file")
    return p.parse_args()


def main() -> int:
    args = parse_args()
    data_dir = Path(args.data_dir).expanduser().resolve()
    data_files = _ensure_data_dir(data_dir)

    items: list[dict] = []
    for perspective, level, path in data_files:
        loaded = _load_items(path, perspective, level)
        print(f"  {path.name}: {len(loaded)} items ({perspective}/{level})", flush=True)
        items.extend(loaded)

    print(f"Total: {len(items)} items across {len(data_files)} files.", flush=True)

    binary = Path(args.binary).expanduser().resolve()
    if not binary.exists():
        print(f"ironmem binary not found: {binary}", file=sys.stderr)
        print("Build it with: cargo build --release -p ironmem --bin ironmem", file=sys.stderr)
        return 1

    ef_label = f"  ef_search={args.ef_search}" if args.ef_search else ""
    print(f"\nironmem{ef_label}:", flush=True)
    result = run_membench_benchmark(
        items=items,
        binary=str(binary),
        limit=args.limit,
        n_results=args.n_results,
        top_k=args.top_k,
        ef_search=args.ef_search,
    )
    print_results([result], args.top_k)

    if args.output_json:
        Path(args.output_json).write_text(json.dumps([result], indent=2))
        print(f"Results written to {args.output_json}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())

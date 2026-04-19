#!/usr/bin/env python3
"""ConvoMem benchmark for ironmem.

Tests retrieval recall across 6 conversational memory categories.
Matches MemPal's published ConvoMem harness: 250 items (50 per category),
one drawer per message, scored at top-10.

Mempalace published baseline:
  92.9% avg recall  (all categories, 250 items, top-10)

Dataset: Salesforce/ConvoMem on HuggingFace (75K QA pairs).
The Hugging Face dataset viewer and `datasets.load_dataset(..., streaming=True)`
currently fail on this repo because the published split mixes incompatible JSON
schemas. This script reads the raw benchmark JSON files directly from the HF
repo and samples from the `1_evidence` subset MemPal uses, preferring
`evidence_questions/<category>/1_evidence` and falling back to the equivalent
`pre_mixed_testcases/<category>/1_evidence` files when only those are cached.

Six categories evaluated:
  user_evidence               — user states facts about themselves
  assistant_facts_evidence    — assistant provided information
  changing_evidence           — track state changes across messages
  abstention_evidence         — information was never stated
  preference_evidence         — apply user preferences
  implicit_connection_evidence — multi-hop reasoning across messages

For abstention items, the upstream MemPal benchmark treats the item as a
trivial pass when no evidence messages are present. This script mirrors that
behavior for apples-to-apples comparisons.

Usage:
    python3 scripts/benchmark_convomem.py
    python3 scripts/benchmark_convomem.py --n-per-category 50
    python3 scripts/benchmark_convomem.py --skip-abstention
    python3 scripts/benchmark_convomem.py --output-json results.json
"""

from __future__ import annotations

import argparse
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
import time
from collections import defaultdict
from pathlib import Path

_CATEGORIES = [
    "user_evidence",
    "assistant_facts_evidence",
    "changing_evidence",
    "abstention_evidence",
    "preference_evidence",
    "implicit_connection_evidence",
]

# Shorter aliases for display
_CAT_LABELS = {
    "user_evidence": "user_evidence",
    "assistant_facts_evidence": "assistant_facts",
    "changing_evidence": "changing",
    "abstention_evidence": "abstention",
    "preference_evidence": "preference",
    "implicit_connection_evidence": "implicit_conn",
}

_HF_REPO_ID = "Salesforce/ConvoMem"
_HF_EVIDENCE_PREFIX = "core_benchmark/evidence_questions/"
_HF_PREMIXED_PREFIX = "core_benchmark/pre_mixed_testcases/"
_HF_CACHE_ROOT = Path.home() / ".cache" / "huggingface" / "hub" / "datasets--Salesforce--ConvoMem" / "snapshots"


# ── Dataset loading ───────────────────────────────────────────────────────────

def _stream_sample(n_per_category: int, seed: int) -> list[dict]:
    """Sample ConvoMem items from raw HF repo files.

    The dataset's advertised `train` split currently mixes flat rows with
    nested `evidence_items` JSON documents, which breaks `datasets` streaming.
    We bypass that split entirely and read the raw `1_evidence` files, using
    the same subset MemPal benchmarks against.
    """
    try:
        from huggingface_hub import HfApi, hf_hub_download  # type: ignore
    except ImportError:
        print(
            "error: huggingface_hub not installed. Run: pip install huggingface_hub",
            file=sys.stderr,
        )
        sys.exit(1)

    print(
        f"Streaming Salesforce/ConvoMem from HuggingFace "
        f"(sampling {n_per_category} per category)...",
        flush=True,
    )

    rng = random.Random(seed)
    reservoir: dict[str, list[dict]] = {c: [] for c in _CATEGORIES}
    counts: dict[str, int] = {c: 0 for c in _CATEGORIES}
    filled: set[str] = set()

    data_files = _list_convomem_files(HfApi())
    if not data_files:
        print("error: no ConvoMem 1_evidence benchmark files found", file=sys.stderr)
        sys.exit(1)

    rng.shuffle(data_files)

    for path in data_files:
        category = _category_from_path(path)
        if category not in reservoir or category in filled:
            continue

        local_path = _resolve_local_convomem_file(path, hf_hub_download)
        with open(local_path) as f:
            payload = json.load(f)

        for item in _iter_evidence_items(payload, default_category=category):
            cat = item.get("category")
            if cat not in reservoir:
                continue

            counts[cat] += 1
            n = counts[cat]

            # Reservoir sampling so later items can replace earlier ones.
            if len(reservoir[cat]) < n_per_category:
                reservoir[cat].append(item)
            else:
                j = rng.randint(0, n - 1)
                if j < n_per_category:
                    reservoir[cat][j] = item

            if len(reservoir[cat]) >= n_per_category and cat not in filled:
                filled.add(cat)
                print(f"  category '{cat}': {n_per_category} sampled", flush=True)

            if len(filled) == len(_CATEGORIES):
                break
        if len(filled) == len(_CATEGORIES):
            break

    result: list[dict] = []
    for cat in _CATEGORIES:
        items = reservoir[cat]
        if not items:
            print(f"  warning: no items found for category '{cat}'", file=sys.stderr)
        result.extend(items)

    return result


def _load_local(path: Path) -> list[dict]:
    """Load a pre-sampled ConvoMem JSON file (list of normalized items)."""
    with open(path) as f:
        data = json.load(f)
    if not isinstance(data, list):
        raise ValueError(f"Expected a list at top level, got {type(data).__name__}")
    return data


def _category_from_path(path: str) -> str | None:
    parts = Path(path).parts
    for category in _CATEGORIES:
        if category in parts:
            return category
    return None


def _prioritize_convomem_files(paths: list[str]) -> list[str]:
    """Prefer `1_evidence` files per category, but backfill gaps from pre-mixed files."""
    preferred_by_cat: dict[str, list[str]] = {cat: [] for cat in _CATEGORIES}
    fallback_by_cat: dict[str, list[str]] = {cat: [] for cat in _CATEGORIES}

    for path in paths:
        category = _category_from_path(path)
        if category is None:
            continue

        if (
            (path.startswith(_HF_EVIDENCE_PREFIX) or path.startswith(_HF_PREMIXED_PREFIX))
            and "/1_evidence/" in path
        ):
            preferred_by_cat[category].append(path)
        elif path.startswith(_HF_PREMIXED_PREFIX):
            fallback_by_cat[category].append(path)

    ordered: list[str] = []
    for category in _CATEGORIES:
        preferred = preferred_by_cat[category]
        if preferred:
            ordered.extend(preferred)
        else:
            ordered.extend(fallback_by_cat[category])
    return ordered


def _list_convomem_files(api: object) -> list[str]:
    try:
        repo_files = api.list_repo_files(repo_id=_HF_REPO_ID, repo_type="dataset")
        candidates = [
            path for path in repo_files
            if (
                path.endswith(".json")
                and (
                    path.startswith(_HF_EVIDENCE_PREFIX)
                    or path.startswith(_HF_PREMIXED_PREFIX)
                )
            )
        ]
        prioritized = _prioritize_convomem_files(candidates)
        if prioritized:
            return prioritized
    except Exception:
        pass

    cached: list[str] = []
    if _HF_CACHE_ROOT.exists():
        for path in _HF_CACHE_ROOT.glob("**/core_benchmark/**/*.json"):
            try:
                rel = path.relative_to(next(parent for parent in path.parents if parent.parent == _HF_CACHE_ROOT))
            except StopIteration:
                continue
            rel_posix = rel.as_posix()
            if rel_posix.startswith(_HF_EVIDENCE_PREFIX) or rel_posix.startswith(_HF_PREMIXED_PREFIX):
                cached.append(rel_posix)
    return _prioritize_convomem_files(cached)


def _resolve_local_convomem_file(path: str, hf_hub_download) -> Path:
    for snapshot in _HF_CACHE_ROOT.glob("*"):
        candidate = snapshot / path
        if candidate.exists():
            return candidate
    return Path(hf_hub_download(repo_id=_HF_REPO_ID, repo_type="dataset", filename=path))


def _iter_evidence_items(payload: object, default_category: str | None) -> list[dict]:
    """Normalize HF payloads into the flat item shape used by the benchmark."""
    items: list[dict] = []

    if isinstance(payload, dict):
        bundles = [payload]
    elif isinstance(payload, list):
        bundles = payload
    else:
        return items

    for bundle in bundles:
        if not isinstance(bundle, dict):
            continue

        raw_items = bundle.get("evidenceItems") or bundle.get("evidence_items") or []
        if not isinstance(raw_items, list):
            continue

        for raw in raw_items:
            if not isinstance(raw, dict):
                continue
            category = default_category or raw.get("evidence_type") or raw.get("category")
            if category not in _CATEGORIES:
                continue
            items.append({
                "category": category,
                "question": raw.get("question", ""),
                "answer": raw.get("answer", ""),
                "conversations": raw.get("conversations", bundle.get("conversations", [])),
                "messages": raw.get("messages", []),
                "message_evidences": raw.get("message_evidences", []),
                "persona": raw.get("persona") or raw.get("personId"),
            })
    return items


# ── Evidence extraction ───────────────────────────────────────────────────────

def _extract_messages(item: dict) -> list[str]:
    """Return all conversation messages to ingest as drawers."""
    messages: list[str] = []

    # Raw HF files store full conversations under `conversations`, while older
    # pre-sampled files may already have a flat top-level `messages` list.
    for conv in item.get("conversations", []):
        for msg in conv.get("messages", []):
            text = msg.get("text", "")
            speaker = msg.get("speaker", "")
            if text:
                messages.append(f"{speaker}: {text}" if speaker else text)

    if messages:
        return messages

    for msg in item.get("messages", []):
        text = msg.get("text", "")
        speaker = msg.get("speaker", "")
        if text:
            messages.append(f"{speaker}: {text}" if speaker else text)

    return messages


def _extract_evidence_texts(item: dict) -> list[str]:
    """Return the evidence message texts that should be retrieved for this item."""
    texts: list[str] = []
    for ev in item.get("message_evidences", []):
        text = ev.get("text", "")
        speaker = ev.get("speaker", "")
        if text:
            texts.append(f"{speaker}: {text}" if speaker else text)
    return texts


def _is_evidence_hit(results: list[dict], evidence_texts: list[str], top_k: int) -> bool:
    """Return True if any evidence text appears in any of the top-k results."""
    top_contents = [r.get("content", "") for r in results[:top_k]]
    for ev in evidence_texts:
        ev_lower = ev.lower()
        for content in top_contents:
            if ev_lower in content.lower():
                return True
    return False


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

def run_convomem_benchmark(
    items: list[dict],
    binary: str,
    n_results: int,
    top_k: int,
    skip_abstention: bool,
    ef_search: int | None,
) -> dict:
    """Run ConvoMem retrieval recall benchmark against ironmem.

    Each item gets its own wing for isolation. All conversation messages are
    ingested; retrieval is scored by whether any evidence message appears in
    the top-k results.
    """
    tmp = Path(tempfile.mkdtemp(prefix="ironmem-convomem-"))
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

    per_cat_hits: dict[str, int] = defaultdict(int)
    per_cat_total: dict[str, int] = defaultdict(int)
    search_latencies: list[float] = []

    try:
        client.start(wait_for_embedder=True)
        print(f"  Model loaded. Running {len(items)} items...", flush=True)

        for i, item in enumerate(items):
            cat = item.get("category", "unknown")

            if skip_abstention and cat == "abstention_evidence":
                continue

            question = item.get("question", "")
            if not question:
                continue

            messages = _extract_messages(item)
            evidence_texts = _extract_evidence_texts(item)

            if not messages:
                continue

            # Match MemPal's benchmark: no evidence messages is a trivial pass.
            if cat == "abstention_evidence":
                per_cat_total[cat] += 1
                per_cat_hits[cat] += 1
                continue

            if not evidence_texts:
                continue

            wing = f"item{i}"

            for j, msg in enumerate(messages):
                client.call_tool("add_drawer", {
                    "content": msg,
                    "wing": wing,
                    "room": "message",
                })

            t0 = time.perf_counter()
            payload = client.call_tool("search", {
                "query": question,
                "limit": n_results,
                "wing": wing,
            })
            elapsed_ms = (time.perf_counter() - t0) * 1000
            search_latencies.append(elapsed_ms)

            results = payload.get("results", [])
            hit = _is_evidence_hit(results, evidence_texts, top_k)

            per_cat_total[cat] += 1
            if hit:
                per_cat_hits[cat] += 1

            if (i + 1) % 50 == 0:
                scored = sum(per_cat_total.values())
                total_hits = sum(per_cat_hits.values())
                avg = total_hits / max(scored, 1)
                med = sorted(search_latencies)[len(search_latencies) // 2] if search_latencies else 0
                print(
                    f"  [{i+1:>3}/{len(items)}]  scored={scored}  "
                    f"avg_recall={avg:.1%}  med_search={med:.1f}ms",
                    flush=True,
                )

    finally:
        client.stop()
        shutil.rmtree(tmp, ignore_errors=True)

    sl = sorted(search_latencies)
    total_scored = sum(per_cat_total.values())
    total_hits = sum(per_cat_hits.values())
    avg_recall = total_hits / max(total_scored, 1)

    return {
        "backend": "ironmem",
        "items_scored": total_scored,
        "avg_recall": avg_recall,
        "per_category": {c: per_cat_hits[c] / per_cat_total[c] for c in per_cat_total if per_cat_total[c] > 0},
        "per_category_total": dict(per_cat_total),
        "latency_p50_ms": sl[len(sl) // 2] if sl else 0,
        "latency_p95_ms": sl[int(len(sl) * 0.95)] if sl else 0,
    }


# ── Output ────────────────────────────────────────────────────────────────────

def print_results(results: list[dict]) -> None:
    print()
    print("ConvoMem Benchmark Results")
    print("=" * 60)
    print(f"{'Backend':<22}  {'Avg Recall':>10}  {'p50':>8}  {'p95':>8}")
    print("-" * 60)
    for r in results:
        if not r:
            continue
        print(
            f"{r['backend']:<22}  "
            f"{r['avg_recall']:>10.1%}  "
            f"{r['latency_p50_ms']:>7.1f}ms  "
            f"{r['latency_p95_ms']:>7.1f}ms"
        )
    print()

    if any(r.get("per_category") for r in results):
        all_cats = sorted({c for r in results if r for c in r.get("per_category", {})})
        print("Recall by category:")
        print(f"  {'Category':<35}", end="")
        for r in results:
            if r:
                print(f"  {'n':>5}  {'recall':>8}", end="")
        print()
        print(f"  {'-'*35}", end="")
        for r in results:
            if r:
                print(f"  {'---':>5}  {'------':>8}", end="")
        print()
        for cat in all_cats:
            label = _CAT_LABELS.get(cat, cat)
            print(f"  {label:<35}", end="")
            for r in results:
                if not r:
                    continue
                v = r["per_category"].get(cat)
                n = r.get("per_category_total", {}).get(cat, 0)
                s = f"{v:.1%}" if v is not None else "—"
                print(f"  {n:>5}  {s:>8}", end="")
            print()
        print()

    print("mempalace baseline (all categories, 250 items, top-10): 92.9% avg recall")
    print()


# ── CLI ───────────────────────────────────────────────────────────────────────

def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="ConvoMem retrieval recall benchmark for ironmem.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument(
        "data",
        nargs="?",
        default=None,
        help="Path to pre-sampled JSON file (streams from HuggingFace if omitted)",
    )
    p.add_argument(
        "--n-per-category",
        type=int,
        default=50,
        help="Items to sample per category (default: 50, total=300 minus abstention)",
    )
    p.add_argument(
        "--n-results",
        type=int,
        default=10,
        help="Results to retrieve per query (default: 10)",
    )
    p.add_argument(
        "--top-k",
        type=int,
        default=10,
        help="Top-k threshold for hit scoring (default: 10, matches MemPal benchmark)",
    )
    p.add_argument(
        "--skip-abstention",
        action="store_true",
        help="Skip abstention_evidence items (no evidence to retrieve)",
    )
    p.add_argument(
        "--ironmem-binary",
        default="./target/release/ironmem",
        help="Path to ironmem binary",
    )
    p.add_argument("--ef-search", type=int, default=None, help="Override HNSW ef_search")
    p.add_argument("--seed", type=int, default=42, help="Random seed for sampling (default: 42)")
    p.add_argument("--output-json", default=None, help="Write results to JSON file")
    p.add_argument(
        "--save-sample",
        default=None,
        metavar="PATH",
        help="Save sampled items to JSON file for reproducible reruns",
    )
    return p.parse_args()


def main() -> int:
    args = parse_args()

    if args.data:
        p = Path(args.data)
        if not p.exists():
            print(f"error: file not found: {p}", file=sys.stderr)
            return 1
        items = _load_local(p)
        print(f"Loaded {len(items)} items from {p}.", flush=True)
    else:
        items = _stream_sample(args.n_per_category, args.seed)
        print(f"Sampled {len(items)} items total.", flush=True)

    if args.save_sample:
        Path(args.save_sample).write_text(json.dumps(items, indent=2))
        print(f"Sample saved to {args.save_sample} (reuse with: python3 {__file__} {args.save_sample})")

    binary = Path(args.binary).expanduser().resolve()
    if not binary.exists():
        print(f"ironmem binary not found: {binary}", file=sys.stderr)
        print("Build it with: cargo build --release -p ironmem --bin ironmem", file=sys.stderr)
        return 1

    ef_label = f"  ef_search={args.ef_search}" if args.ef_search else ""
    print(f"\nironmem{ef_label}:", flush=True)
    result = run_convomem_benchmark(
        items=items,
        binary=str(binary),
        n_results=args.n_results,
        top_k=args.top_k,
        skip_abstention=args.skip_abstention,
        ef_search=args.ef_search,
    )
    print_results([result])

    if args.output_json:
        Path(args.output_json).write_text(json.dumps([result], indent=2))
        print(f"Results written to {args.output_json}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())

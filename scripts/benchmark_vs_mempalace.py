#!/usr/bin/env python3
"""Benchmark ironmem against mempalace through their MCP servers.

This harness focuses on surfaces both systems already expose in common:

- add drawer
- search
- status
- list wings
- taxonomy
- delete drawer

It deliberately avoids file-mining comparisons even though ironmem
implements `mine`, because the two projects have meaningfully different
mining pipelines and this harness is intended to compare common MCP tool
surfaces.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import random
import shutil
import statistics
import subprocess
import sys
import tempfile
import textwrap
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


TECH_TERMS = [
    "authentication middleware",
    "database migration",
    "error handling",
    "connection pooling",
    "retry logic",
    "rate limiting",
    "deployment pipeline",
    "GraphQL subscription",
    "Redis failover",
    "Kubernetes autoscaling",
]

NEEDLES = [
    "PostgreSQL vacuum autovacuum threshold set to 50 percent for table users",
    "Redis cluster failover timeout configured at 30 seconds with sentinel monitoring",
    "Kubernetes horizontal pod autoscaler targets 70 percent CPU utilization",
    "JWT token rotation policy requires refresh every 15 minutes with sliding window",
    "Elasticsearch index sharding strategy uses 5 primary shards with 1 replica each",
]


@dataclass
class Document:
    wing: str
    room: str
    content: str
    needle: str


class McpProcessError(RuntimeError):
    pass


class JsonRpcClient:
    def __init__(self, name: str, cmd: list[str], cwd: Path | None, env: dict[str, str], log_stderr: bool = False) -> None:
        self.name = name
        self.cmd = cmd
        self.cwd = cwd
        self.env = env
        self.log_stderr = log_stderr
        self.proc: subprocess.Popen[str] | None = None
        self._request_id = 0
        self._stderr_file: Any = None

    def start(self, warmup_tool: str | None = None, warmup_timeout: float = 120.0) -> float:
        """Start the server and return ms to initialize response.

        If warmup_tool is provided, polls that tool until warming_up is False
        (or the field is absent). Time-to-ready is tracked separately in
        self.warmup_ms so callers can report it without conflating it with
        connection latency.
        """
        if self.log_stderr:
            import tempfile
            self._stderr_file = tempfile.NamedTemporaryFile(
                mode="w", prefix=f"ironmem-{self.name}-stderr-", suffix=".log",
                delete=False, dir="/tmp",
            )
            stderr_dest = self._stderr_file
            print(f"  [debug] server stderr → {self._stderr_file.name}", file=sys.stderr)
        else:
            stderr_dest = subprocess.DEVNULL

        started = time.perf_counter()
        self.proc = subprocess.Popen(
            self.cmd,
            cwd=str(self.cwd) if self.cwd else None,
            env=self.env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr_dest,
            text=True,
            bufsize=1,
        )
        self.call("initialize", {})
        connect_ms = (time.perf_counter() - started) * 1000

        self.warmup_ms: float = 0.0
        if warmup_tool:
            warmup_start = time.perf_counter()
            deadline = warmup_start + warmup_timeout
            while time.perf_counter() < deadline:
                try:
                    result = self.call_tool(warmup_tool, {})
                    if not result.get("warming_up", False):
                        break
                except Exception:
                    pass
                time.sleep(0.5)
            self.warmup_ms = (time.perf_counter() - warmup_start) * 1000

        return connect_ms

    def stop(self) -> None:
        if not self.proc:
            return
        if self.proc.stdin:
            self.proc.stdin.close()
        try:
            self.proc.terminate()
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=5)
        finally:
            self.proc = None
            if self._stderr_file:
                self._stderr_file.flush()
                self._stderr_file.close()
                self._stderr_file = None

    def call(self, method: str, params: dict[str, Any]) -> Any:
        if not self.proc or not self.proc.stdin or not self.proc.stdout:
            raise McpProcessError(f"{self.name}: process is not running")

        self._request_id += 1
        payload = {
            "jsonrpc": "2.0",
            "id": self._request_id,
            "method": method,
            "params": params,
        }
        self.proc.stdin.write(json.dumps(payload) + "\n")
        self.proc.stdin.flush()

        line = self.proc.stdout.readline()
        if not line:
            raise McpProcessError(f"{self.name}: no response for {method}")

        response = json.loads(line)
        if "error" in response:
            raise McpProcessError(f"{self.name}: RPC error for {method}: {response['error']}")
        return response.get("result")

    def call_tool(self, tool_name: str, arguments: dict[str, Any]) -> Any:
        result = self.call("tools/call", {"name": tool_name, "arguments": arguments})
        if not isinstance(result, dict):
            return result
        if result.get("isError"):
            raise McpProcessError(f"{self.name}: tool {tool_name} failed: {result}")
        content = result.get("content", [])
        if not content:
            return result
        text = content[0].get("text", "")
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            return {"raw_text": text}


def percentile(values: list[float], p: float) -> float:
    if not values:
        return 0.0
    if len(values) == 1:
        return values[0]
    ordered = sorted(values)
    rank = (len(ordered) - 1) * p
    lower = math.floor(rank)
    upper = math.ceil(rank)
    if lower == upper:
        return ordered[lower]
    lower_value = ordered[lower]
    upper_value = ordered[upper]
    return lower_value + (upper_value - lower_value) * (rank - lower)


def generate_documents(count: int, seed: int) -> list[Document]:
    rng = random.Random(seed)
    wings = ["backend_api", "webapp", "mobile_app", "docs_site", "devops"]
    rooms = ["auth", "database", "tests", "deployment", "search", "api", "cache"]
    documents: list[Document] = []
    for i in range(count):
        wing = wings[i % len(wings)]
        room = rooms[i % len(rooms)]
        term = TECH_TERMS[i % len(TECH_TERMS)]
        needle = NEEDLES[i % len(NEEDLES)]
        extra = rng.choice(
            [
                "Migration completed after a rollback rehearsal.",
                "Regression coverage was added for the failure mode.",
                "Operations approved the rollout after a canary check.",
                "The issue was traced to an environment mismatch.",
                "A follow-up task was created for edge-case handling.",
            ]
        )
        content = (
            f"Document {i}. Wing {wing}. Room {room}. "
            f"Primary topic: {term}. "
            f"Needle: {needle}. "
            f"{extra}"
        )
        documents.append(Document(wing=wing, room=room, content=content, needle=needle))
    return documents


def dir_size_bytes(path: Path) -> int:
    total = 0
    if not path.exists():
        return total
    for child in path.rglob("*"):
        if child.is_file():
            total += child.stat().st_size
    return total


def _truncate_sqlite_wal(storage_dir: Path) -> None:
    """Force a WAL TRUNCATE checkpoint on any SQLite databases in the dir.

    SQLite WAL files aren't deleted when the last connection closes; they stay
    at their full size even after all frames are checkpointed. A TRUNCATE
    checkpoint rewrites the WAL to zero length, giving a fair storage comparison.
    Requires the `sqlite3` CLI to be on PATH; silently skips if unavailable.
    """
    try:
        for db_path in storage_dir.rglob("*.sqlite3"):
            subprocess.run(
                ["sqlite3", str(db_path), "PRAGMA wal_checkpoint(TRUNCATE);"],
                capture_output=True,
                timeout=10,
                check=False,
            )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass  # sqlite3 CLI not available — leave WAL as-is


def summarize_latencies(values: list[float]) -> dict[str, float]:
    return {
        "count": len(values),
        "mean_ms": round(statistics.fmean(values), 2) if values else 0.0,
        "p50_ms": round(percentile(values, 0.50), 2),
        "p95_ms": round(percentile(values, 0.95), 2),
        "max_ms": round(max(values), 2) if values else 0.0,
    }


def measure_call(fn) -> tuple[Any, float]:
    started = time.perf_counter()
    result = fn()
    elapsed_ms = (time.perf_counter() - started) * 1000
    return result, elapsed_ms


def normalize_status(payload: dict[str, Any]) -> dict[str, Any]:
    return {
        "total_drawers": payload.get("total_drawers"),
        "wings_count": len(payload.get("wings", {})),
    }


def extract_search_hit(payload: dict[str, Any], needle: str) -> bool:
    text = json.dumps(payload, sort_keys=True).lower()
    return needle.lower() in text


def benchmark_backend(
    name: str,
    client: JsonRpcClient,
    tool_names: dict[str, str],
    documents: list[Document],
    query_count: int,
    storage_path: Path,
    runs: int,
) -> dict[str, Any]:
    startup_samples: list[float] = []
    warmup_samples: list[float] = []
    add_samples: list[float] = []
    search_samples: list[float] = []
    delete_samples: list[float] = []
    status_samples: list[float] = []
    wings_samples: list[float] = []
    taxonomy_samples: list[float] = []
    search_hit_count = 0
    total_searches = 0

    for run_index in range(runs):
        if storage_path.exists():
            shutil.rmtree(storage_path)
        storage_path.mkdir(parents=True, exist_ok=True)

        warmup_tool = tool_names.get("status") if name == "ironmem" else None
        startup_ms = client.start(warmup_tool=warmup_tool)
        startup_samples.append(startup_ms)
        if hasattr(client, "warmup_ms"):
            warmup_samples.append(client.warmup_ms)

        created_ids: list[str] = []
        for doc in documents:
            payload, elapsed_ms = measure_call(
                lambda d=doc: client.call_tool(
                    tool_names["add_drawer"],
                    {"wing": d.wing, "room": d.room, "content": d.content},
                )
            )
            add_samples.append(elapsed_ms)
            drawer_id = payload.get("drawer_id") or payload.get("id")
            if drawer_id:
                created_ids.append(drawer_id)

        _, elapsed_ms = measure_call(lambda: client.call_tool(tool_names["status"], {}))
        status_samples.append(elapsed_ms)

        _, elapsed_ms = measure_call(lambda: client.call_tool(tool_names["list_wings"], {}))
        wings_samples.append(elapsed_ms)

        _, elapsed_ms = measure_call(lambda: client.call_tool(tool_names["taxonomy"], {}))
        taxonomy_samples.append(elapsed_ms)

        for index in range(min(query_count, len(documents))):
            needle = documents[index].needle
            payload, elapsed_ms = measure_call(
                lambda n=needle: client.call_tool(
                    tool_names["search"],
                    {"query": n, "limit": 5},
                )
            )
            search_samples.append(elapsed_ms)
            total_searches += 1
            if extract_search_hit(payload, needle):
                search_hit_count += 1

        for drawer_id in created_ids[-10:]:
            _, elapsed_ms = measure_call(
                lambda i=drawer_id: client.call_tool(tool_names["delete_drawer"], {"id": i})
                if name == "ironmem"
                else client.call_tool(tool_names["delete_drawer"], {"drawer_id": i})
            )
            delete_samples.append(elapsed_ms)

        # Stop server BEFORE measuring storage so SQLite WAL is checkpointed.
        client.stop()
        # SQLite WAL files aren't truncated on last connection close; force a
        # TRUNCATE checkpoint so the storage measurement reflects actual data size.
        if name == "ironmem":
            _truncate_sqlite_wal(storage_path)
        final_storage_bytes = dir_size_bytes(storage_path)

    return {
        "startup": summarize_latencies(startup_samples),
        "warmup": summarize_latencies(warmup_samples) if warmup_samples else None,
        "add_drawer": summarize_latencies(add_samples),
        "search": summarize_latencies(search_samples),
        "delete_drawer": summarize_latencies(delete_samples),
        "status": summarize_latencies(status_samples),
        "list_wings": summarize_latencies(wings_samples),
        "taxonomy": summarize_latencies(taxonomy_samples),
        "search_hit_rate": round(search_hit_count / max(total_searches, 1), 3),
        "storage_bytes": final_storage_bytes,
        "documents": len(documents),
        "queries": min(query_count, len(documents)) * runs,
    }


def make_client(args, storage_root: Path) -> JsonRpcClient:
    binary = Path(args.ironmem_binary).expanduser().resolve()
    if not binary.exists():
        raise SystemExit(
            f"ironmem binary not found at {binary}. Run `cargo build -p ironmem --bin ironmem` first."
        )

    env = os.environ.copy()
    env["IRONMEM_DB_PATH"] = str(storage_root / "ironmem.sqlite3")
    env["IRONMEM_MCP_MODE"] = "trusted"
    # Disable auto-migration and workspace mining — benchmark measures raw search/add perf,
    # not one-time bootstrap cost. Background thread still loads the embedder (real warmup).
    env["IRONMEM_AUTO_BOOTSTRAP"] = "0"
    env["IRONMEM_DISABLE_MIGRATION"] = "1"
    if args.ironmem_model_dir:
        env["IRONMEM_MODEL_DIR"] = str(Path(args.ironmem_model_dir).expanduser().resolve())

    setup = subprocess.run(
        [str(binary), "setup"],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        check=False,
        text=True,
    )
    if setup.returncode != 0:
        raise SystemExit(
            f"ironmem setup failed (stderr: {setup.stderr.strip()!r}). "
            "Ensure the embedding model is available."
        )

    return JsonRpcClient(
        name="ironmem",
        cmd=[str(binary), "serve", "--db", str(storage_root / "ironmem.sqlite3")],
        cwd=Path(args.ironmem_repo).expanduser().resolve(),
        env=env,
        log_stderr=getattr(args, "debug_stderr", False),
    )


def make_mempalace_client(args, storage_root: Path) -> JsonRpcClient:
    repo = Path(args.mempalace_repo).expanduser().resolve()
    if not repo.exists():
        raise SystemExit(f"mempalace repo not found at {repo}")

    env = os.environ.copy()
    env["PYTHONPATH"] = str(repo) + os.pathsep + env.get("PYTHONPATH", "")
    env["MEMPALACE_PALACE_PATH"] = str(storage_root / "mempalace")
    Path(env["MEMPALACE_PALACE_PATH"]).mkdir(parents=True, exist_ok=True)

    return JsonRpcClient(
        name="mempalace",
        cmd=[args.mempalace_python, "-m", "mempalace.mcp_server", "--palace", env["MEMPALACE_PALACE_PATH"]],
        cwd=repo,
        env=env,
    )


def format_bytes(n: int) -> str:
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024:
            return f"{n:.1f} {unit}"
        n /= 1024
    return f"{n:.1f} TB"


def print_summary(results: dict[str, Any]) -> None:
    cfg = results.get("config", {})
    print()
    print("Benchmark Summary")
    print("=================")
    print(f"documents: {cfg.get('documents')}  queries: {cfg.get('queries')}  runs: {cfg.get('runs')}  seed: {cfg.get('seed')}")
    for backend_name, metrics in results["backends"].items():
        print()
        print(backend_name)
        print("-" * len(backend_name))
        print(f"startup p50:    {metrics['startup']['p50_ms']} ms  (connect only)")
        if metrics.get("warmup"):
            print(f"ready p50:      {metrics['warmup']['p50_ms']} ms  (model load + bootstrap)")
        print(f"add p50:        {metrics['add_drawer']['p50_ms']} ms  (p95: {metrics['add_drawer']['p95_ms']} ms)")
        print(f"search p50:     {metrics['search']['p50_ms']} ms  (p95: {metrics['search']['p95_ms']} ms)")
        print(f"status p50:     {metrics['status']['p50_ms']} ms")
        print(f"taxonomy p50:   {metrics['taxonomy']['p50_ms']} ms")
        print(f"delete p50:     {metrics['delete_drawer']['p50_ms']} ms")
        print(f"search hit rate:{metrics['search_hit_rate']}")
        print(f"storage:        {format_bytes(metrics['storage_bytes'])}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Benchmark ironmem vs mempalace over common MCP tool calls.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=textwrap.dedent(
            """\
            Example:
              python3 scripts/benchmark_vs_mempalace.py \
                --documents 100 \
                --queries 20 \
                --runs 2 \
                --output-json /tmp/ironmem-vs-mempalace.json
            """
        ),
    )
    parser.add_argument("--ironmem-repo", default=".", help="Path to the ironmem repo")
    parser.add_argument(
        "--ironmem-binary",
        default="./target/debug/ironmem",
        help="Path to the built ironmem binary",
    )
    parser.add_argument(
        "--ironmem-model-dir",
        default=None,
        help="Optional model directory for ironmem",
    )
    parser.add_argument(
        "--mempalace-repo",
        default="~/git-repos/mempalace",
        help="Path to the mempalace repo",
    )
    parser.add_argument(
        "--mempalace-python",
        default=sys.executable,
        help="Python interpreter to use for mempalace",
    )
    parser.add_argument("--documents", type=int, default=100, help="Number of documents to ingest")
    parser.add_argument("--queries", type=int, default=20, help="Number of searches per run")
    parser.add_argument("--runs", type=int, default=1, help="Number of fresh runs per backend")
    parser.add_argument("--seed", type=int, default=42, help="Dataset seed")
    parser.add_argument(
        "--output-json",
        default=None,
        help="Optional path to write machine-readable results",
    )
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        help="Keep the temporary benchmark workspace instead of deleting it",
    )
    parser.add_argument(
        "--debug-stderr",
        action="store_true",
        help="Redirect server stderr to /tmp log files for debugging",
    )
    parser.add_argument(
        "--ironmem-only",
        action="store_true",
        help="Skip mempalace benchmark (ironmem only)",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    documents = generate_documents(args.documents, args.seed)
    temp_dir = Path(tempfile.mkdtemp(prefix="ironmem-bench-"))

    iron_storage = temp_dir / "ironmem-store"
    mempal_storage = temp_dir / "mempalace-store"

    iron_client = make_client(args, iron_storage)
    mempal_client = None if args.ironmem_only else make_mempalace_client(args, mempal_storage)

    results = {
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "config": {
            "documents": args.documents,
            "queries": args.queries,
            "runs": args.runs,
            "seed": args.seed,
        },
        "backends": {},
    }

    try:
        results["backends"]["ironmem"] = benchmark_backend(
            name="ironmem",
            client=iron_client,
            tool_names={
                "status": "status",
                "list_wings": "list_wings",
                "taxonomy": "get_taxonomy",
                "search": "search",
                "add_drawer": "add_drawer",
                "delete_drawer": "delete_drawer",
            },
            documents=documents,
            query_count=args.queries,
            storage_path=iron_storage,
            runs=args.runs,
        )

        if mempal_client is not None:
            results["backends"]["mempalace"] = benchmark_backend(
                name="mempalace",
                client=mempal_client,
                tool_names={
                    "status": "mempalace_status",
                    "list_wings": "mempalace_list_wings",
                    "taxonomy": "mempalace_get_taxonomy",
                    "search": "mempalace_search",
                    "add_drawer": "mempalace_add_drawer",
                    "delete_drawer": "mempalace_delete_drawer",
                },
                documents=documents,
                query_count=args.queries,
                storage_path=mempal_storage,
                runs=args.runs,
            )
    finally:
        iron_client.stop()
        if mempal_client is not None:
            mempal_client.stop()
        if not args.keep_temp:
            shutil.rmtree(temp_dir, ignore_errors=True)

    if args.output_json:
        output_path = Path(args.output_json).expanduser().resolve()
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")

    print_summary(results)
    if args.output_json:
        print()
        print(f"Wrote JSON results to {Path(args.output_json).expanduser().resolve()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

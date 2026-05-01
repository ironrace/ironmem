#!/usr/bin/env python3
"""Recall quality benchmark for ironmem (with optional mempalace comparison).

Measures whether the right document is retrieved — not just how fast search is.
Uses planted needles: unique, semantically identifiable phrases injected into a
synthetic corpus. Each needle has a paraphrased query so the test exercises
semantic similarity, not keyword matching.

Metrics:
  Recall@1    needle appears in top-1 result
  Recall@5    needle appears in top-5 results
  Recall@10   needle appears in top-10 results
  MRR         mean reciprocal rank (1/rank of first hit, 0 if not found in top-10)
  p50/p95     search latency at each scale

Ingestion strategy:
  <=10k docs  MCP add_drawer (fair apples-to-apples with mempalace)
  >10k docs   write synthetic files + `ironmem mine` (MCP ingestion would take hours)

Usage:
  python3 scripts/benchmark_recall.py --scale 1000
  python3 scripts/benchmark_recall.py --scale 10000 100000
  python3 scripts/benchmark_recall.py --scale 1000 --compare-mempalace
  python3 scripts/benchmark_recall.py --scale all
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
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

# ── Needle bank ──────────────────────────────────────────────────────────────
# 50 unique, highly specific technical facts. Queries are paraphrased to
# require semantic understanding rather than keyword matching.

NEEDLE_BANK: list[tuple[str, str]] = [
    # (needle_text, query)
    ("PostgreSQL autovacuum threshold set to 50 percent for table users", "database maintenance scheduling for user table bloat"),
    ("Redis cluster failover timeout configured at 30 seconds with sentinel", "cache high-availability failover timing"),
    ("Kubernetes HPA targets 70 percent CPU utilization across all pods", "container orchestration autoscaling policy"),
    ("JWT tokens rotate every 15 minutes with a sliding expiry window", "auth token refresh strategy and expiry policy"),
    ("Elasticsearch uses 5 primary shards with 1 replica per index", "search index replication and sharding configuration"),
    ("S3 lifecycle rule moves objects to Glacier after 90 days", "object storage cost optimization via tiering"),
    ("gRPC keepalive ping interval is 20 seconds with 10-second timeout", "RPC connection health check and keepalive settings"),
    ("Nginx upstream zone allocated 64k shared memory for peer state", "reverse proxy load balancing shared state allocation"),
    ("Kafka consumer group commits offsets every 5000 milliseconds", "message queue consumer checkpoint frequency"),
    ("Prometheus scrape interval configured to 15 seconds globally", "metrics collection frequency for observability"),
    ("CircleCI pipeline runs on medium-plus resource class with 4 CPUs", "CI build environment resource allocation"),
    ("Terraform state backend uses S3 with DynamoDB locking enabled", "infrastructure state management and concurrency control"),
    ("Datadog APM sampling rate set to 10 percent in production", "application performance monitoring trace sampling"),
    ("Celery worker concurrency set to 8 with prefetch multiplier 1", "task queue worker parallelism and prefetch tuning"),
    ("OpenTelemetry exports spans to Jaeger on port 14268 via HTTP", "distributed tracing collector endpoint configuration"),
    ("Istio circuit breaker trips after 5 consecutive 5xx responses", "service mesh fault tolerance and circuit breaking"),
    ("PostgreSQL connection pool max size is 100 with idle timeout 600s", "database connection pooling and resource limits"),
    ("CloudFront CDN cache TTL set to 86400 seconds for static assets", "content delivery edge caching duration"),
    ("GitHub Actions workflow triggers on push to main and pull_request", "CI pipeline trigger configuration for version control"),
    ("Vault secret lease duration is 1 hour with 30-minute renewal window", "secrets management lease and renewal policy"),
    ("Stripe webhook signature verified using HMAC-SHA256 with raw body", "payment provider webhook authenticity validation"),
    ("GraphQL query depth limited to 7 levels to prevent nested abuse", "API query complexity and depth restriction"),
    ("RabbitMQ dead-letter queue bound to exchange with 5-minute TTL", "message broker error handling and retry configuration"),
    ("Helm chart deploys 3 replicas with pod disruption budget of 1", "Kubernetes deployment reliability and update safety"),
    ("Sentry error sampling rate is 0.1 in production environment", "error tracking rate limiting and noise reduction"),
    ("Alembic migration uses batch mode for SQLite column alterations", "schema migration tooling for SQLite compatibility"),
    ("Gunicorn workers computed as 2 times CPU count plus 1", "WSGI server worker sizing formula"),
    ("Tailscale ACL grants engineers SSH access to staging subnet only", "zero-trust network policy for developer access"),
    ("Cloudflare WAF rule blocks requests with more than 100 headers", "web application firewall header count protection"),
    ("Pulumi stack uses Python 3.11 runtime on AWS Lambda arm64", "serverless function runtime and architecture selection"),
    ("pg_bouncer runs in transaction pooling mode with pool size 50", "connection pooler mode selection and capacity"),
    ("OTEL SDK uses batch span processor with max export batch 512", "telemetry SDK export batching configuration"),
    ("Argo Workflows retries failed steps up to 3 times with 60s delay", "workflow orchestration retry and backoff policy"),
    ("Traefik entrypoint listens on port 8443 for TLS termination", "edge router TLS configuration and port binding"),
    ("Pydantic v2 model uses model_config with strict mode enabled", "data validation library configuration for type safety"),
    ("Loki log retention is 30 days with chunk target size 1536KB", "log aggregation storage and chunk configuration"),
    ("ArgoCD sync policy auto-prunes orphaned resources on every sync", "GitOps sync behavior for resource lifecycle"),
    ("Temporal workflow history size limit set to 50000 events", "workflow engine event history size constraint"),
    ("Honeycomb dataset receives traces sampled at 20 percent via collector", "observability pipeline sampling before dataset ingestion"),
    ("Keycloak realm token lifespan is 300 seconds for access tokens", "identity provider token duration configuration"),
    ("dbt project uses incremental strategy merge with unique key order_id", "analytics transformation incremental load strategy"),
    ("Snowflake warehouse auto-suspends after 5 minutes of inactivity", "cloud data warehouse cost control and suspend policy"),
    ("FastAPI depends on Pydantic validation before route handler runs", "web framework request validation middleware ordering"),
    ("Fargate task CPU set to 512 units with 1024MB memory allocation", "serverless container resource sizing"),
    ("Consul service mesh uses mTLS with 24-hour certificate rotation", "service discovery mutual TLS and certificate lifecycle"),
    ("Flink checkpoint interval is 60 seconds stored in S3 backend", "stream processing fault tolerance checkpoint configuration"),
    ("Clickhouse MergeTree engine uses 3 days as merge TTL policy", "columnar database storage engine merge scheduling"),
    ("OpenSearch index template applies 2 replicas for prod indices", "search engine index replication defaults"),
    ("Buildkite pipeline uses agent queue tagged gpu for ML training steps", "CI agent routing by capability tag for specialized jobs"),
    ("Supabase edge function timeout is 2 seconds for synchronous calls", "edge compute execution timeout limit"),
]

# Background document vocabulary (semantically distinct from needles)
_BACKGROUND_TOPICS = [
    "sprint planning retrospective and velocity tracking",
    "design system component library documentation",
    "onboarding checklist for new engineers",
    "quarterly OKR review and goal alignment",
    "accessibility audit results for screen reader support",
    "dependency upgrade policy and security patch schedule",
    "incident post-mortem template and blameless culture",
    "data privacy impact assessment for GDPR compliance",
    "API versioning strategy and deprecation timeline",
    "mobile app beta testing feedback collection process",
    "internal knowledge base article quality guidelines",
    "vendor evaluation scorecard for SaaS tools",
    "code review etiquette and turnaround expectations",
    "load testing plan for Black Friday traffic spike",
    "open source contribution policy for employees",
    "disaster recovery runbook for primary database failure",
    "feature flag lifecycle and cleanup process",
    "data retention policy for analytics events",
    "UX research study recruitment and consent process",
    "cost allocation tagging strategy for cloud resources",
]


# ── Data structures ───────────────────────────────────────────────────────────

@dataclass
class NeedleDoc:
    needle_id: int
    needle_text: str
    query: str
    wing: str
    room: str
    content: str  # needle embedded in surrounding context


@dataclass
class ScaleResult:
    scale: int
    backend: str
    ingestion_method: str  # "mcp" or "mine"
    recall_at_1: float
    recall_at_5: float
    recall_at_10: float
    mrr: float
    search_p50_ms: float
    search_p95_ms: float
    needles_tested: int
    ingestion_total_ms: float
    ef_search: int | None = None


# ── Corpus generation ─────────────────────────────────────────────────────────

_WINGS = ["backend_api", "webapp", "mobile_app", "data_pipeline", "devops", "auth_service", "docs_site"]
_ROOMS = ["database", "auth", "deployment", "api", "cache", "tests", "monitoring", "config"]


def _make_needle_docs(n_needles: int, seed: int) -> list[NeedleDoc]:
    """Generate needle documents — each is a unique, queryable fact."""
    rng = random.Random(seed)
    docs = []
    chosen = NEEDLE_BANK[:n_needles] if n_needles <= len(NEEDLE_BANK) else NEEDLE_BANK * (n_needles // len(NEEDLE_BANK) + 1)
    chosen = chosen[:n_needles]
    for i, (needle_text, query) in enumerate(chosen):
        wing = _WINGS[i % len(_WINGS)]
        room = _ROOMS[i % len(_ROOMS)]
        # Embed the needle in brief context so it looks like real memory
        prefix = rng.choice([
            "Engineering decision recorded: ",
            "Configuration note: ",
            "Architecture decision: ",
            "Ops runbook entry: ",
            "Infrastructure note: ",
        ])
        suffix = rng.choice([
            " Reviewed by the platform team.",
            " Documented after incident review.",
            " Approved in RFC process.",
            " Confirmed in production deploy.",
            " Validated during load test.",
        ])
        content = f"NEEDLE_{i:04d}: {prefix}{needle_text}.{suffix}"
        docs.append(NeedleDoc(
            needle_id=i,
            needle_text=needle_text,
            query=query,
            wing=wing,
            room=room,
            content=content,
        ))
    return docs


def _make_background_docs(count: int, seed: int) -> list[tuple[str, str, str]]:
    """Return (wing, room, content) tuples for background noise documents."""
    rng = random.Random(seed + 1000)
    docs = []
    for i in range(count):
        wing = _WINGS[i % len(_WINGS)]
        room = _ROOMS[i % len(_ROOMS)]
        topic = _BACKGROUND_TOPICS[i % len(_BACKGROUND_TOPICS)]
        filler = rng.choice([
            "This was discussed in the team meeting.",
            "Referenced in the engineering wiki.",
            "Part of the quarterly planning process.",
            "Tracked in the project management tool.",
            "Documented by the team lead.",
        ])
        content = f"Background doc {i}: {topic}. {filler}"
        docs.append((wing, room, content))
    return docs


# ── MCP JSON-RPC client ───────────────────────────────────────────────────────

class McpClient:
    """Minimal JSON-RPC client for a subprocess MCP server."""

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
            # Poll status until warming_up is False (model loaded)
            deadline = time.monotonic() + 120.0
            while time.monotonic() < deadline:
                try:
                    result = self.call_tool("status", {})
                    if not result.get("warming_up", False):
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
            raise RuntimeError(f"{self.name}: server closed stdout unexpectedly")
        return json.loads(line)

    def call_tool(self, name: str, arguments: dict) -> dict:
        resp = self._call("tools/call", {"name": name, "arguments": arguments})
        if "error" in resp:
            raise RuntimeError(f"{self.name}: tool error: {resp['error']}")
        content = resp.get("result", {}).get("content", [])
        if content and content[0].get("type") == "text":
            try:
                return json.loads(content[0]["text"])
            except json.JSONDecodeError:
                return {"raw": content[0]["text"]}
        return {}


# ── Ingestion ─────────────────────────────────────────────────────────────────

def _ingest_via_mcp(
    client: McpClient,
    needle_docs: list[NeedleDoc],
    background_docs: list[tuple[str, str, str]],
) -> float:
    """Add all documents through MCP add_drawer. Returns total ingestion ms."""
    t0 = time.perf_counter()
    for nd in needle_docs:
        client.call_tool("add_drawer", {
            "content": nd.content,
            "wing": nd.wing,
            "room": nd.room,
        })
    for wing, room, content in background_docs:
        client.call_tool("add_drawer", {
            "content": content,
            "wing": wing,
            "room": room,
        })
    return (time.perf_counter() - t0) * 1000


def _ingest_via_mine(
    binary: str,
    db_path: Path,
    needle_docs: list[NeedleDoc],
    background_docs: list[tuple[str, str, str]],
    model_dir: str | None,
) -> float:
    """Write synthetic files and run `ironmem mine`. Returns total ms."""
    docs_dir = db_path.parent / "corpus"
    docs_dir.mkdir(parents=True, exist_ok=True)

    # Write needles as individual files so they land in their own drawers
    for nd in needle_docs:
        p = docs_dir / f"needle_{nd.needle_id:04d}.txt"
        p.write_text(nd.content)

    # Each background doc gets its own file so each becomes exactly one drawer.
    # At 100k+ scale this produces many files but keeps drawer count = scale,
    # which is what we want for accurate recall-at-scale measurement.
    for i, (_, _, content) in enumerate(background_docs):
        p = docs_dir / f"bg_{i:08d}.txt"
        p.write_text(content)

    env = {
        "IRONMEM_DB_PATH": str(db_path),
        "IRONMEM_EMBED_MODE": "real",
        "IRONMEM_MCP_MODE": "trusted",
        "IRONMEM_AUTO_BOOTSTRAP": "0",
    }
    if model_dir:
        env["IRONMEM_MODEL_DIR"] = model_dir
    # ef_search only affects search, not mine — no need to set it here

    t0 = time.perf_counter()
    result = subprocess.run(
        [binary, "mine", str(docs_dir)],
        env={**os.environ, **env},
        capture_output=True,
        text=True,
    )
    elapsed = (time.perf_counter() - t0) * 1000

    if result.returncode != 0:
        raise RuntimeError(f"ironmem mine failed:\n{result.stderr}")
    return elapsed


# ── Recall measurement ────────────────────────────────────────────────────────

def _check_hit(results: list[dict], needle_id: int, k: int) -> bool:
    tag = f"NEEDLE_{needle_id:04d}:"
    for r in results[:k]:
        if tag in r.get("content", ""):
            return True
    return False


def _reciprocal_rank(results: list[dict], needle_id: int) -> float:
    tag = f"NEEDLE_{needle_id:04d}:"
    for rank, r in enumerate(results[:10], start=1):
        if tag in r.get("content", ""):
            return 1.0 / rank
    return 0.0


def _run_recall_queries(
    client: McpClient,
    needle_docs: list[NeedleDoc],
    limit: int = 10,
) -> tuple[float, float, float, float, list[float]]:
    """Return recall@1, @5, @10, MRR, and per-query latency list."""
    hits_1 = hits_5 = hits_10 = 0
    rr_sum = 0.0
    latencies: list[float] = []

    for nd in needle_docs:
        t0 = time.perf_counter()
        payload = client.call_tool("search", {"query": nd.query, "limit": limit})
        elapsed_ms = (time.perf_counter() - t0) * 1000
        latencies.append(elapsed_ms)

        results = payload.get("results", [])
        if _check_hit(results, nd.needle_id, 1):
            hits_1 += 1
        if _check_hit(results, nd.needle_id, 5):
            hits_5 += 1
        if _check_hit(results, nd.needle_id, 10):
            hits_10 += 1
        rr_sum += _reciprocal_rank(results, nd.needle_id)

    n = max(len(needle_docs), 1)
    return (
        hits_1 / n,
        hits_5 / n,
        hits_10 / n,
        rr_sum / n,
        latencies,
    )


def _percentile(values: list[float], p: float) -> float:
    if not values:
        return 0.0
    s = sorted(values)
    rank = (len(s) - 1) * p
    lo, hi = math.floor(rank), math.ceil(rank)
    return s[lo] + (s[hi] - s[lo]) * (rank - lo)


# ── Per-backend runners ───────────────────────────────────────────────────────

def run_ironrace(
    scale: int,
    n_needles: int,
    binary: str,
    model_dir: str | None,
    seed: int,
    ef_search: int | None = None,
    rerank: str = "none",
    shrinkage: str = "on",
) -> ScaleResult:
    needle_docs = _make_needle_docs(n_needles, seed)
    n_background = scale - n_needles
    background_docs = _make_background_docs(n_background, seed)

    use_mine = scale > 10_000

    tmp = Path(tempfile.mkdtemp(prefix="ironmem-recall-"))
    db_path = tmp / "memory.sqlite3"
    model_dir_path = tmp / "noop-model"

    env: dict[str, str] = {
        "IRONMEM_DB_PATH": str(db_path),
        "IRONMEM_EMBED_MODE": "real",
        "IRONMEM_MCP_MODE": "trusted",
        "IRONMEM_AUTO_BOOTSTRAP": "0",
    }
    if model_dir:
        env["IRONMEM_MODEL_DIR"] = model_dir
    if ef_search is not None:
        env["IRONMEM_EF_SEARCH"] = str(ef_search)
    if rerank != "none":
        env["IRONMEM_RERANK"] = rerank
    env["IRONMEM_SHRINKAGE_RERANK"] = "1" if shrinkage == "on" else "0"

    try:
        if use_mine:
            ingestion_ms = _ingest_via_mine(
                binary, db_path, needle_docs, background_docs, model_dir
            )
            ingestion_method = "mine"
        else:
            # Start MCP server, ingest, then query
            client = McpClient(
                name="ironmem",
                cmd=[binary, "serve"],
                env=env,
            )
            client.start(wait_for_embedder=True)
            ingestion_ms = _ingest_via_mcp(client, needle_docs, background_docs)
            r1, r5, r10, mrr, latencies = _run_recall_queries(client, needle_docs)
            client.stop()
            shutil.rmtree(tmp, ignore_errors=True)
            return ScaleResult(
                scale=scale,
                backend="ironmem",
                ingestion_method="mcp",
                recall_at_1=r1,
                recall_at_5=r5,
                recall_at_10=r10,
                mrr=mrr,
                search_p50_ms=_percentile(latencies, 0.5),
                search_p95_ms=_percentile(latencies, 0.95),
                needles_tested=len(needle_docs),
                ingestion_total_ms=ingestion_ms,
                ef_search=ef_search,
            )

        # After mine, start server for queries
        client = McpClient(
            name="ironmem",
            cmd=[binary, "serve"],
            env=env,
        )
        client.start(wait_for_embedder=True)
        r1, r5, r10, mrr, latencies = _run_recall_queries(client, needle_docs)
        client.stop()

    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    return ScaleResult(
        scale=scale,
        backend="ironmem",
        ingestion_method=ingestion_method,
        recall_at_1=r1,
        recall_at_5=r5,
        recall_at_10=r10,
        mrr=mrr,
        search_p50_ms=_percentile(latencies, 0.5),
        search_p95_ms=_percentile(latencies, 0.95),
        needles_tested=len(needle_docs),
        ingestion_total_ms=ingestion_ms,
        ef_search=ef_search,
    )


def run_mempalace(
    scale: int,
    n_needles: int,
    mempalace_python: str,
    seed: int,
) -> ScaleResult:
    """Run the same recall benchmark against mempalace."""
    needle_docs = _make_needle_docs(n_needles, seed)
    n_background = scale - n_needles
    background_docs = _make_background_docs(n_background, seed)

    tmp = Path(tempfile.mkdtemp(prefix="mempalace-recall-"))
    palace_path = str(tmp / "palace")

    env: dict[str, str] = {
        "MEMPALACE_PALACE_PATH": palace_path,
    }

    client = McpClient(
        name="mempalace",
        cmd=[mempalace_python, "-m", "mempalace.mcp_server", "--palace", palace_path],
        env=env,
    )

    try:
        client.start()

        t0 = time.perf_counter()
        for nd in needle_docs:
            client.call_tool("mempalace_add_drawer", {
                "content": nd.content,
                "wing": nd.wing,
                "room": nd.room,
            })
        for wing, room, content in background_docs:
            client.call_tool("mempalace_add_drawer", {
                "content": content,
                "wing": wing,
                "room": room,
            })
        ingestion_ms = (time.perf_counter() - t0) * 1000

        # mempalace uses mempalace_search with the same params
        hits_1 = hits_5 = hits_10 = 0
        rr_sum = 0.0
        latencies: list[float] = []

        for nd in needle_docs:
            t0 = time.perf_counter()
            payload = client.call_tool("mempalace_search", {"query": nd.query, "limit": 10})
            elapsed_ms = (time.perf_counter() - t0) * 1000
            latencies.append(elapsed_ms)

            results = payload.get("results", [])
            tag = f"NEEDLE_{nd.needle_id:04d}:"

            def hit_at(k: int) -> bool:
                return any(tag in r.get("text", r.get("content", "")) for r in results[:k])

            def rr() -> float:
                for rank, r in enumerate(results[:10], start=1):
                    if tag in r.get("text", r.get("content", "")):
                        return 1.0 / rank
                return 0.0

            if hit_at(1):
                hits_1 += 1
            if hit_at(5):
                hits_5 += 1
            if hit_at(10):
                hits_10 += 1
            rr_sum += rr()

    finally:
        client.stop()
        shutil.rmtree(tmp, ignore_errors=True)

    n = max(len(needle_docs), 1)
    return ScaleResult(
        scale=scale,
        backend="mempalace",
        ingestion_method="mcp",
        recall_at_1=hits_1 / n,
        recall_at_5=hits_5 / n,
        recall_at_10=hits_10 / n,
        mrr=rr_sum / n,
        search_p50_ms=_percentile(latencies, 0.5),
        search_p95_ms=_percentile(latencies, 0.95),
        needles_tested=n,
        ingestion_total_ms=ingestion_ms,
    )


# ── Output formatting ─────────────────────────────────────────────────────────

def _fmt_pct(v: float) -> str:
    return f"{v * 100:.1f}%"


def _fmt_ms(v: float) -> str:
    if v >= 1000:
        return f"{v / 1000:.2f}s"
    return f"{v:.1f}ms"


def print_results(results: list[ScaleResult]) -> None:
    print()
    print("Recall Quality Benchmark")
    print("=" * 82)
    print(f"{'Scale':>10}  {'Backend':<16}  {'ef':>6}  {'R@1':>6}  {'R@5':>6}  {'R@10':>6}  {'MRR':>6}  {'p50':>8}  {'p95':>8}")
    print("-" * 82)
    for r in results:
        ef_label = str(r.ef_search) if r.ef_search is not None else "auto"
        print(
            f"{r.scale:>10,}  {r.backend:<16}  {ef_label:>6}  "
            f"{_fmt_pct(r.recall_at_1):>6}  "
            f"{_fmt_pct(r.recall_at_5):>6}  "
            f"{_fmt_pct(r.recall_at_10):>6}  "
            f"{_fmt_pct(r.mrr):>6}  "
            f"{_fmt_ms(r.search_p50_ms):>8}  "
            f"{_fmt_ms(r.search_p95_ms):>8}"
        )
    print()
    print("Needles: unique phrases embedded in synthetic docs; queries are")
    print("paraphrased (semantic match required, not keyword match).")
    print()


# ── CLI ───────────────────────────────────────────────────────────────────────

_SCALE_CHOICES = [100, 1_000, 10_000, 100_000, 1_000_000]

# Number of needle documents per scale — enough to measure recall reliably
# without overwhelming the corpus (needles should be ~1-5% of total)
_NEEDLES_PER_SCALE = {
    100: 10,
    1_000: 20,
    10_000: 30,
    100_000: 40,
    1_000_000: 50,
}


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Recall quality benchmark for ironmem.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Examples:
  python3 scripts/benchmark_recall.py --scale 1000
  python3 scripts/benchmark_recall.py --scale 1000 10000
  python3 scripts/benchmark_recall.py --scale 1000 --compare-mempalace
  python3 scripts/benchmark_recall.py --scale all
""",
    )
    p.add_argument(
        "--scale",
        nargs="+",
        default=["1000"],
        help="Document counts to benchmark. Use 'all' for 100,1000,10000,100000,1000000.",
    )
    p.add_argument(
        "--ironmem-binary",
        default="./target/release/ironmem",
        help="Path to the compiled ironmem binary (default: ./target/release/ironmem)",
    )
    p.add_argument(
        "--ironmem-model-dir",
        default=None,
        help="Optional path to ONNX model directory for ironmem",
    )
    p.add_argument(
        "--rerank",
        choices=["none", "cross_encoder", "llm_haiku"],
        default="none",
        help="Reranker mode (env: IRONMEM_RERANK)",
    )
    p.add_argument(
        "--shrinkage",
        choices=["on", "off"],
        default="on",
        help="Lexical shrinkage rerank (env: IRONMEM_SHRINKAGE_RERANK)",
    )
    p.add_argument(
        "--compare-mempalace",
        action="store_true",
        help="Also benchmark mempalace at scales <=10k (requires Python 3.11)",
    )
    p.add_argument(
        "--mempalace-python",
        default="/opt/homebrew/bin/python3.11",
        help="Python interpreter for mempalace (default: /opt/homebrew/bin/python3.11)",
    )
    p.add_argument(
        "--seed",
        type=int,
        default=42,
        help="RNG seed for deterministic corpus generation (default: 42)",
    )
    p.add_argument(
        "--ef-search",
        nargs="+",
        default=None,
        metavar="N",
        help="ef_search values to sweep (e.g. 100 200 500 1000). Omit to use auto formula.",
    )
    p.add_argument(
        "--output-json",
        default=None,
        help="Write machine-readable results to this path",
    )
    return p.parse_args()


def resolve_scales(raw: list[str]) -> list[int]:
    if len(raw) == 1 and raw[0].lower() == "all":
        return _SCALE_CHOICES
    scales = []
    for s in raw:
        v = int(s.replace(",", "").replace("_", ""))
        if v not in _SCALE_CHOICES:
            print(f"Warning: {v} is not a standard scale ({_SCALE_CHOICES}). Using it anyway.", file=sys.stderr)
        scales.append(v)
    return sorted(set(scales))


def main() -> int:
    args = parse_args()
    scales = resolve_scales(args.scale)

    binary = Path(args.ironmem_binary).expanduser().resolve()
    if not binary.exists():
        print(f"ironmem binary not found: {binary}", file=sys.stderr)
        print("Build it with: cargo build --release -p ironmem --bin ironmem", file=sys.stderr)
        return 1

    ef_values: list[int | None] = [int(x) for x in args.ef_search] if args.ef_search else [None]
    results: list[ScaleResult] = []

    for scale in scales:
        n_needles = _NEEDLES_PER_SCALE.get(scale, min(50, max(10, scale // 50)))

        for ef in ef_values:
            ef_label = f"ef={ef}" if ef is not None else "ef=auto"
            print(f"\nRunning scale={scale:,}  needles={n_needles}  {ef_label}  ...", flush=True)

            t0 = time.perf_counter()
            r = run_ironrace(
                scale=scale,
                n_needles=n_needles,
                binary=str(binary),
                model_dir=args.ironmem_model_dir,
                seed=args.seed,
                ef_search=ef,
                rerank=args.rerank,
                shrinkage=args.shrinkage,
            )
            elapsed = time.perf_counter() - t0
            results.append(r)
            print(f"  ironmem done in {elapsed:.1f}s  R@5={_fmt_pct(r.recall_at_5)}  p50={_fmt_ms(r.search_p50_ms)}", flush=True)

        if args.compare_mempalace and scale <= 10_000:
            t0 = time.perf_counter()
            mp = run_mempalace(
                scale=scale,
                n_needles=n_needles,
                mempalace_python=args.mempalace_python,
                seed=args.seed,
            )
            elapsed = time.perf_counter() - t0
            results.append(mp)
            print(f"  mempalace done in {elapsed:.1f}s  R@5={_fmt_pct(mp.recall_at_5)}  p50={_fmt_ms(mp.search_p50_ms)}", flush=True)

    print_results(results)

    if args.output_json:
        out = {
            "config": {
                "scales": scales,
                "seed": args.seed,
                "ef_search": args.ef_search,
                "rerank": args.rerank,
                "shrinkage": args.shrinkage,
            },
            "results": [
                {
                    "scale": r.scale,
                    "backend": r.backend,
                    "ingestion_method": r.ingestion_method,
                    "recall_at_1": round(r.recall_at_1, 4),
                    "recall_at_5": round(r.recall_at_5, 4),
                    "recall_at_10": round(r.recall_at_10, 4),
                    "mrr": round(r.mrr, 4),
                    "search_p50_ms": round(r.search_p50_ms, 2),
                    "search_p95_ms": round(r.search_p95_ms, 2),
                    "needles_tested": r.needles_tested,
                    "ingestion_total_ms": round(r.ingestion_total_ms, 1),
                }
                for r in results
            ],
        }
        Path(args.output_json).write_text(json.dumps(out, indent=2))
        print(f"Results written to {args.output_json}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())

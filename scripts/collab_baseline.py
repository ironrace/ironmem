#!/usr/bin/env python3
"""Capture and gate a reproducible `/collab` token/MCP baseline.

The live capture path deliberately consumes `ironmem report --json`; it does
not read the SQLite database directly. This keeps the artifact tied to the
public report contract and makes the regression check usable with a committed
report fixture in CI.
"""

from __future__ import annotations

import argparse
import json
import math
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

SCHEMA_VERSION = 1
DEFAULT_THRESHOLD = 0.20
DistributionKey = tuple[str, str | None]


class BaselineError(ValueError):
    """Raised when a report or baseline cannot be safely evaluated."""


def _require_mapping(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise BaselineError(f"{label} must be a JSON object")
    return value


def _require_nonnegative_int(value: Any, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise BaselineError(f"{label} must be a non-negative integer")
    return value


def _require_finite_number(value: Any, label: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise BaselineError(f"{label} must be a finite number")
    number = float(value)
    if not math.isfinite(number) or number < 0:
        raise BaselineError(f"{label} must be a finite non-negative number")
    return number


def _read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise BaselineError(f"cannot read JSON from {path}: {exc}") from exc
    return _require_mapping(value, str(path))


def _validate_threshold(threshold: float) -> float:
    if isinstance(threshold, bool) or not isinstance(threshold, (int, float)):
        raise BaselineError("threshold must be a finite number between 0 and 10")
    if not math.isfinite(threshold) or threshold < 0 or threshold > 10:
        raise BaselineError("threshold must be finite and between 0 and 10")
    return threshold


def _report_task(report: dict[str, Any], session_id: str) -> dict[str, Any]:
    generated_for = _require_mapping(report.get("generated_for"), "generated_for")
    if generated_for.get("task") != session_id:
        raise BaselineError(
            f"report task is {generated_for.get('task')!r}, expected {session_id!r}"
        )
    tasks = report.get("tasks")
    if not isinstance(tasks, list):
        raise BaselineError("report.tasks must be a JSON array")
    for task in tasks:
        if isinstance(task, dict) and task.get("task_key") == session_id:
            outcome = task.get("outcome")
            if outcome is not None:
                _require_mapping(outcome, "task.outcome")
            return task
    raise BaselineError(f"report has no task row for {session_id!r}")


def _validate_distribution(
    raw_item: Any, index: int
) -> tuple[dict[str, Any], DistributionKey]:
    item = _require_mapping(raw_item, f"MCP distribution[{index}]")
    harness = item.get("harness")
    if not isinstance(harness, str) or not harness:
        raise BaselineError(f"MCP distribution[{index}] has an invalid harness")
    tool_name = item.get("tool_name")
    if tool_name is not None and (not isinstance(tool_name, str) or not tool_name):
        raise BaselineError(f"MCP distribution[{index}] has an invalid tool_name")

    item_row_count = _require_nonnegative_int(
        item.get("row_count"), f"MCP distribution[{index}].row_count"
    )
    if item_row_count == 0:
        raise BaselineError(f"MCP distribution[{index}] row_count must be positive")
    _require_nonnegative_int(
        item.get("total_chars"), f"MCP distribution[{index}].total_chars"
    )
    item_total_output = _require_nonnegative_int(
        item.get("total_output_tokens"),
        f"MCP distribution[{index}].total_output_tokens",
    )
    item_mean = _require_finite_number(
        item.get("mean_output_tokens"),
        f"MCP distribution[{index}].mean_output_tokens",
    )
    if not math.isclose(
        item_mean,
        item_total_output / item_row_count,
        rel_tol=1e-9,
        abs_tol=1e-9,
    ):
        raise BaselineError(
            f"MCP distribution[{index}] mean does not match total / row_count"
        )
    _validate_percentiles(item, index)
    return item, (harness, tool_name)


def _display_distribution_key(key: DistributionKey) -> tuple[str, str]:
    harness, tool_name = key
    return harness, tool_name if tool_name is not None else "<protocol>"


def _validate_percentiles(item: dict[str, Any], index: int) -> None:
    p50 = _require_nonnegative_int(
        item.get("p50_output_tokens"),
        f"MCP distribution[{index}].p50_output_tokens",
    )
    p95 = _require_nonnegative_int(
        item.get("p95_output_tokens"),
        f"MCP distribution[{index}].p95_output_tokens",
    )
    maximum = _require_nonnegative_int(
        item.get("max_output_tokens"),
        f"MCP distribution[{index}].max_output_tokens",
    )
    if not p50 <= p95 <= maximum:
        raise BaselineError(
            f"MCP distribution[{index}] percentiles are not ordered p50 <= p95 <= max"
        )


def _mcp_distributions(report: dict[str, Any]) -> list[dict[str, Any]]:
    value_summary = _require_mapping(report.get("value_summary"), "value_summary")
    mcp = value_summary.get("mcp_response")
    if mcp is None:
        raise BaselineError("report contains no MCP response metrics")
    mcp = _require_mapping(mcp, "value_summary.mcp_response")
    distributions = mcp.get("distributions")
    if not isinstance(distributions, list):
        raise BaselineError(
            "report is missing value_summary.mcp_response.distributions; "
            "use a report produced by issue 212 or newer"
        )
    if not distributions:
        raise BaselineError("report contains no MCP response distributions/tools")

    row_count = _require_nonnegative_int(
        mcp.get("row_count"), "value_summary.mcp_response.row_count"
    )
    total_output_tokens = _require_nonnegative_int(
        mcp.get("total_output_tokens"),
        "value_summary.mcp_response.total_output_tokens",
    )
    mean_output_tokens = _require_finite_number(
        mcp.get("mean_output_tokens"),
        "value_summary.mcp_response.mean_output_tokens",
    )
    if row_count == 0:
        raise BaselineError("MCP response row_count must be positive")
    if not math.isclose(
        mean_output_tokens,
        total_output_tokens / row_count,
        rel_tol=1e-9,
        abs_tol=1e-9,
    ):
        raise BaselineError("MCP response mean does not match total / row_count")

    validated = []
    seen: set[DistributionKey] = set()
    for index, raw_item in enumerate(distributions):
        item, key = _validate_distribution(raw_item, index)
        if key in seen:
            harness, tool_name = _display_distribution_key(key)
            raise BaselineError(
                f"report contains duplicate MCP distribution key {harness}/{tool_name}"
            )
        seen.add(key)
        validated.append(item)
    if sum(item["row_count"] for item in validated) != row_count:
        raise BaselineError("MCP distribution row counts do not sum to report row_count")
    if sum(item["total_output_tokens"] for item in validated) != total_output_tokens:
        raise BaselineError(
            "MCP distribution totals do not sum to report total_output_tokens"
        )
    return validated


def _prompt_spec(spec: str) -> tuple[str, Path]:
    phase, separator, raw_path = spec.partition("=")
    if not separator or not phase or not raw_path:
        raise BaselineError(f"prompt must use PHASE=PATH, got {spec!r}")
    path = Path(raw_path)
    if not path.is_file():
        raise BaselineError(f"Codex prompt does not exist: {path}")
    return phase, path


def _prompt_profile(prompt_specs: Iterable[str | tuple[str, Path]]) -> list[dict[str, Any]]:
    profiles = []
    for item in prompt_specs:
        phase, path = item if isinstance(item, tuple) else _prompt_spec(item)
        try:
            chars = len(path.read_text(encoding="utf-8"))
        except OSError as exc:
            raise BaselineError(f"cannot read Codex prompt {path}: {exc}") from exc
        profiles.append(
            {
                "phase": phase,
                "file": path.name,
                "chars": chars,
                "estimated_tokens": math.ceil(chars / 4),
            }
        )
    return profiles


_REVIEW_ARTIFACT_METRIC_KEYS = (
    "source_bytes",
    "artifact_bytes",
    "source_estimated_tokens",
    "artifact_estimated_tokens",
)


def _review_artifact_spec(spec: str) -> tuple[str, Path]:
    phase, separator, raw_path = spec.partition("=")
    if not separator or not phase or not raw_path:
        raise BaselineError(f"review artifact must use PHASE=PATH, got {spec!r}")
    path = Path(raw_path)
    if not path.is_file():
        raise BaselineError(f"review artifact does not exist: {path}")
    return phase, path


def _review_artifact_metric(text: str, key: str) -> int:
    matches = re.findall(rf"(?m)^{re.escape(key)}=(\d+)$", text)
    if len(matches) != 1:
        raise BaselineError(f"review artifact metrics missing or duplicate {key}")
    return int(matches[0])


def _review_artifact_footer(text: str) -> str:
    """Return a verified final footer from the deterministic render contract."""
    title = "review-diff source index\n"
    body_marker = "\nreview-diff compressed body\n"
    footer_marker = "\nreview-diff metrics\n"
    if not text.startswith(title):
        raise BaselineError("review artifact shape is missing its source index title")
    body_start = text.find(body_marker)
    footer_start = text.rfind(footer_marker)
    if body_start < len(title) or footer_start < body_start + len(body_marker):
        raise BaselineError("review artifact shape is missing compressed body or footer")
    return text[footer_start + len(footer_marker):]


def _validate_review_artifact_render(
    text: str, footer: str, metrics: dict[str, int]
) -> None:
    expected_footer = (
        f"source_bytes={metrics['source_bytes']}\n"
        f"artifact_bytes={metrics['artifact_bytes']}\n"
        f"source_estimated_tokens={metrics['source_estimated_tokens']}\n"
        f"artifact_estimated_tokens={metrics['artifact_estimated_tokens']}\n"
    )
    if footer != expected_footer:
        raise BaselineError("review artifact shape has a noncanonical footer")
    actual_bytes = len(text)
    if metrics["artifact_bytes"] != actual_bytes:
        raise BaselineError("review artifact artifact_bytes does not match content")
    if metrics["artifact_estimated_tokens"] != math.ceil(actual_bytes / 4):
        raise BaselineError("review artifact artifact_estimated_tokens does not match content")
    if metrics["source_estimated_tokens"] != math.ceil(metrics["source_bytes"] / 4):
        raise BaselineError("review artifact source_estimated_tokens is inconsistent")


def _review_artifact_profile(
    artifact_specs: Iterable[str | tuple[str, Path]],
) -> list[dict[str, Any]]:
    profiles = []
    for item in artifact_specs:
        phase, path = item if isinstance(item, tuple) else _review_artifact_spec(item)
        if not isinstance(phase, str) or not phase:
            raise BaselineError("review artifact phase must be a non-empty string")
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as exc:
            raise BaselineError(f"cannot read review artifact {path}: {exc}") from exc
        footer = _review_artifact_footer(text)
        metrics = {
            key: _require_nonnegative_int(
                _review_artifact_metric(footer, key), f"review artifact metrics.{key}"
            )
            for key in _REVIEW_ARTIFACT_METRIC_KEYS
        }
        _validate_review_artifact_render(text, footer, metrics)
        if metrics["artifact_bytes"] >= metrics["source_bytes"]:
            raise BaselineError("review artifact metrics must be smaller than source")
        profiles.append({"phase": phase, "file": path.name, **metrics})
    return profiles


def build_baseline(
    report: dict[str, Any],
    *,
    session_id: str,
    prompt_specs: Iterable[str | tuple[str, Path]],
    captured_at: str,
    threshold: float,
    review_artifact_specs: Iterable[str | tuple[str, Path]] = (),
) -> dict[str, Any]:
    threshold = _validate_threshold(threshold)
    task = _report_task(report, session_id)
    distributions = _mcp_distributions(report)
    phase_totals: dict[str, int] = {}
    by_phase = task.get("by_phase", [])
    if not isinstance(by_phase, list):
        raise BaselineError("task.by_phase must be a JSON array")
    for phase in by_phase:
        phase = _require_mapping(phase, "task.by_phase item")
        name = phase.get("phase") or "<none>"
        tokens = phase.get("tokens")
        if isinstance(tokens, bool) or not isinstance(tokens, int) or tokens < 0:
            raise BaselineError(f"invalid token total for phase {name!r}")
        phase_totals[name] = tokens

    return {
        "schema_version": SCHEMA_VERSION,
        "kind": "ironmem-collab-baseline",
        "captured_at": captured_at,
        "reference": {
            "collab_session_id": session_id,
            "report_task": report["generated_for"].get("task"),
            "outcome": (task.get("outcome") or {}).get("outcome"),
        },
        "threshold": {"max_p95_relative_increase": threshold},
        "phase_totals": phase_totals,
        "mcp_response_distributions": distributions,
        "codex_dispatch_prompts": _prompt_profile(prompt_specs),
        "review_diff_artifacts": _review_artifact_profile(review_artifact_specs),
    }


def _baseline_distributions(baseline: dict[str, Any]) -> list[dict[str, Any]]:
    if baseline.get("kind") != "ironmem-collab-baseline":
        raise BaselineError("unsupported baseline kind")
    if baseline.get("schema_version") != SCHEMA_VERSION:
        raise BaselineError("unsupported baseline schema_version")
    distributions = baseline.get("mcp_response_distributions")
    if not isinstance(distributions, list) or not distributions:
        raise BaselineError("baseline has no MCP response distributions")
    validated: list[dict[str, Any]] = []
    seen: set[DistributionKey] = set()
    for index, raw_item in enumerate(distributions):
        item, key = _validate_distribution(raw_item, index)
        if key in seen:
            harness, tool_name = _display_distribution_key(key)
            raise BaselineError(
                f"baseline contains duplicate MCP distribution key {harness}/{tool_name}"
            )
        seen.add(key)
        validated.append(item)
    # Reuse the report validator by wrapping the artifact's distribution list in
    # the same public report shape. This keeps capture and check fail-closed on
    # one schema instead of allowing a hand-edited baseline to bypass checks.
    report = {
        "value_summary": {
            "mcp_response": {
                "row_count": sum(item["row_count"] for item in validated),
                "total_output_tokens": sum(
                    item["total_output_tokens"] for item in validated
                ),
                "mean_output_tokens": 0,
                "distributions": validated,
            }
        }
    }
    row_count = report["value_summary"]["mcp_response"]["row_count"]
    total_output_tokens = report["value_summary"]["mcp_response"][
        "total_output_tokens"
    ]
    report["value_summary"]["mcp_response"]["mean_output_tokens"] = (
        total_output_tokens / row_count if row_count else 0
    )
    return _mcp_distributions(report)


def _distribution_key(item: dict[str, Any]) -> DistributionKey:
    harness = item.get("harness")
    if not isinstance(harness, str) or not harness:
        raise BaselineError("MCP distribution has an invalid harness")
    tool = item.get("tool_name")
    if tool is not None and not isinstance(tool, str):
        raise BaselineError("MCP distribution has an invalid tool_name")
    return harness, tool


def _distribution_map(
    distributions: Iterable[dict[str, Any]], label: str
) -> dict[DistributionKey, dict[str, Any]]:
    result: dict[DistributionKey, dict[str, Any]] = {}
    for item in distributions:
        key = _distribution_key(item)
        if key in result:
            harness, tool_name = _display_distribution_key(key)
            raise BaselineError(
                f"duplicate {label} MCP distribution key {harness}/{tool_name}"
            )
        result[key] = item
    return result


def check_regression(
    baseline: dict[str, Any], report: dict[str, Any], *, threshold: float | None = None
) -> dict[str, Any]:
    baseline_items = _baseline_distributions(baseline)
    current_items = _mcp_distributions(report)
    configured = _require_mapping(baseline.get("threshold"), "baseline.threshold")
    configured_threshold = configured.get("max_p95_relative_increase")
    if isinstance(configured_threshold, bool) or not isinstance(
        configured_threshold, (int, float)
    ):
        raise BaselineError("baseline threshold is missing or invalid")
    threshold = _validate_threshold(
        configured_threshold if threshold is None else threshold
    )

    current_by_key = _distribution_map(current_items, "current")
    regressions = []
    comparisons = []
    for expected in baseline_items:
        key = _distribution_key(expected)
        harness, tool_name = _display_distribution_key(key)
        actual = current_by_key.get(key)
        if actual is None:
            regressions.append(
                {"harness": harness, "tool_name": tool_name, "reason": "missing"}
            )
            continue
        base_p95 = expected.get("p95_output_tokens")
        current_p95 = actual.get("p95_output_tokens")
        if not isinstance(base_p95, (int, float)) or not isinstance(current_p95, (int, float)):
            raise BaselineError(f"invalid p95 values for {harness}/{tool_name}")
        allowed = float(base_p95) * (1 + threshold)
        comparison = {
            "harness": harness,
            "tool_name": tool_name,
            "baseline_p95": base_p95,
            "current_p95": current_p95,
            "allowed_p95": allowed,
        }
        comparisons.append(comparison)
        if current_p95 > allowed:
            regressions.append({**comparison, "reason": "p95_increase"})

    return {
        "passed": not regressions,
        "threshold": threshold,
        "comparisons": comparisons,
        "regressions": regressions,
    }


def _invoke_report(args: argparse.Namespace) -> dict[str, Any]:
    command = [args.ironmem_bin, "report", "--json", "--task", args.session]
    if args.db:
        command.extend(["--db", args.db])
    try:
        completed = subprocess.run(command, check=False, capture_output=True, text=True)
    except OSError as exc:
        raise BaselineError(f"cannot run {' '.join(command)}: {exc}") from exc
    if completed.returncode:
        detail = completed.stderr.strip() or "no stderr"
        raise BaselineError(f"ironmem report failed ({completed.returncode}): {detail}")
    try:
        return _require_mapping(json.loads(completed.stdout), "ironmem report output")
    except json.JSONDecodeError as exc:
        raise BaselineError(f"ironmem report did not return JSON: {exc}") from exc


def _capture(args: argparse.Namespace) -> int:
    report = _read_json(Path(args.report)) if args.report else _invoke_report(args)
    captured_at = args.captured_at or datetime.now(timezone.utc).replace(microsecond=0).isoformat()
    baseline = build_baseline(
        report,
        session_id=args.session,
        prompt_specs=args.codex_prompt,
        review_artifact_specs=args.review_artifact,
        captured_at=captured_at,
        threshold=args.threshold,
    )
    Path(args.output).write_text(json.dumps(baseline, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {args.output}")
    return 0


def _check(args: argparse.Namespace) -> int:
    baseline = _read_json(Path(args.baseline))
    report = _read_json(Path(args.report)) if args.report else _invoke_report(args)
    result = check_regression(baseline, report, threshold=args.threshold)
    print(json.dumps(result, indent=2, sort_keys=True))
    if result["passed"]:
        return 0
    print("collab baseline regression detected", file=sys.stderr)
    return 1


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    capture = subparsers.add_parser("capture", help="capture a report into a baseline artifact")
    capture.add_argument("--session", required=True)
    capture.add_argument("--output", required=True)
    capture.add_argument("--report", help="existing report JSON; omit to invoke ironmem")
    capture.add_argument("--ironmem-bin", default="ironmem")
    capture.add_argument("--db")
    capture.add_argument("--codex-prompt", action="append", default=[], metavar="PHASE=PATH")
    capture.add_argument("--review-artifact", action="append", default=[], metavar="PHASE=PATH")
    capture.add_argument("--captured-at")
    capture.add_argument("--threshold", type=float, default=DEFAULT_THRESHOLD)
    capture.set_defaults(handler=_capture)

    check = subparsers.add_parser("check", help="check a report against a baseline artifact")
    check.add_argument("--baseline", required=True)
    check.add_argument("--report", help="existing report JSON; omit to invoke ironmem")
    check.add_argument("--ironmem-bin", default="ironmem")
    check.add_argument("--db")
    check.add_argument("--session", help="collab session when invoking ironmem")
    check.add_argument("--threshold", type=float)
    check.set_defaults(handler=_check)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    if args.command == "check" and not args.report and not args.session:
        args.parser_error = "check requires --session when --report is omitted"
        print(args.parser_error, file=sys.stderr)
        return 2
    try:
        return args.handler(args)
    except BaselineError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())

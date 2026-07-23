import json
import tempfile
import unittest
from pathlib import Path

from collab_baseline import (
    BaselineError,
    build_baseline,
    check_regression,
    main,
)


def report_fixture(p95: int = 20) -> dict:
    return {
        "generated_for": {"task": "session-1", "since": None},
        "tasks": [
            {
                "task_key": "session-1",
                "by_phase": [
                    {"phase": "planning", "tokens": 120},
                    {"phase": "impl", "tokens": 340},
                ],
            }
        ],
        "value_summary": {
            "mcp_response": {
                "row_count": 3,
                "total_output_tokens": 60,
                "mean_output_tokens": 20.0,
                "distributions": [
                    {
                        "harness": "codex",
                        "tool_name": "collab_status",
                        "row_count": 3,
                        "total_chars": 240,
                        "total_output_tokens": 60,
                        "mean_output_tokens": 20.0,
                        "p50_output_tokens": 20,
                        "p95_output_tokens": p95,
                        "max_output_tokens": p95,
                    }
                ],
            }
        },
    }


class CollabBaselineTests(unittest.TestCase):
    def test_build_baseline_records_phase_totals_and_prompt_sizes(self):
        with tempfile.TemporaryDirectory() as directory:
            prompt = Path(directory) / "collab.md"
            prompt.write_text("abcd" * 5, encoding="utf-8")
            baseline = build_baseline(
                report_fixture(),
                session_id="session-1",
                prompt_specs=[("planning", prompt)],
                captured_at="2026-07-22T12:00:00Z",
                threshold=0.2,
            )

        self.assertEqual(baseline["schema_version"], 1)
        self.assertEqual(baseline["phase_totals"], {"planning": 120, "impl": 340})
        self.assertEqual(baseline["codex_dispatch_prompts"][0]["chars"], 20)
        self.assertEqual(baseline["codex_dispatch_prompts"][0]["estimated_tokens"], 5)
        self.assertEqual(baseline["mcp_response_distributions"][0]["p95_output_tokens"], 20)

    def test_check_passes_at_threshold(self):
        baseline = build_baseline(
            report_fixture(),
            session_id="session-1",
            prompt_specs=[],
            captured_at="2026-07-22T12:00:00Z",
            threshold=0.2,
        )
        result = check_regression(baseline, report_fixture(p95=24))
        self.assertTrue(result["passed"])
        self.assertEqual(result["regressions"], [])

    def test_check_fails_on_p95_regression_and_missing_tool(self):
        baseline = build_baseline(
            report_fixture(),
            session_id="session-1",
            prompt_specs=[],
            captured_at="2026-07-22T12:00:00Z",
            threshold=0.2,
        )
        current = report_fixture(p95=25)
        current["value_summary"]["mcp_response"]["distributions"] = [
            {
                "harness": "codex",
                "tool_name": "different_tool",
                "row_count": 1,
                "total_chars": 4,
                "total_output_tokens": 1,
                "mean_output_tokens": 1.0,
                "p50_output_tokens": 1,
                "p95_output_tokens": 1,
                "max_output_tokens": 1,
            }
        ]
        current["value_summary"]["mcp_response"].update(
            row_count=1, total_output_tokens=1, mean_output_tokens=1.0
        )
        result = check_regression(baseline, current)
        self.assertFalse(result["passed"])
        self.assertEqual(len(result["regressions"]), 1)
        self.assertIn("missing", result["regressions"][0]["reason"])

    def test_build_baseline_rejects_missing_mcp_tools(self):
        report = report_fixture()
        report["value_summary"]["mcp_response"]["distributions"] = []
        with self.assertRaises(BaselineError):
            build_baseline(
                report,
                session_id="session-1",
                prompt_specs=[],
                captured_at="2026-07-22T12:00:00Z",
                threshold=0.2,
            )

    def test_build_baseline_rejects_malformed_distribution_schema(self):
        report = report_fixture()
        del report["value_summary"]["mcp_response"]["distributions"][0]["row_count"]
        with self.assertRaises(BaselineError):
            build_baseline(
                report,
                session_id="session-1",
                prompt_specs=[],
                captured_at="2026-07-22T12:00:00Z",
                threshold=0.2,
            )

    def test_check_rejects_duplicate_distribution_keys(self):
        baseline = build_baseline(
            report_fixture(),
            session_id="session-1",
            prompt_specs=[],
            captured_at="2026-07-22T12:00:00Z",
            threshold=0.2,
        )
        current = report_fixture()
        duplicate = dict(current["value_summary"]["mcp_response"]["distributions"][0])
        current["value_summary"]["mcp_response"]["distributions"].append(duplicate)
        with self.assertRaises(BaselineError):
            check_regression(baseline, current)

    def test_baseline_keeps_protocol_and_named_protocol_groups_distinct(self):
        report = report_fixture()
        report["value_summary"]["mcp_response"]["distributions"][0]["tool_name"] = None
        protocol_named = dict(report["value_summary"]["mcp_response"]["distributions"][0])
        protocol_named["tool_name"] = "<protocol>"
        report["value_summary"]["mcp_response"]["distributions"].append(protocol_named)
        report["value_summary"]["mcp_response"].update(
            row_count=6, total_output_tokens=120, mean_output_tokens=20.0
        )

        baseline = build_baseline(
            report,
            session_id="session-1",
            prompt_specs=[],
            captured_at="2026-07-22T12:00:00Z",
            threshold=0.2,
        )

        self.assertEqual(len(baseline["mcp_response_distributions"]), 2)
        self.assertTrue(check_regression(baseline, report)["passed"])

    def test_check_rejects_a_malformed_baseline_distribution(self):
        baseline = build_baseline(
            report_fixture(),
            session_id="session-1",
            prompt_specs=[],
            captured_at="2026-07-22T12:00:00Z",
            threshold=0.2,
        )
        baseline["mcp_response_distributions"][0]["row_count"] = "3"

        with self.assertRaises(BaselineError):
            check_regression(baseline, report_fixture())

    def test_check_fails_closed_on_invalid_p95(self):
        baseline = build_baseline(
            report_fixture(),
            session_id="session-1",
            prompt_specs=[],
            captured_at="2026-07-22T12:00:00Z",
            threshold=0.2,
        )
        current = report_fixture()
        current["value_summary"]["mcp_response"]["distributions"][0][
            "p95_output_tokens"
        ] = "20"
        with self.assertRaises(BaselineError):
            check_regression(baseline, current)

    def test_check_fails_when_current_p95_exceeds_threshold(self):
        baseline = build_baseline(
            report_fixture(),
            session_id="session-1",
            prompt_specs=[],
            captured_at="2026-07-22T12:00:00Z",
            threshold=0.2,
        )
        result = check_regression(baseline, report_fixture(p95=25))
        self.assertFalse(result["passed"])
        self.assertEqual(result["regressions"][0]["reason"], "p95_increase")

    def test_check_accepts_a_protocol_level_tool_group(self):
        report = report_fixture()
        report["value_summary"]["mcp_response"]["distributions"][0]["tool_name"] = None
        baseline = build_baseline(
            report,
            session_id="session-1",
            prompt_specs=[],
            captured_at="2026-07-22T12:00:00Z",
            threshold=0.2,
        )
        result = check_regression(baseline, report)
        self.assertTrue(result["passed"])
        self.assertEqual(result["comparisons"][0]["tool_name"], "<protocol>")

    def test_check_requires_a_session_without_a_report_file(self):
        self.assertEqual(main(["check", "--baseline", "baseline.json"]), 2)

    def test_capture_cli_writes_a_valid_baseline_from_a_report_file(self):
        with tempfile.TemporaryDirectory() as directory:
            report_path = Path(directory) / "report.json"
            output_path = Path(directory) / "baseline.json"
            report_path.write_text(json.dumps(report_fixture()), encoding="utf-8")

            result = main(
                [
                    "capture",
                    "--session",
                    "session-1",
                    "--report",
                    str(report_path),
                    "--output",
                    str(output_path),
                    "--captured-at",
                    "2026-07-22T12:00:00Z",
                ]
            )

            self.assertEqual(result, 0)
            baseline = json.loads(output_path.read_text(encoding="utf-8"))
        self.assertEqual(baseline["reference"]["collab_session_id"], "session-1")

    def test_build_baseline_rejects_wrong_session(self):
        with self.assertRaises(BaselineError):
            build_baseline(
                report_fixture(),
                session_id="wrong-session",
                prompt_specs=[],
                captured_at="2026-07-22T12:00:00Z",
                threshold=0.2,
            )


if __name__ == "__main__":
    unittest.main()

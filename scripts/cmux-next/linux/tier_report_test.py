#!/usr/bin/env python3
"""Tests for tier-report.py (p15-oracle-151): receipts in, numbers out, never a gate.

Run: python3 -I scripts/cmux-next/linux/tier_report_test.py (stdlib unittest, no Swift).
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("tier_report", HERE / "tier-report.py")
assert _spec and _spec.loader
tier_report = importlib.util.module_from_spec(_spec)
sys.modules["tier_report"] = tier_report  # dataclasses look the module up by name
_spec.loader.exec_module(tier_report)


def tier_receipt(**over: object) -> dict:
    receipt = {
        "schema": "cmux-linux-swift-receipt/1",
        "runner": "swift-tier.sh",
        "suite": "swift-tier",
        "sha": "a" * 40,
        "dirty": False,
        "toolchain": "Swift version 6.2.4 (swift-6.2.4-RELEASE)",
        "sanitize": None,
        "started_at": "2026-10-27T10:00:00Z",
        "wall_seconds": {"sync": 5.0, "resolve": 10.0, "build-all": 420.5, "test": 60.25},
        "targets": {"built": [f"T{i}" for i in range(31)], "failed": [], "blocked": [], "skipped": [], "missing": []},
        "test_targets": {"built": [f"T{i}Tests" for i in range(16)], "failed": [], "blocked": [], "run": [f"T{i}Tests" for i in range(16)], "skipped": []},
        "packages": {"built": [], "failed": [], "skipped": []},
        "tests": {"executed": 1600, "passed": 1598, "failed": 2, "skipped": 4},
        "cases": {
            "T0": {"kind": "source", "build": "pass"},
            "T0Tests": {"kind": "test", "build": "pass", "run": "pass", "tests": 40, "linux_share": 0.05},
        },
        "unexpected": [],
        "incremental": {"target": "CmuxNextTerminalGeometry", "status": "pass", "seconds": 12.3, "budget_seconds": 30.0},
        "errors_queue": {"path": "q.jsonl", "entries": 0},
        "ok": True,
    }
    receipt.update(over)
    return receipt


def shared_receipt(**over: object) -> dict:
    receipt = tier_receipt(
        runner="swift-shared.sh",
        suite="swift-shared",
        wall_seconds={"sync": 4.0, "build-and-test": 300.0},
        targets={"built": [], "failed": [], "blocked": [], "skipped": [], "missing": []},
        packages={"built": [f"P{i}" for i in range(20)], "failed": ["Bad"], "skipped": ["Skip"]},
        tests={"executed": 1234, "passed": 1234, "failed": 0, "skipped": 0},
        cases={"P0": {"kind": "package", "build": "pass", "run": "pass", "tests": 99, "linux_share": 0.0}},
        incremental=None,
        ok=False,
    )
    receipt.update(over)
    return receipt


def only_receipt(**over: object) -> dict:
    """`swift-tier.sh --only CmuxNextTerminalGeometry --measure-incremental CmuxNextTerminalGeometry`: no build-all."""
    receipt = tier_receipt(
        wall_seconds={"sync": 2.0, "resolve": 3.0, "build-targets": 40.0, "test": 5.0},
        targets={"built": ["CmuxNextTerminalGeometry"], "failed": [], "blocked": [], "skipped": [], "missing": []},
        test_targets={"built": ["GeometryTests"], "failed": [], "blocked": [], "run": ["GeometryTests"], "skipped": []},
        tests={"executed": 20, "passed": 20, "failed": 0, "skipped": 0},
        incremental={"target": "CmuxNextTerminalGeometry", "status": "pass", "seconds": 8.5, "budget_seconds": 30.0},
    )
    receipt.update(over)
    return receipt


def checks(*receipts: dict) -> dict[str, dict]:
    """The done checks of the receipts, given oldest first, by check label."""
    rows = [tier_report.row_from_receipt(r, f"r{i}.json") for i, r in enumerate(receipts)]
    (selection,) = tier_report.select(rows)
    return {g["check"]: g for g in tier_report.goal_results(selection)}


BUILD_CHECK = "build-all wall seconds (clean scratch)"
INCREMENTAL_CHECK = "incremental build seconds"


class TierReportTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def write(self, rel: str, data: object) -> Path:
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(data) if not isinstance(data, str) else data, encoding="utf-8")
        return path

    def run_main(self, *argv: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = tier_report.main(list(argv))
        return rc, out.getvalue(), err.getvalue()

    def test_numbers_come_from_the_receipt(self) -> None:
        row = tier_report.row_from_receipt(tier_receipt(), "r.json")
        self.assertEqual((row.built, row.planned, row.test_targets_built, row.tests_executed), (31, 31, 16, 1600))
        self.assertEqual(row.build_seconds, 420.5)
        self.assertEqual(row.test_seconds, 60.25)
        self.assertEqual(row.total_seconds, 495.75)
        self.assertEqual(row.incremental_seconds, 12.3)
        self.assertEqual(row.cases, (("T0Tests", "pass", 40, 0.05),))

    def test_shared_counts_packages(self) -> None:
        row = tier_report.row_from_receipt(shared_receipt(), "s.json")
        self.assertEqual((row.built, row.failed, row.planned, row.tests_executed), (20, 1, 22, 1234))
        self.assertEqual(row.build_seconds, 300.0)
        self.assertIsNone(row.incremental_seconds)

    def test_goals_met_and_not_yet(self) -> None:
        met = {name: g["met"] for name, g in checks(tier_receipt()).items()}
        self.assertTrue(all(met[name] for name in met if name != BUILD_CHECK), met)
        red = tier_receipt(
            targets={"built": ["A"], "failed": ["B"], "blocked": ["C"], "skipped": [], "missing": []},
            incremental={"target": "CmuxNextTerminalGeometry", "status": "fail", "seconds": 3.0},
            wall_seconds={"build-all": 100.0},
        )
        met = {name: g["met"] for name, g in checks(red).items()}
        self.assertFalse(met["CmuxNext source targets built"])
        self.assertFalse(met[BUILD_CHECK], "a red build has no tier build time")
        self.assertFalse(met[INCREMENTAL_CHECK])
        no_build = tier_receipt(wall_seconds={"sync": 1.0}, incremental=None)
        met = {name: g["met"] for name, g in checks(no_build).items()}
        self.assertEqual(set(met.values()), {None}, "a run with no build-all phase holds no tier number")

    def test_build_time_without_clean_scratch_proof_is_never_met(self) -> None:
        # A warm --all rebuild in the kept scratch finishes in seconds; the receipt cannot tell it from a clean one.
        warm = checks(tier_receipt(wall_seconds={"sync": 1.0, "resolve": 1.0, "build-all": 4.2, "test": 60.0}))
        self.assertEqual(warm[BUILD_CHECK]["value"], 4.2)
        self.assertIsNone(warm[BUILD_CHECK]["met"])
        slow = checks(tier_receipt(wall_seconds={"build-all": 700.0}))
        self.assertFalse(slow[BUILD_CHECK]["met"], "a warm build over the limit means a clean one is too")

    def test_a_test_target_that_fails_to_compile_makes_the_build_red(self) -> None:
        receipt = tier_receipt(
            test_targets={"built": ["T0Tests"], "failed": ["T1Tests"], "blocked": ["T2Tests"], "run": ["T0Tests"], "skipped": []},
            wall_seconds={"build-all": 100.0},
        )
        row = tier_report.row_from_receipt(receipt, "r")
        self.assertEqual((row.built, row.failed, row.blocked), (31, 0, 0), "every source target built")
        self.assertEqual((row.test_targets_failed, row.test_targets_blocked), (1, 1))
        self.assertFalse(checks(receipt)[BUILD_CHECK]["met"])
        only_failed = tier_receipt(
            test_targets={"built": [], "failed": ["T1Tests"], "blocked": [], "run": [], "skipped": []},
            wall_seconds={"build-all": 100.0},
        )
        self.assertFalse(checks(only_failed)[BUILD_CHECK]["met"])

    def test_a_newer_only_run_does_not_hide_the_tier_totals(self) -> None:
        full = tier_receipt(started_at="2026-10-27T10:00:00Z", sha="a" * 40, incremental=None)
        partial = only_receipt(started_at="2026-10-27T11:00:00Z", sha="b" * 40)
        result = checks(full, partial)
        self.assertEqual(result["CmuxNext source targets built"]["value"], 31)
        self.assertEqual(result["tests executed"]["value"], 1600)
        self.assertEqual(result[BUILD_CHECK]["value"], 420.5)
        self.assertEqual(result["tests executed"]["receipt"], "r0.json")
        self.assertEqual(result[INCREMENTAL_CHECK]["value"], 8.5)
        self.assertTrue(result[INCREMENTAL_CHECK]["met"])
        self.assertEqual(result[INCREMENTAL_CHECK]["receipt"], "r1.json")
        self.write("a/receipt.json", full)
        self.write("b/receipt.json", partial)
        rc, out, _ = self.run_main(str(self.root), "--history", "0")
        self.assertEqual(rc, 0)
        self.assertIn("| swift-tier | tests executed | 1,600 | >= 1,500 | met | 2026-10-27T10:00:00Z `aaaaaaaaaaaa` |", out)
        self.assertIn(
            "| swift-tier | incremental build seconds | 8.5 | < 30 | met | 2026-10-27T11:00:00Z `bbbbbbbbbbbb` |", out
        )
        self.assertIn("| 31 / 31 | 0 | 0 | - | 16 / 16 (0, 0) | 1,600 (1,598, 2) | 420.5 | 60.2 | - | 0 | 0 | yes | totals |", out)
        self.assertIn("| 1 / 1 | 0 | 0 | - | 1 / 1 (0, 0) | 20 (20, 0) | - | 5.0 | 8.5 s (CmuxNextTerminalGeometry) | 0 | 0 | yes | incremental |", out)

    def test_a_newer_full_run_keeps_the_older_incremental_time(self) -> None:
        partial = only_receipt(started_at="2026-10-27T10:00:00Z")
        full = tier_receipt(started_at="2026-10-27T11:00:00Z", incremental=None, tests={"executed": 1700, "passed": 1700, "failed": 0, "skipped": 0})
        result = checks(partial, full)
        self.assertEqual(result["tests executed"]["value"], 1700)
        self.assertEqual(result["tests executed"]["receipt"], "r1.json")
        self.assertEqual(result[INCREMENTAL_CHECK]["value"], 8.5)
        self.assertEqual(result[INCREMENTAL_CHECK]["receipt"], "r0.json")
        self.assertTrue(result[INCREMENTAL_CHECK]["met"])

    def test_one_full_run_with_incremental_feeds_every_check(self) -> None:
        rows = [tier_report.row_from_receipt(tier_receipt(), "r.json")]
        (selection,) = tier_report.select(rows)
        self.assertEqual([(row.path, used) for row, used in selection.shown()], [("r.json", "totals, incremental")])

    def test_only_subset_runs_hold_no_totals(self) -> None:
        rows = [tier_report.row_from_receipt(only_receipt(incremental=None), "r.json")]
        (selection,) = tier_report.select(rows)
        self.assertIsNone(selection.totals)
        self.assertEqual([(row.path, used) for row, used in selection.shown()], [("r.json", "none (subset run)")])
        self.assertEqual({g["met"] for g in tier_report.goal_results(selection)}, {None})

    def test_setup_failure_is_shown(self) -> None:
        receipt = tier_receipt(
            setup={"failed": ["resolve", "dump-package"], "passed": []},
            targets={"built": [], "failed": [], "blocked": [], "skipped": [], "missing": []},
            test_targets={"built": [], "failed": [], "blocked": [], "run": [], "skipped": []},
            tests={"executed": 0, "passed": 0, "failed": 0, "skipped": 0},
            cases={},
            incremental=None,
            wall_seconds={"sync": 11.2, "resolve": 145.1},
            ok=False,
        )
        self.write("r/receipt.json", receipt)
        rc, out, _ = self.run_main(str(self.root))
        self.assertEqual(rc, 0)
        self.assertIn("| 0 / 0 | 0 | 0 | resolve, dump-package |", out)
        self.assertIn("| swift-tier | build-all wall seconds (clean scratch) | - | < 600 | not yet |", out)
        self.assertIn("| swift-tier | tests executed | 0 | >= 1,500 | not yet | 2026-10-27T10:00:00Z `aaaaaaaaaaaa` |", out)

    def test_sanitized_receipts_skip_time_goals(self) -> None:
        names = list(checks(tier_receipt(sanitize="thread")))
        self.assertNotIn(BUILD_CHECK, names)
        self.assertNotIn(INCREMENTAL_CHECK, names)
        self.assertIn("tests executed", names)

    def test_load_skips_other_json_dedupes_and_orders(self) -> None:
        self.write("a/receipt.json", tier_receipt(started_at="2026-10-27T12:00:00Z"))
        self.write("b/linux-tier.json", tier_receipt(started_at="2026-10-27T12:00:00Z"))  # the same run, copied
        self.write("c/receipt.json", tier_receipt(started_at="2026-10-26T12:00:00Z", sha="b" * 40))
        self.write("d/receipt.json", shared_receipt())
        self.write("e/other.json", {"schema": "something-else"})
        self.write("f/broken.json", "{not json")
        self.write("g/receipt.json", {"schema": "cmux-linux-swift-receipt/1", "suite": "swift-tier"})
        warnings: list[str] = []
        rows = tier_report.load_rows([self.root], warnings)
        self.assertEqual([r.started_at for r in rows], ["2026-10-26T12:00:00Z", "2026-10-27T10:00:00Z", "2026-10-27T12:00:00Z"])
        self.assertEqual(len(warnings), 1)
        self.assertIn("g/receipt.json", warnings[0])
        selected = tier_report.select(rows)
        self.assertEqual(
            [(s.suite, s.totals.started_at) for s in selected],
            [("swift-tier", "2026-10-27T12:00:00Z"), ("swift-shared", "2026-10-27T10:00:00Z")],
        )

    def test_report_exits_zero_when_goals_are_not_met(self) -> None:
        self.write("r/receipt.json", tier_receipt(ok=False, targets={"built": [], "failed": [], "blocked": ["A"], "skipped": [], "missing": []}))
        rc, out, _ = self.run_main(str(self.root), "--cases")
        self.assertEqual(rc, 0)
        self.assertIn("| swift-tier | CmuxNext source targets built | 0 | >= 30 | not yet |", out)
        self.assertIn("T0Tests", out)

    def test_no_receipt_is_an_error(self) -> None:
        rc, _, err = self.run_main(str(self.root))
        self.assertEqual(rc, 2)
        self.assertIn("no cmux-linux-swift-receipt/1 receipt", err)

    def test_json_output(self) -> None:
        self.write("r/receipt.json", tier_receipt())
        rc, out, _ = self.run_main(str(self.root), "--json")
        self.assertEqual(rc, 0)
        report = json.loads(out)
        latest = report["latest"][0]
        self.assertEqual(latest["totals"]["tests_executed"], 1600)
        self.assertEqual(latest["totals"]["cases"][0]["case"], "T0Tests")
        self.assertEqual(latest["incremental"]["path"], latest["totals"]["path"])
        self.assertEqual({c["receipt"] for c in latest["done_checks"]}, {latest["totals"]["path"]})

    def test_write_doc_replaces_only_the_block(self) -> None:
        self.write("r/receipt.json", tier_receipt())
        doc = self.write(
            "doc.md",
            f"# Doc\n\nbefore\n\n{tier_report.DOC_BEGIN}\nold table\n{tier_report.DOC_END}\n\nafter\n",
        )
        rc, _, _ = self.run_main(str(self.root / "r"), "--write-doc", str(doc), "--json")
        self.assertEqual(rc, 0)
        text = doc.read_text(encoding="utf-8")
        self.assertNotIn("old table", text)
        self.assertTrue(text.startswith("# Doc\n\nbefore\n\n" + tier_report.DOC_BEGIN))
        self.assertTrue(text.endswith(tier_report.DOC_END + "\n\nafter\n"))
        self.assertIn("| swift-tier | 2026-10-27T10:00:00Z | `aaaaaaaaaaaa` | 31 / 31 | 0 | 0 | - |", text)
        again = self.run_main(str(self.root / "r"), "--write-doc", str(doc), "--json")
        self.assertEqual(again[0], 0)
        self.assertEqual(doc.read_text(encoding="utf-8"), text, "a second write changes nothing")

    def test_write_doc_without_markers_fails(self) -> None:
        self.write("r/receipt.json", tier_receipt())
        doc = self.write("doc.md", "# Doc\n")
        rc, _, err = self.run_main(str(self.root / "r"), "--write-doc", str(doc))
        self.assertEqual(rc, 2)
        self.assertIn("tier-report.py", err)
        self.assertEqual(doc.read_text(encoding="utf-8"), "# Doc\n")

    def test_design_doc_has_the_block(self) -> None:
        text = tier_report.DESIGN_DOC.read_text(encoding="utf-8")
        self.assertEqual(text.count(tier_report.DOC_BEGIN), 1)
        self.assertEqual(text.count(tier_report.DOC_END), 1)


if __name__ == "__main__":
    unittest.main()

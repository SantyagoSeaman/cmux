#!/usr/bin/env python3
"""Tests for report.py (p15-oracle-116): the first failure of a run, a fleet dir or a comparison names its step,
pointer and the event and checkpoint lines; untested states and artifacts are listed.

Run: python3 -I scripts/cmux-next/oracle/report_test.py (stdlib unittest; no build, no fleet). The run dirs are
written by oracle.perform_run with a scripted driver, merged by oracle.merge_runs, compared by the real diff.py,
and laid out as fleet.py lays out a fleet dir (fleet.fleet_record), so the report reads the real formats.
"""

from __future__ import annotations

import io
import json
import shutil
import tempfile
import threading
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

HERE = Path(__file__).resolve().parent
import sys  # noqa: E402

sys.path.insert(0, str(HERE))

import fleet  # noqa: E402
import oracle  # noqa: E402
import report  # noqa: E402
from oracle_cli_test import ResetDriver  # noqa: E402

SHA = "0123456789abcdef0123456789abcdef01234567"
STEPS = [{"id": "start", "op": "snapshot"}, {"id": "create", "op": "resource.call"}, {"id": "close", "op": "resource.call"}]


def snapshot(value):
    return {"topology": {"panes": 1}, "value": value}


class ScriptDriver:
    """`script[cid]` is the list of results of its successive runs (the last repeats); every other case passes."""

    def __init__(self, script=None, value=lambda cid: "v-" + cid):
        self.script = script or {}
        self.value = value
        self.count = {}
        self.mutex = threading.Lock()  # test-only bookkeeping across the runner's worker threads

    def open_session(self, ctx):
        return self

    def close(self):
        pass

    def run_case(self, case, ctx):
        cid = ctx["case_id"]
        with self.mutex:
            n = self.count.get(cid, 0)
            self.count[cid] = n + 1
        runs = self.script.get(cid) or [{}]
        spec = runs[min(n, len(runs) - 1)]
        checkpoints = [{"step": s["id"], "checkpoint": cp, "snapshot": snapshot(self.value(cid))}
                       for s in STEPS[: spec.get("steps", len(STEPS))] for cp in ("before", "settled")]
        for cp in checkpoints:
            if cp["step"] in spec.get("not_comparable", {}):
                cp["notComparable"] = spec["not_comparable"][cp["step"]]
        return {"status": spec.get("status", "Pass"), "checkpoints": checkpoints, "message": spec.get("message", ""),
                **({"step": spec["step"]} if "step" in spec else {})}


def write_corpus(root: Path, per_suite=6, extra=None) -> Path:
    for suite in ("alpha", "beta"):
        for i in range(per_suite):
            path = root / suite / f"c{i}.json"
            path.parent.mkdir(parents=True, exist_ok=True)
            data = {"id": f"c{i}", "steps": STEPS, **((extra or {}).get(f"{suite}/c{i}") or {})}
            path.write_text(json.dumps(data))
    return root


def options(tmp: Path, **overrides) -> oracle.RunOptions:
    base = dict(
        product="daemon", suites=("all",), filters=(), shard=(0, 1), jobs=4, batch_limit=1, repeat=1,
        retry_limit=0, seed=None, platform="linux", build="debug", switch=None, arm=None, switches={},
        ref=SHA, tag=None, binary=None, corpus=tmp / "corpus", out=tmp / "out", state_base=Path("/tmp/o"),
        run_id="r", expectations_root=tmp / "expectations", isolation="none",
    )
    base.update(overrides)
    return oracle.RunOptions(**base)


def quiet(fn, *args, **kwargs):
    with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
        return fn(*args, **kwargs)


def run(tmp: Path, driver, **overrides):
    cases, errors = oracle.load_corpus(tmp / "corpus")
    assert errors == [], errors
    return oracle.perform_run(options(tmp, **overrides), cases, driver.open_session, None, {})


def make_report(directory: Path, tmp: Path, *extra):
    out = tmp / "report-out"
    code = quiet(report.main, [str(directory), "--out", str(out), "--corpus", str(tmp / "corpus"), *extra])
    receipt = json.loads((out / "receipt.json").read_text()) if (out / "receipt.json").is_file() else None
    index = (out / "index.md").read_text() if (out / "index.md").is_file() else ""
    return code, receipt, index


def line_of(path: Path, predicate) -> int:
    for number, line in enumerate(path.read_text().splitlines(), 1):
        if predicate(json.loads(line)):
            return number
    raise AssertionError(f"no line of {path} matches")


class RunDirTests(unittest.TestCase):
    def test_first_failure_names_step_pointer_and_lines(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            write_corpus(tmp / "corpus")
            driver = ScriptDriver({"beta/c3": [{"status": "Failure", "step": "create", "steps": 2,
                                                "message": "result differs at /value/id: \"a\", want \"b\""}],
                                   "alpha/c5": [{"status": "Timeout", "step": "close",
                                                 "message": "waitFor /topology/panes not met in 500 ms"}]})
            run(tmp, driver)
            code, receipt, index = make_report(tmp / "out", tmp)
            self.assertEqual(code, report.EXIT_FAIL)
            self.assertEqual(receipt["verdict"], "fail")
            self.assertEqual(receipt["counts"], {"unexpected": 2})
            order = json.loads((tmp / "out" / "receipt.json").read_text())["order"]
            first_case = min(("beta/c3", "alpha/c5"), key=order.index)
            first = receipt["first_failure"]
            self.assertEqual(first["case"], first_case)
            failure = next(f for f in receipt["failures"] if f["case"] == "beta/c3")
            self.assertEqual(failure["step"], {"id": "create", "index": 1, "op": "resource.call", "from": "driver"})
            self.assertEqual(failure["pointer"], {"pointer": "/value/id", "from": "message"})
            self.assertEqual(failure["result"], "Failure")
            event_line = line_of(tmp / "out" / "events.jsonl", lambda r: r["case"] == "beta/c3")
            cp_line = line_of(tmp / "out" / "checkpoints.jsonl",
                              lambda r: r["case"] == "beta/c3" and r["step"] == "create" and r["checkpoint"] == "settled")
            self.assertIn({"kind": "event", "path": "events.jsonl", "line": event_line}, failure["artifacts"])
            self.assertIn({"kind": "checkpoint", "path": "checkpoints.jsonl", "line": cp_line}, failure["artifacts"])
            self.assertIn("--filter beta/c3", failure["replay"])
            self.assertTrue(failure["replay"].startswith(f"bash scripts/cmux-next/oracle/linux_run.sh run {SHA} --"))
            timeout = next(f for f in receipt["failures"] if f["case"] == "alpha/c5")
            self.assertEqual(timeout["pointer"]["pointer"], "/topology/panes")
            # The first failure leads index.md: case, step, pointer and the line links come before the run table.
            head = index.split("## Runs")[0]
            self.assertIn(f"case `{first_case}`", head)
            self.assertIn("events.jsonl line", head)
            self.assertIn("checkpoints.jsonl line", head)
            self.assertIn("(../out/events.jsonl)", index)

    def test_crash_without_step_points_at_the_last_checkpoint(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            write_corpus(tmp / "corpus")
            run(tmp, ScriptDriver({"alpha/c1": [{"status": "Crash", "steps": 1,
                                                 "message": "I17: renderer exit 1 not allowed"}]}))
            code, receipt, index = make_report(tmp / "out", tmp)
            failure = receipt["first_failure"]
            self.assertEqual(failure["step"], {"id": "start", "index": 0, "op": "snapshot", "from": "last-checkpoint"})
            self.assertEqual(failure["invariant"], "I17")
            self.assertIsNone(failure["pointer"])
            self.assertIn("in it or after it", index)

    def test_flaky_untested_and_not_comparable_are_listed(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            write_corpus(tmp / "corpus", extra={"alpha/c2": {"untested": ["VoiceOver speech output"]}})
            driver = ScriptDriver({"beta/c0": [{"status": "Failure", "message": "x"}, {}],
                                   "alpha/c4": [{"not_comparable": {"create": ["webviews"]}}]})
            run(tmp, driver, retry_limit=3)
            code, receipt, index = make_report(tmp / "out", tmp)
            self.assertEqual(code, report.EXIT_PASS, receipt["failures"])
            self.assertEqual(receipt["verdict"], "pass")
            self.assertIsNone(receipt["first_failure"])
            untested = receipt["untested"]
            self.assertEqual([c["case"] for c in untested["flaky"]], ["beta/c0"])
            self.assertEqual(untested["case_reasons"], [{"case": "alpha/c2", "reasons": ["VoiceOver speech output"]}])
            self.assertEqual({(c["case"], c["step"], tuple(c["facets"])) for c in untested["not_comparable"]},
                             {("alpha/c4", "create", ("webviews",))})
            self.assertIn("flaky, left out of every comparison: 1", index)
            self.assertIn("VoiceOver speech output", index)
            self.assertIn("None: every case ended as expected.", index)

    def test_dry_run_is_incomplete(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            write_corpus(tmp / "corpus")
            run(tmp, ScriptDriver(), dry_run=True)
            code, receipt, index = make_report(tmp / "out", tmp)
            self.assertEqual(code, report.EXIT_INCOMPLETE)
            self.assertEqual(len(receipt["untested"]["planned"]), 12)
            self.assertIn("no case ran", index)

    def test_leaked_state_names_the_pointer(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            write_corpus(tmp / "corpus")
            run(tmp, ResetDriver(leaks={"alpha/c2"}), product="headless", batch_limit=4, jobs=1)
            code, receipt, _ = make_report(tmp / "out", tmp)
            self.assertEqual(code, report.EXIT_FAIL)
            leak = next(f for f in receipt["failures"] if f["case"] == "alpha/c2")
            self.assertEqual(leak["kind"], "leaked")
            self.assertEqual(leak["pointer"]["from"], "leak")
            self.assertTrue(leak["pointer"]["pointer"].startswith("/windows"), leak["pointer"])
            self.assertIn("left the product unlike its fixture", leak["message"])

    def test_bad_input_and_out_in_checkout_exit_2(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            (tmp / "empty").mkdir()
            self.assertEqual(quiet(report.main, [str(tmp / "empty"), "--out", str(tmp / "r")]), report.EXIT_BAD_INPUT)
            write_corpus(tmp / "corpus")
            run(tmp, ScriptDriver())
            inside = oracle.REPO / "tmp-report-116"
            self.assertEqual(quiet(report.main, [str(tmp / "out"), "--out", str(inside)]), report.EXIT_BAD_INPUT)
            self.assertFalse(inside.exists())

    def test_message_parsing(self):
        self.assertEqual(report.message_pointer("result differs at /: 1, want 2"), "/")
        self.assertEqual(report.message_pointer("x: the reply of s1 has nothing at /value/0/id"), "/value/0/id")
        self.assertEqual(report.message_pointer("I3 pointer /topology/x): turn != settled"), "/topology/x")
        self.assertIsNone(report.message_pointer("shutdown failed: /tmp/o/ab/daemon.log tail"))
        self.assertEqual(report.message_invariant("step failed; I6: orphan socket"), "I6")
        self.assertIsNone(report.message_invariant("expected ok, got I/O error"))


def fleet_dir(tmp: Path, *, sides=("head",), missing=(), head_value=None) -> Path:
    """A fleet.py out dir: shards/<side>/<i>/{step.log, run/}, <side>/ merged, compare/ for two sides."""
    out = tmp / "fleet-run"
    cases, _ = oracle.load_corpus(tmp / "corpus")
    record_sides = []
    for side in sides:
        driver = ScriptDriver({"beta/c3": [{"status": "Failure", "step": "create",
                                            "message": "result differs at /value/id: 1, want 2"}]}
                              if side == "head" and head_value is None else {},
                              value=head_value if side == "head" and head_value else (lambda cid: "v-" + cid))
        record = fleet.Side(side, SHA)
        for i in range(2):
            shard_dir = out / "shards" / side / str(i)
            shard = fleet.Shard(side, i, 2, SHA, f"oracle-r-{side}-s{i}of2", [], shard_dir, step_id=f"step-{side}-{i}",
                                state="succeeded" if (side, i) not in missing else "failed", worker="aws-m4pro-3")
            shard_dir.mkdir(parents=True)
            (shard_dir / "step.log").write_text(f"step {side} {i}\n")
            if (side, i) in missing:
                shard.error = "build failed"
            else:
                shard.run_dir = shard_dir / "run"
                oracle.perform_run(options(tmp, product="app", build="bench", shard=(i, 2), out=shard.run_dir,
                                           run_id=f"{side}{i}"), cases, driver.open_session, None, {})
                (shard.run_dir / "app" / "a1x").mkdir(parents=True)
                (shard.run_dir / "app" / "a1x" / "app.log").write_text("launched\n")
            shard.save()
            record.shards.append(shard)
        if not missing:
            fleet.merge_side(record, out)
        record_sides.append(record)
    comparison = None
    if len(sides) == 2 and not missing:
        argv = ["diff", "--product", "app", "--out", str(out / "compare"),
                "--expectations-root", str(tmp / "expectations")]
        for record, flag in zip(record_sides, ("--base-run", "--head-run")):
            argv += [a for s in record.shards for a in (flag, str(s.run_dir))]
        comparison = {"exit": quiet(oracle.main, argv), "dir": str(out / "compare"), "labels": list(sides)}
    fleet.write_fleet(out, fleet.fleet_record("r", record_sides, comparison=comparison))
    return out


class FleetDirTests(unittest.TestCase):
    def test_fleet_run_maps_the_failure_to_its_shard_after_a_copy(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            write_corpus(tmp / "corpus")
            out = fleet_dir(tmp)
            # The dir moves to another host: fleet.json's absolute paths are gone, its own layout is not.
            moved = tmp / "copied" / out.name
            moved.parent.mkdir()
            shutil.move(str(out), str(moved))
            code, receipt, index = make_report(moved, tmp)
            self.assertEqual(code, report.EXIT_FAIL)
            self.assertEqual([r["label"] for r in receipt["runs"]], ["head"])
            first = receipt["first_failure"]
            self.assertEqual((first["case"], first["step"]["id"], first["pointer"]["pointer"]),
                             ("beta/c3", "create", "/value/id"))
            shard = oracle.shard_of("beta/c3", 2)
            kinds = {a["kind"]: a for a in first["artifacts"]}
            self.assertEqual(kinds["shard-run"]["path"], f"shards/head/{shard}/run")
            self.assertEqual(kinds["step-log"]["path"], f"shards/head/{shard}/step.log")
            self.assertEqual(kinds["event"]["path"], "head/events.jsonl")
            self.assertTrue(first["replay"].startswith(f"python3 scripts/cmux-next/oracle/fleet.py run --ref {SHA}"))
            self.assertIn({"kind": "log", "path": "shards/head/0/run/app/a1x/app.log", "bytes": 9}, receipt["artifacts"])
            self.assertIn("step-log: shards/head", index.split("## Runs")[0])

    def test_missing_shard_is_incomplete_with_its_step_log(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            write_corpus(tmp / "corpus")
            out = fleet_dir(tmp, missing={("head", 1)})
            code, receipt, index = make_report(out, tmp)
            self.assertEqual(code, report.EXIT_FAIL if receipt["counts"].get("unexpected") else report.EXIT_INCOMPLETE)
            shard = next(f for f in receipt["failures"] if f["kind"] == "shard")
            self.assertIn("shard 1/2 failed: build failed", shard["message"])
            self.assertEqual(shard["artifacts"], [{"kind": "step-log", "path": "shards/head/1/step.log"}])
            self.assertEqual([r["label"] for r in receipt["runs"]], ["head shard 0"])

    def test_fleet_diff_first_divergence(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            write_corpus(tmp / "corpus")
            out = fleet_dir(tmp, sides=("base", "head"),
                            head_value=lambda cid: "changed" if cid == "alpha/c1" else "v-" + cid)
            code, receipt, index = make_report(out, tmp)
            self.assertEqual(code, report.EXIT_FAIL)
            first = receipt["first_failure"]
            self.assertEqual(first["kind"], "divergence")
            self.assertEqual(first["case"], "alpha/c1")
            self.assertEqual(first["step"]["id"], "start")
            self.assertEqual(first["checkpoint"], "before")
            self.assertEqual(first["pointer"], {"pointer": "/value", "from": "divergence", "a": "v-alpha/c1", "b": "changed"})
            paths = [a["path"] for a in first["artifacts"]]
            self.assertIn("compare/divergence.json", paths)
            self.assertTrue(any(a["kind"] == "checkpoint" and a.get("line") for a in first["artifacts"]))
            self.assertEqual(receipt["comparison"]["verdict"], "divergent")
            self.assertIn("## Comparison", index)


class ComparisonDirTests(unittest.TestCase):
    def test_comparison_dir_alone(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            write_corpus(tmp / "corpus")
            run(tmp, ScriptDriver(), out=tmp / "base", run_id="b")
            run(tmp, ScriptDriver(value=lambda cid: "x" if cid == "beta/c2" else "v-" + cid), out=tmp / "head", run_id="h")
            quiet(oracle.main, ["diff", "--out", str(tmp / "cmp"), "--expectations-root", str(tmp / "expectations"),
                                "--base-run", str(tmp / "base"), "--head-run", str(tmp / "head")])
            code, receipt, _ = make_report(tmp / "cmp", tmp)
            self.assertEqual(code, report.EXIT_FAIL)
            self.assertEqual(receipt["source"]["kind"], "comparison")
            self.assertEqual(receipt["first_failure"]["case"], "beta/c2")
            self.assertEqual(len(receipt["runs"]), 2)


if __name__ == "__main__":
    unittest.main()

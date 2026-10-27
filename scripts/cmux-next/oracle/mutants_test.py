#!/usr/bin/env python3
"""Tests of mutants.py (p15-oracle-191): the list, the shards, the verdicts and the report.

    python3 -I scripts/cmux-next/oracle/mutants_test.py [-v]

Stdlib only; needs git. The cargo-mutants list test runs only when cargo-mutants is installed.
"""

from __future__ import annotations

import copy
import json
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import mutants  # noqa: E402

SOURCE = "".join(f"line {i}\n" for i in range(1, 11)).replace("line 5\n", "    a + b\n")
FILE = "crates/cmux-tui-core/src/layout.rs"


def entry(name: str, line: int = 5, file: str = FILE, replacement: str = "a - b") -> dict:
    diff = (f"--- {file}\n+++ replace + with - in f\n@@ -3,5 +3,5 @@\n line 3\n line 4\n-    a + b\n"
            f"+    {replacement}\n line 6\n line 7\n")
    return {"name": f"{file}:{line}:7: {name}", "file": file, "diff": diff, "genre": "BinaryOperator",
            "replacement": replacement, "function": {"function_name": "f"},
            "span": {"start": {"line": line, "column": 7}, "end": {"line": line, "column": 8}}}


def base(**overrides) -> mutants.Base:
    fields = dict(unexpected=frozenset(), leaked=frozenset(), duration_s=10.0, selection="s", fingerprint="f")
    fields.update(overrides)
    return mutants.Base(**fields)


def shard_doc(index: int, total: int, results: list, **overrides) -> dict:
    doc = {"schema": mutants.SHARD_SCHEMA, "sha": "a" * 40, "suites": "daemon-ops,walks", "shard": index,
           "shardTotal": total, "limit": None, "filters": {"file": [], "re": []}, "listDigest": "d",
           "listCount": 3, "baseSelection": "s", "results": results, "patches": {}}
    doc.update(overrides)
    return doc


def result(mutant_id: str, status: str, file: str = FILE, line: int = 1, **extra) -> dict:
    return {"id": mutant_id, "name": f"{file}:{line}:1: m{mutant_id}", "file": file, "line": line, "column": 1,
            "function": "f", "genre": "FnValue", "replacement": "()", "status": status, "reason": "r", **extra}


class ListTest(unittest.TestCase):
    def test_parse_sorts_by_place_and_ids_are_stable(self):
        entries = [entry("late", line=9), entry("early", line=2)]
        first = mutants.parse_list(entries)
        self.assertEqual([m.line for m in first], [2, 9])
        again = mutants.parse_list(list(reversed(entries)))
        self.assertEqual([m.id for m in first], [m.id for m in again])
        self.assertEqual(mutants.list_digest(first), mutants.list_digest(again))

    def test_duplicate_name_is_refused(self):
        with self.assertRaises(ValueError):
            mutants.parse_list([entry("same"), entry("same")])

    def test_diff_naming_another_file_is_refused(self):
        bad = entry("x")
        bad["file"] = "crates/cmux-tui-core/src/mux.rs"
        with self.assertRaises(ValueError):
            mutants.parse_list([bad])

    def test_narrow_by_file_glob_and_name_regex(self):
        listed = mutants.parse_list([entry("one", line=2), entry("two", line=3, file="crates/cmux-tui-core/src/mux/a.rs")])
        self.assertEqual([m.line for m in mutants.narrow(listed, ["layout.rs"], [])], [2])
        self.assertEqual([m.line for m in mutants.narrow(listed, ["crates/*/src/mux/*.rs"], [])], [3])
        self.assertEqual([m.line for m in mutants.narrow(listed, [], ["two$"])], [3])
        self.assertEqual(len(mutants.narrow(listed, [], [])), 2)
        with self.assertRaises(mutants.UsageError):
            mutants.narrow(listed, [], ["("])

    def test_patch_applies_with_git_at_the_repo_root(self):
        git = shutil.which("git")
        if git is None:
            self.skipTest("no git")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            target = root / "cmux-tui" / FILE
            target.parent.mkdir(parents=True)
            target.write_text(SOURCE, encoding="utf-8")
            (mutant,) = mutants.parse_list([entry("op")])
            subprocess.run([git, "init", "-q", str(root)], check=True)
            subprocess.run([git, "-C", str(root), "apply", "-"], input=mutant.patch.encode(), check=True)
            self.assertIn("    a - b\n", target.read_text(encoding="utf-8"))

    def test_cargo_mutants_list_of_this_tree(self):
        if shutil.which("cargo-mutants") is None:
            self.skipTest("cargo-mutants is not installed")
        found = mutants.cargo_mutants_list(mutants.REPO / "cmux-tui", ["crates/cmux-tui-core/src/resource_mutation.rs"], [])
        self.assertTrue(found)
        self.assertTrue(all(m.file.endswith("resource_mutation.rs") for m in found))
        self.assertTrue(all(m.patch.startswith("diff --git a/cmux-tui/") for m in found))


class ShardTest(unittest.TestCase):
    def setUp(self):
        self.mutants = mutants.parse_list([entry(f"m{i}", line=i + 1) for i in range(200)])

    def test_shards_cover_each_mutant_once(self):
        for total in (1, 2, 3, 7):
            names = [m.name for i in range(total) for m in mutants.select_shard(self.mutants, i, total, None)]
            self.assertEqual(sorted(names), sorted(m.name for m in self.mutants))

    def test_a_new_mutant_moves_no_other(self):
        before = {m.name: mutants.shard_of(m.name, 4) for m in self.mutants}
        grown = mutants.parse_list([entry(f"m{i}", line=i + 1) for i in range(201)])
        self.assertTrue(all(mutants.shard_of(m.name, 4) == before[m.name] for m in grown if m.name in before))

    def test_limit_takes_the_first_of_the_shard(self):
        whole = mutants.select_shard(self.mutants, 1, 3, None)
        self.assertEqual(mutants.select_shard(self.mutants, 1, 3, 5), whole[:5])

    def test_parse_shard(self):
        self.assertEqual(mutants.parse_shard("2/4"), (2, 4))
        for bad in ("4/4", "x/2", "1", "-1/2"):
            with self.assertRaises(mutants.UsageError):
                mutants.parse_shard(bad)


class ClassifyTest(unittest.TestCase):
    head_ok = {"unexpected": [], "leaked_state": [], "infra_errors": []}

    def test_divergence_is_caught(self):
        verdict = mutants.classify(base(), self.head_ok, {"divergent": ["daemon-ops/a"], "compared": 3})
        self.assertEqual((verdict["status"], verdict["reason"], verdict["by"]), ("caught", "divergence", ["daemon-ops/a"]))

    def test_all_equal_is_missed(self):
        self.assertEqual(mutants.classify(base(), self.head_ok, {"divergent": [], "compared": 3})["status"], "missed")

    def test_unexpected_case_is_caught_unless_the_clean_run_had_it(self):
        head = {**self.head_ok, "unexpected": ["daemon-ops/x"]}
        cmp = {"divergent": [], "compared": 3}
        self.assertEqual(mutants.classify(base(), head, cmp)["status"], "caught")
        self.assertEqual(mutants.classify(base(unexpected=frozenset({"daemon-ops/x"})), head, cmp)["status"], "missed")

    def test_new_leak_is_caught(self):
        head = {**self.head_ok, "leaked_state": ["daemon-ops/y"]}
        self.assertEqual(mutants.classify(base(), head, {"divergent": [], "compared": 1})["reason"], "leaked-state")

    def test_no_evidence_is_an_error_not_a_survivor(self):
        for head, cmp in ((None, None), (self.head_ok, None),
                          (self.head_ok, {"refused": "nothing compared", "divergent": [], "compared": 0}),
                          (self.head_ok, {"divergent": [], "compared": 0}),
                          ({**self.head_ok, "infra_errors": ["session lost"]}, {"divergent": [], "compared": 2})):
            self.assertEqual(mutants.classify(base(), head, cmp)["status"], "error", (head, cmp))

    def test_run_bounded_kills_the_group_on_timeout(self):
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "log"
            t0 = time.monotonic()
            self.assertIsNone(mutants.run_bounded(["sh", "-c", "sleep 30 & sleep 30"], timeout=0.5, log=log))
            self.assertLess(time.monotonic() - t0, 10)
            self.assertEqual(mutants.run_bounded(["sh", "-c", "echo hi; exit 3"], timeout=10, log=log), 3)
            self.assertIn("hi", log.read_text())


class ReportTest(unittest.TestCase):
    def shards(self):
        return [shard_doc(0, 2, [result("1", "caught", line=3, buildMs=5), result("3", "missed", line=1)],
                          patches={"3": "diff --git a/x b/x\n"}),
                shard_doc(1, 2, [result("2", "unviable", file="crates/cmux-tui-core/src/mux/focus.rs", testMs=9)])]

    def test_report_is_byte_stable_and_has_no_times(self):
        one = mutants.dump(mutants.merge_shards(self.shards()))
        two = mutants.dump(mutants.merge_shards(list(reversed(self.shards()))))
        self.assertEqual(one, two)
        self.assertNotIn("Ms", one)
        report = json.loads(one)
        self.assertTrue(report["complete"])
        self.assertEqual(report["survivors"], ["3"])
        self.assertEqual(report["files"][FILE]["killRate"], 0.5)
        self.assertEqual(report["areas"]["mux"]["unviable"], 1)
        self.assertIsNone(report["areas"]["mux"]["killRate"])

    def test_survivors_queue_routes_to_corpus_gap_with_the_patch(self):
        shards = self.shards()
        queue_doc = mutants.survivors_queue(mutants.merge_shards(shards), shards)
        self.assertEqual(queue_doc["queue"], "MQ-mutants")
        (item,) = queue_doc["items"]
        self.assertEqual((item["id"], item["route"], item["patch"]), ("3", "corpus-gap", "diff --git a/x b/x\n"))
        self.assertNotIn("status", item)

    def test_gap_overlap_and_mismatch(self):
        shards = self.shards()
        self.assertEqual(mutants.merge_shards(shards[:1])["missingShards"], [1])
        self.assertFalse(mutants.merge_shards(shards[:1])["complete"])
        with self.assertRaises(ValueError):
            mutants.merge_shards([shards[0], copy.deepcopy(shards[0])])
        other = copy.deepcopy(shards[1])
        other["listDigest"] = "e"
        with self.assertRaises(ValueError):
            mutants.merge_shards([shards[0], other])
        twice = copy.deepcopy(shards[1])
        twice["results"].append(result("1", "missed"))
        with self.assertRaises(ValueError):
            mutants.merge_shards([shards[0], twice])

    def test_area_of(self):
        self.assertEqual(mutants.area_of("crates/cmux-tui-core/src/resource_router/topology.rs"), "resource_router")
        self.assertEqual(mutants.area_of("crates/cmux-tui-core/src/resource_mutation.rs"), "resource_mutation")
        self.assertEqual(mutants.area_of("crates/cmux-tui-core/src/journal_reducers.rs"), "journal_reducers")
        self.assertEqual(mutants.area_of("crates/cmux-tui-core/src/mux.rs"), "mux")

    def test_report_command_writes_the_three_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            dirs = []
            for index, doc in enumerate(self.shards()):
                d = Path(tmp) / f"s{index}"
                d.mkdir()
                (d / "shard.json").write_text(json.dumps(doc), encoding="utf-8")
                dirs.append(str(d))
            out = Path(tmp) / "out"
            code = subprocess.run([sys.executable, "-I", str(HERE / "mutants.py"), "report", *dirs, "--out", str(out),
                                   "--json"], check=False, stdout=subprocess.PIPE)
            self.assertEqual(code.returncode, 0)
            self.assertEqual(json.loads(code.stdout)["items"][0]["id"], "3")
            for name in ("mutants.json", "survivors.json", "receipt.json"):
                self.assertTrue((out / name).is_file(), name)


if __name__ == "__main__":
    unittest.main()

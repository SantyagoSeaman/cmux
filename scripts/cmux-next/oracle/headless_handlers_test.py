#!/usr/bin/env python3
"""Tests for headless-handlers.py (p15-oracle-162): the candidate set, the dependency kinds, the leaf-first queue and
the B5 domain batches.

Run: python3 -I scripts/cmux-next/oracle/headless_handlers_test.py (stdlib unittest). The unit cases build Facts by
hand; the git case builds a throwaway repository; the last case checks the plan of this checkout against git grep.
"""

from __future__ import annotations

import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]


def load():
    spec = importlib.util.spec_from_file_location("cmux_next_headless_handlers", HERE / "headless-handlers.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


hh = load()
ag = hh.load_app_graph()

ROOT_FILE = "Handlers/AppActionContext.swift"


def layout(groups: dict[str, str], dests: dict[str, str], gate=None, order=None, ui=frozenset()):
    return hh.Layout(group_of=groups, destination=dests, gate=gate or {}, order=order or {}, ui_modules=ui)


def facts(files: dict[str, int], refs: list[tuple], types: dict[str, str], imports=None):
    f = ag.Facts()
    f.files = dict(files)
    f.types = dict(types)
    for ref in refs:
        f.refs[ref] += 1
    f.imports = imports or {}
    return f


class ClassifyTests(unittest.TestCase):
    def setUp(self):
        self.groups = {ROOT_FILE: "Handlers/Infra", "Handlers/A.swift": "Handlers/Tab", "Handlers/B.swift": "Handlers/Tab",
                       "Windows/W.swift": "Windows", "Handlers/C.swift": "Handlers/Pane"}
        self.dests = {"Handlers/Infra": "CmuxNextApp", "Handlers/Tab": "CmuxNextWorkbench", "Windows": "CmuxNextWorkbench",
                      "Handlers/Pane": "CmuxNextWorkbench"}
        self.types = {"AppActionContext": ROOT_FILE, "Beta": "Handlers/B.swift", "WindowManager": "Windows/W.swift",
                      "Gamma": "Handlers/C.swift"}

    def test_kinds(self):
        f = facts({ROOT_FILE: 10, "Handlers/A.swift": 20, "Handlers/B.swift": 5, "Windows/W.swift": 50, "Handlers/C.swift": 7},
                  [("Handlers/A.swift", "", ROOT_FILE, "AppActionContext", "services"),
                   ("Handlers/A.swift", "", "Handlers/B.swift", "Beta", ""),
                   ("Handlers/A.swift", "", "Windows/W.swift", "WindowManager", "active"),
                   # a member Gamma's file adds to WindowManager: a dependency on that file, named by member
                   ("Handlers/B.swift", "", "Handlers/C.swift", "WindowManager", "crossesIncognito"),
                   ("Handlers/A.swift", "", "Handlers/A.swift", "Alpha", "")],
                  self.types, {"Handlers/A.swift": {"CmuxNextDesign", "CmuxNextDaemon"}})
        rows = hh.classify(f, [ROOT_FILE, "Handlers/A.swift", "Handlers/B.swift"],
                           layout(self.groups, self.dests, ui=frozenset({"CmuxNextDesign"})), frozenset())
        a, b, root = rows["Handlers/A.swift"], rows["Handlers/B.swift"], rows[ROOT_FILE]
        self.assertEqual(root.role, "stays")
        self.assertEqual(a.role, "move")
        self.assertEqual(a.host, ["AppActionContext.services"])
        self.assertEqual(a.after, ["Handlers/B.swift"])
        self.assertEqual(a.needs, ["WindowManager (Windows)", "import CmuxNextDesign"])
        self.assertEqual(b.needs, ["WindowManager.crossesIncognito (Handlers/Pane)"])
        self.assertEqual((a.lines, b.lines), (20, 5))

    def test_b5_last_by_stem(self):
        f = facts({"Handlers/A+More.swift": 3}, [], {})
        rows = hh.classify(f, ["Handlers/A+More.swift"],
                           layout({"Handlers/A+More.swift": "Handlers/Tab"}, self.dests), frozenset({"A"}))
        self.assertTrue(rows["Handlers/A+More.swift"].b5_last)


def row(path, group="G", needs=(), after=(), host=(), lines=10, last=False, b5=None, role="move"):
    return hh.Row(path, lines, group, "Dest" + group, None, role, list(host), list(after), list(needs), last, b5)


class OrderTests(unittest.TestCase):
    def test_leaf_first(self):
        rows = {r.path: r for r in [
            row("a", needs=["X (Y)"]),
            row("b"),
            row("c", after=["a"]),
            row("d", host=["AppActionContext"]),
            row("e", last=True),
            row("s", role="stays"),
        ]}
        queue = [r.path for r in hh.order(rows, ag.tarjan)]
        self.assertEqual(queue, ["b", "d", "a", "c", "e", "s"])
        self.assertEqual([rows[p].rank for p in queue], list(range(1, 7)))

    def test_dependency_before_dependent_even_when_heavier(self):
        rows = {r.path: r for r in [row("leaf", after=["heavy"]), row("heavy", needs=["A (x)", "B (y)"]), row("free")]}
        self.assertEqual([r.path for r in hh.order(rows, ag.tarjan)], ["free", "heavy", "leaf"])

    def test_cycle_is_one_contiguous_unit(self):
        rows = {r.path: r for r in [row("p", after=["q"]), row("q", after=["p"]), row("z")]}
        queue = hh.order(rows, ag.tarjan)
        self.assertEqual([r.path for r in queue], ["p", "q", "z"])
        self.assertIsNotNone(queue[0].unit)
        self.assertEqual(queue[0].unit, queue[1].unit)
        self.assertIsNone(queue[2].unit)


class BatchTests(unittest.TestCase):
    def plan(self, rows, max_lines=100):
        return hh.batches(hh.order({r.path: r for r in rows}, ag.tarjan), max_lines)

    def test_size_split_and_order(self):
        out = self.plan([row("a1", "A", lines=60), row("a2", "A", lines=60), row("b1", "B", lines=10, needs=["x (y)"])])
        self.assertEqual([(b["group"], b["files"]) for b in out], [("A", ["a1"]), ("A", ["a2"]), ("B", ["b1"])])
        self.assertEqual([b["index"] for b in out], [1, 2, 3])

    def test_large_file_alone(self):
        out = self.plan([row("big", "A", lines=500), row("small", "A", lines=5, needs=["x (y)"])])
        self.assertEqual([b["files"] for b in out], [["big"], ["small"]])

    def test_cross_group_dependency_closes_and_orders(self):
        # b1 needs a1: A's open batch closes, so a2 (after b1 in the queue) goes to a new A batch after B's
        out = self.plan([row("a1", "A"), row("b1", "B", after=["a1"]),
                         row("a2", "A", after=["b1"], needs=["x (y)"])])
        files = [b["files"] for b in out]
        self.assertEqual(files, [["a1"], ["b1"], ["a2"]])
        for b in out:
            self.assertTrue(all(a < b["index"] for a in b["after"]), b)

    def test_cross_group_cycle_is_its_own_batch(self):
        out = self.plan([row("a0", "A"), row("p", "A", after=["q"]), row("q", "B", after=["p"])])
        cyc = [b for b in out if b["cycle"]]
        self.assertEqual(len(cyc), 1)
        self.assertEqual((cyc[0]["group"], sorted(cyc[0]["files"])), ("A + B", ["p", "q"]))
        self.assertEqual(cyc[0]["destination"], "DestA + DestB")

    def test_b5_last_never_joins_an_earlier_batch(self):
        out = self.plan([row("a1", "A"), row("a2", "A", last=True)])
        self.assertEqual([(b["group"], b["files"]) for b in out], [("A", ["a1"]), ("A (B5 last)", ["a2"])])


class B5RowTests(unittest.TestCase):
    def test_reads_the_row(self):
        doc = ("| A1 | x | - | none | - | done |\n"
               "| B5 | Handlers by domain | - | medium | Handlers touched by PL-cef-tab (BrowserHandlers, "
               "TabLifecycle): those last | |\n")
        self.assertEqual(hh.b5_last(doc), frozenset({"BrowserHandlers", "TabLifecycle"}))

    def test_missing_row_is_an_error(self):
        with self.assertRaises(hh.QueueError):
            hh.b5_last("| B4 | other | - | low | - | |\n")

    def test_checked_in_design_names_the_cef_tab_handlers(self):
        last = hh.b5_last((REPO / hh.ARCH_DESIGN).read_text())
        self.assertIn("PaneTabBarHandlers", last)


class CandidateTests(unittest.TestCase):
    def test_git_grep_semantics(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            handlers = repo / hh.HANDLERS
            (handlers / "Sub").mkdir(parents=True)
            (handlers / "Free.swift").write_text("import Foundation\nstruct Free {}\n")
            (handlers / "Ui.swift").write_text("@preconcurrency import AppKit\n")
            (handlers / "Quartz.swift").write_text("import QuartzCore\n")
            (handlers / "Sub" / "Deep.swift").write_text("import CmuxNextDaemon\n")
            (handlers / "Untracked.swift").write_text("import Foundation\n")
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            subprocess.run(["git", "-C", str(repo), "add", "--", hh.PATHSPEC], check=True)
            subprocess.run(["git", "-C", str(repo), "rm", "-q", "--cached", "--",
                            f"{hh.HANDLERS}/Untracked.swift"], check=True)
            free, every = hh.candidates(repo)
            self.assertEqual(free, [f"{hh.HANDLERS}/Free.swift", f"{hh.HANDLERS}/Sub/Deep.swift"])
            self.assertEqual(len(every), 4)

    def test_no_handlers_is_an_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            subprocess.run(["git", "init", "-q", tmp], check=True)
            with self.assertRaises(hh.QueueError):
                hh.candidates(Path(tmp))


class LiveTests(unittest.TestCase):
    """This checkout: the queue lists exactly what git grep finds, once each, dependencies first."""

    @classmethod
    def setUpClass(cls):
        cls.plan = hh.build_queue(REPO, ag)

    def test_matches_git_grep(self):
        grep = subprocess.run(["git", "-C", str(REPO), "grep", "-L", "-E", hh.UI_IMPORT, "--", hh.PATHSPEC],
                              capture_output=True, text=True, check=False).stdout.split()
        files = subprocess.run(["git", "-C", str(REPO), "ls-files", "--", hh.PATHSPEC],
                               capture_output=True, text=True, check=True).stdout.split()
        self.assertEqual(self.plan["ui_free"], len(grep))
        self.assertEqual(self.plan["handlers"], len(files))
        prefix = ag.APP_DIR + "/"
        self.assertEqual(sorted(prefix + r["path"] for r in self.plan["queue"]), sorted(grep))

    def test_ranks_and_dependencies(self):
        q = self.plan["queue"]
        self.assertEqual([r["rank"] for r in q], list(range(1, len(q) + 1)))
        rank = {r["path"]: r["rank"] for r in q}
        unit = {r["path"]: r["unit"] for r in q}
        for r in q:
            for dep in r["after"]:
                self.assertTrue(rank[dep] < r["rank"] or (r["unit"] is not None and unit[dep] == r["unit"]),
                                (r["path"], dep))
        moves = [r["role"] == "move" for r in q]
        self.assertEqual(moves, sorted(moves, reverse=True), "rows that stay come after the queue")

    def test_batches_cover_the_queue_in_order(self):
        moved = sorted(r["path"] for r in self.plan["queue"] if r["role"] == "move")
        batched = sorted(f for b in self.plan["batches"] for f in b["files"])
        self.assertEqual(batched, moved)
        for b in self.plan["batches"]:
            self.assertTrue(all(a < b["index"] for a in b["after"]), b["index"])
            if len(b["files"]) > 1:
                self.assertLessEqual(b["lines"], hh.MAX_LINES)


if __name__ == "__main__":
    unittest.main()

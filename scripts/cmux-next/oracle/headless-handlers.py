#!/usr/bin/env python3
"""The queue of UI-free action handlers that can move to a headless target, leaf first (p15-oracle-162).

MQ-headless-handlers of p15-oracle.md section 6. The candidates are exactly what git grep finds:

    git grep -L -E 'import (AppKit|SwiftUI|WebKit|QuartzCore)' -- 'Packages/macOS/CmuxNext/Sources/CmuxNextApp/Handlers/*.swift'

out of `git ls-files` of the same pathspec (114 of 191 at the time of writing). For each candidate the script
lists the CmuxNextApp types it uses, read by scripts/cmux-next/app-graph.py (its regex scan, or a Debug build's
index store with --index-store), sorted into three kinds:

  host     a type of the composition root (AppActionContext, AppServices, the Hosts conformances): it becomes a
           member of the domain's `<Domain>ActionHost` (architecture-refactor-DESIGN.md section 4), so it does
           not order the queue;
  after    a type another candidate declares: that candidate moves first (or in the same unit, for a cycle);
  needs    a type the rest of the App declares, or a ui-layer module the file imports: it blocks the file until
           that type moves down or gets a host member.

The queue is a topological order of the `after` edges (cycles are one unit), and among the files that are ready it
takes the one with the fewest `needs`, then the fewest host members, then the earliest PL-arch slice B5 order
(app-graph.json group order), so zero-dependency handlers come first (task 163 moves them). PL-arch B5 asks for the
handlers PL-cef-tab touches to go last; the script reads that list from the B5 row of
architecture-refactor-DESIGN.md section 9, so the two plans stay one plan. Files whose own group stays in the App
(Handlers/Infra, the AppActionContext+X Host conformances) are listed after the queue with role `stays`.

Batches follow B5: one app-graph group (one destination target) per batch, at most 400 lines, emitted so a batch
comes after every batch it needs (Chrome's components/foo/core moves, components/README.md).

Usage:
  scripts/cmux-next/oracle/headless-handlers.py              the queue (TSV, one row per candidate)
  scripts/cmux-next/oracle/headless-handlers.py --batches    the domain batches (TSV)
  scripts/cmux-next/oracle/headless-handlers.py --json       everything as JSON
  scripts/cmux-next/oracle/headless-handlers.py --summary    the counts oracle-DESIGN.md section 2 quotes
  --repo PATH, --index-store PATH, --libindexstore PATH, --max-lines N (default 400)

Test: python3 -I scripts/cmux-next/oracle/headless_handlers_test.py
"""

from __future__ import annotations

import argparse
import collections
import dataclasses
import heapq
import importlib.util
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
HANDLERS = "Packages/macOS/CmuxNext/Sources/CmuxNextApp/Handlers"
PATHSPEC = f"{HANDLERS}/*.swift"
UI_IMPORT = r"import (AppKit|SwiftUI|WebKit|QuartzCore)"
ARCH_DESIGN = "plans/cmux-next/architecture-refactor-DESIGN.md"
B5_ROW = re.compile(r"^\|\s*B5\s*\|.*?PL-cef-tab\s*\(([^)]*)\)", re.M)
MAX_LINES = 400


def load_app_graph():
    """scripts/cmux-next/app-graph.py: the App facts, groups, destinations and the B5 order."""
    path = ROOT / "scripts/cmux-next/app-graph.py"
    spec = importlib.util.spec_from_file_location("cmux_next_app_graph", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class QueueError(Exception):
    """The repository does not have what the queue is built from."""


def git_lines(repo: Path, *args: str) -> list[str]:
    proc = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True, check=False)
    # git grep exits 1 when nothing matches; anything else is a real failure
    if proc.returncode not in (0, 1):
        raise QueueError(f"git {' '.join(args)}: {proc.stderr.strip()}")
    return sorted(line for line in proc.stdout.splitlines() if line)


def candidates(repo: Path) -> tuple[list[str], list[str]]:
    """(UI-free handler files, all handler files), repo-relative, from git grep -L and git ls-files."""
    every = git_lines(repo, "ls-files", "--", PATHSPEC)
    if not every:
        raise QueueError(f"no files match {PATHSPEC} in {repo}")
    free = git_lines(repo, "grep", "-L", "-E", UI_IMPORT, "--", PATHSPEC)
    return free, every


def b5_last(design_text: str) -> frozenset[str]:
    """File stems the B5 row says PL-cef-tab touches (those move last)."""
    m = B5_ROW.search(design_text)
    if not m:
        raise QueueError(f"{ARCH_DESIGN} has no B5 row naming the PL-cef-tab handlers")
    return frozenset(s.strip() for s in m.group(1).split(",") if s.strip())


def stem(rel: str) -> str:
    return re.sub(r"(\+.*)?\.swift$", "", rel.rsplit("/", 1)[-1])


def short(rel: str) -> str:
    return rel.rsplit("/", 1)[-1].removesuffix(".swift")


@dataclasses.dataclass
class Row:
    path: str  # relative to the App directory (Handlers/X.swift)
    lines: int
    group: str
    destination: str
    gate: str | None
    role: str  # move | stays
    host: list[str] = dataclasses.field(default_factory=list)
    after: list[str] = dataclasses.field(default_factory=list)
    needs: list[str] = dataclasses.field(default_factory=list)  # "Type (declaring group)" or "import M"
    b5_last: bool = False
    b5_order: int | None = None
    unit: int | None = None
    rank: int | None = None


@dataclasses.dataclass(frozen=True)
class Layout:
    """What the plan needs from app-graph: each App file's group, each group's destination and gate, the B5 order
    of each group, and the ui-layer modules."""

    group_of: dict[str, str]
    destination: dict[str, str]
    gate: dict[str, str | None]
    order: dict[str, int]
    ui_modules: frozenset[str]
    app: str = "CmuxNextApp"


def classify(facts, free: list[str], layout: Layout, last: frozenset[str]) -> dict[str, Row]:
    """One Row per UI-free file (App-relative paths in FREE), with its host members, queue edges and needs."""
    rows: dict[str, Row] = {}
    for rel in free:
        group = layout.group_of[rel]
        dest = layout.destination[group]
        rows[rel] = Row(rel, facts.files.get(rel, 0), group, dest, layout.gate.get(group),
                        "stays" if dest == layout.app else "move",
                        b5_last=stem(rel) in last, b5_order=layout.order.get(group))
    host: dict[str, set[str]] = collections.defaultdict(set)
    after: dict[str, set[str]] = collections.defaultdict(set)
    needs: dict[str, set[str]] = collections.defaultdict(set)
    for (src, _src_type, dst, dst_type, member), _n in facts.refs.items():
        row = rows.get(src)
        if row is None or row.role != "move" or dst == src:
            continue
        dst_group = layout.group_of.get(dst)
        if dst_group is None:
            continue
        if layout.destination.get(dst_group) == layout.app:
            host[src].add(f"{dst_type}.{member}" if member else dst_type)
        elif dst in rows and rows[dst].role == "move":
            after[src].add(dst)
        else:
            # a member another file adds to the type (an extension) is a dependency on that file, so name it
            shown = f"{dst_type}.{member}" if member and facts.types.get(dst_type) != dst else dst_type
            needs[src].add(f"{shown} ({dst_group})")
    for rel, row in rows.items():
        if row.role != "move":
            continue
        ui = sorted(m for m in facts.imports.get(rel, ()) if m in layout.ui_modules)
        row.host = sorted(host[rel])
        row.after = sorted(after[rel])
        row.needs = sorted(needs[rel]) + [f"import {m}" for m in ui]
    return rows


def order(rows: dict[str, Row], tarjan) -> list[Row]:
    """Leaf-first queue: `after` edges in topological order, cycles as one unit, ready units by fewest needs."""
    movers = sorted(r for r, row in rows.items() if row.role == "move")
    edges = {r: set(rows[r].after) for r in movers}
    comps = tarjan(movers, edges)
    unit_of = {r: i for i, comp in enumerate(comps) for r in comp}
    deps: dict[int, set[int]] = {i: set() for i in range(len(comps))}
    users: dict[int, set[int]] = collections.defaultdict(set)
    for r in movers:
        for d in edges[r]:
            if unit_of[d] != unit_of[r]:
                deps[unit_of[r]].add(unit_of[d])
                users[unit_of[d]].add(unit_of[r])

    def key(i: int) -> tuple:
        members = [rows[r] for r in comps[i]]
        b5 = min((m.b5_order for m in members if m.b5_order is not None), default=1 << 30)
        return (any(m.b5_last for m in members), sum(len(m.needs) for m in members),
                sum(len(m.host) for m in members), b5, comps[i][0])

    ready = [(key(i), i) for i, d in deps.items() if not d]
    heapq.heapify(ready)
    waiting = {i: len(d) for i, d in deps.items()}
    out: list[Row] = []
    while ready:
        _, i = heapq.heappop(ready)
        for r in comps[i]:
            rows[r].unit = i if len(comps[i]) > 1 else None
            rows[r].rank = len(out) + 1
            out.append(rows[r])
        for u in sorted(users[i]):
            waiting[u] -= 1
            if waiting[u] == 0:
                heapq.heappush(ready, (key(u), u))
    for row in sorted((row for row in rows.values() if row.role != "move"), key=lambda x: x.path):
        row.rank = len(out) + 1
        out.append(row)
    return out


def batches(queue: list[Row], max_lines: int = MAX_LINES) -> list[dict]:
    """B5 domain batches: one group per batch, at most MAX_LINES lines (a larger file alone). Walks the queue a unit
    at a time (a cycle unit never splits; one that spans groups is its own batch named after all of them); a unit
    that needs a file in another open batch closes that batch first, so the batches form a DAG. They are listed in
    topological order, earliest queue rank first."""
    units: list[list[Row]] = []
    for row in queue:
        if row.role != "move":
            continue
        if units and row.unit is not None and units[-1][0].unit == row.unit:
            units[-1].append(row)
        else:
            units.append([row])
    made: list[dict] = []
    open_: dict[str, dict] = {}
    batch_of: dict[str, dict] = {}
    for unit in units:
        # B5: the files PL-cef-tab touches move last, so they never join an earlier batch of their group
        key = " + ".join(sorted({r.group for r in unit})) + (" (B5 last)" if any(r.b5_last for r in unit) else "")
        paths = {r.path for r in unit}
        for dep in sorted({d for r in unit for d in r.after} - paths):
            b = batch_of.get(dep)
            if b is not None and b["group"] != key:
                open_.pop(b["group"], None)
        lines = sum(r.lines for r in unit)
        b = open_.get(key)
        if b is not None and b["lines"] + lines > max_lines:
            del open_[key]
            b = None
        if b is None:
            b = {"group": key, "destination": " + ".join(sorted({r.destination for r in unit})),
                 "gate": " + ".join(sorted({r.gate for r in unit if r.gate})) or None, "files": [], "lines": 0,
                 "needs": 0, "first": unit[0].rank, "cycle": len(unit) > 1, "after": set()}
            made.append(b)
            if len({r.group for r in unit}) == 1:
                open_[key] = b
        for r in unit:
            b["files"].append(r.path)
            b["lines"] += r.lines
            b["needs"] += len(r.needs)
            b["cycle"] = b["cycle"] or r.unit is not None
            batch_of[r.path] = b
    for i, b in enumerate(made):
        b["id"] = i
    for b in made:
        b["after"] = {batch_of[d]["id"] for f in b["files"] for d in next(r for r in queue if r.path == f).after
                      if batch_of[d] is not b}
    users: dict[int, set[int]] = collections.defaultdict(set)
    for b in made:
        for a in b["after"]:
            users[a].add(b["id"])
    waiting = {b["id"]: len(b["after"]) for b in made}
    ready = [(b["first"], b["id"]) for b in made if not b["after"]]
    heapq.heapify(ready)
    out: list[dict] = []
    while ready:
        _, i = heapq.heappop(ready)
        out.append(made[i])
        for u in sorted(users[i]):
            waiting[u] -= 1
            if waiting[u] == 0:
                heapq.heappush(ready, (made[u]["first"], u))
    if len(out) != len(made):
        raise QueueError("the domain batches depend on each other in a cycle")
    index = {b["id"]: n for n, b in enumerate(out, 1)}
    return [{"index": index[b["id"]], "group": b["group"], "destination": b["destination"], "gate": b["gate"],
             "lines": b["lines"], "needs": b["needs"], "cycle": b["cycle"], "files": b["files"],
             "after": sorted(index[a] for a in b["after"])} for b in out]


def build_queue(repo: Path, ag, facts=None) -> dict:
    """The whole plan for REPO: the candidate counts, the queue and the batches."""
    free, every = candidates(repo)
    app_dir = repo / ag.APP_DIR
    world = ag.World.read(repo)
    facts = facts if facts is not None else ag.regex_facts(app_dir)
    graph = ag.build(facts, world)
    group_of = ag.assign_groups(facts, world, ag.DESTINATIONS)
    dests = {g: ag.destination_of(g, world, ag.DESTINATIONS)[0] for g in set(group_of.values())}
    layout = Layout(group_of=group_of,
                    destination={g: d.target for g, d in dests.items()},
                    gate={g: d.gated for g, d in dests.items()},
                    order={g: v["order"] for g, v in graph["groups"].items()},
                    ui_modules=frozenset(m for m, layer in world.layers.items() if layer in ("ui", "app")),
                    app=ag.APP)
    prefix = ag.APP_DIR + "/"
    rels = [p[len(prefix):] for p in free]
    missing = [r for r in rels if r not in facts.files]
    if missing:
        raise QueueError(f"git lists files the scan did not read: {', '.join(missing)}")
    last = b5_last((repo / ARCH_DESIGN).read_text())
    rows = classify(facts, rels, layout, last)
    queue = order(rows, ag.tarjan)
    return {"pattern": UI_IMPORT, "pathspec": PATHSPEC, "source": facts.source,
            "ui_free": len(free), "handlers": len(every), "b5_last": sorted(last),
            "queue": [dataclasses.asdict(r) for r in queue], "batches": batches(queue)}


def summary(plan: dict) -> list[str]:
    q = plan["queue"]
    moves = [r for r in q if r["role"] == "move"]
    zero = [r for r in moves if not r["needs"] and not r["after"]]
    free_of_needs = [r for r in moves if not r["needs"]]
    return [
        f"UI-free handler files: {plan['ui_free']} of {plan['handlers']} ({plan['source']} scan)",
        f"to move: {len(moves)}; stay in the App (composition root, Host conformances): {len(q) - len(moves)}",
        f"no App type outside the composition root and no ui-layer import: {len(free_of_needs)} "
        f"({len(zero)} also need no other queued handler)",
        f"cycle units: {len({r['unit'] for r in moves if r['unit'] is not None})}; "
        f"B5 PL-cef-tab files queued last: {sum(1 for r in moves if r['b5_last'])}",
        f"domain batches (<= {MAX_LINES} lines): {len(plan['batches'])}",
    ]


def queue_tsv(plan: dict) -> list[str]:
    out = ["rank\tfile\tlines\tgroup\tdestination\tgate\trole\tneeds\thost\tafter\tnotes"]
    for r in plan["queue"]:
        notes = [n for n in (f"unit {r['unit']}" if r["unit"] is not None else "",
                             "B5 last (PL-cef-tab)" if r["b5_last"] else "") if n]
        out.append("\t".join([
            str(r["rank"]), r["path"], str(r["lines"]), r["group"], r["destination"], r["gate"] or "-", r["role"],
            "; ".join(r["needs"]) or "-", str(len(r["host"])),
            ", ".join(short(a) for a in r["after"]) or "-", ", ".join(notes) or "-"]))
    return out


def batches_tsv(plan: dict) -> list[str]:
    out = ["batch\tgroup\tdestination\tgate\tlines\tneeds\tafter\tfiles"]
    for b in plan["batches"]:
        group = b["group"] + (" (cycle)" if b["cycle"] else "")
        out.append("\t".join([str(b["index"]), group, b["destination"], b["gate"] or "-", str(b["lines"]),
                              str(b["needs"]), ", ".join(map(str, b["after"])) or "-",
                              ", ".join(short(f) for f in b["files"])]))
    return out


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--repo", type=Path, default=ROOT, help="checkout to read (default this one)")
    ap.add_argument("--index-store", type=Path, help="index store of a Debug build (exact references)")
    ap.add_argument("--libindexstore", help="path of libIndexStore.dylib/.so")
    ap.add_argument("--max-lines", type=int, default=MAX_LINES, help="lines per domain batch")
    out = ap.add_mutually_exclusive_group()
    out.add_argument("--json", action="store_true", help="print the plan as JSON")
    out.add_argument("--batches", action="store_true", help="print the domain batches (TSV)")
    out.add_argument("--summary", action="store_true", help="print the counts")
    args = ap.parse_args(argv)

    ag = load_app_graph()
    repo = args.repo.resolve()
    facts = None
    if args.index_store:
        app_dir = repo / ag.APP_DIR
        records = ag.read_index_store(args.index_store, app_dir, ag.find_libindexstore(args.libindexstore))
        facts = ag.index_facts(app_dir, records)
    try:
        plan = build_queue(repo, ag, facts)
    except QueueError as err:
        print(f"headless-handlers: {err}", file=sys.stderr)
        return 2
    if args.max_lines != MAX_LINES:
        queue = [Row(**r) for r in plan["queue"]]
        plan["batches"] = batches(queue, args.max_lines)
    if args.json:
        json.dump(plan, sys.stdout, indent=1)
        sys.stdout.write("\n")
    elif args.batches:
        print("\n".join(batches_tsv(plan)))
    elif args.summary:
        print("\n".join(summary(plan)))
    else:
        print("\n".join(queue_tsv(plan)))
    return 0


if __name__ == "__main__":
    sys.exit(main())

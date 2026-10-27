#!/usr/bin/env python3
"""Sanitizer reports as oracle results (p15-oracle-187; plans/cmux-next/chrome-parity/p15-oracle.md, task 187).

Every AddressSanitizer, LeakSanitizer and ThreadSanitizer report becomes a Crash of the case (a daemon case,
or a Swift test target of swift-tier.sh) it came from, with one stack key per report: `<kind>-<hash12>`, the
signature of scripts/cmux-next/crash_signature.py (ClusterFuzz's crash state, the same id the Rust lane
cmux-tui/scripts/sanitize.sh and the Swift sweep give). expectations.py judges the Crash: a stack is expected
only by a case-level Crash line or a SanitizerExpectations.d stack line. The MQ-races queue has one line per
unique stack, whatever the number of cases and reports that hit it.

Chromium does the same on its ASan, LSan and TSan bots (build/config/sanitizers/sanitizers.gni): one
instrumented build runs the normal suites, the test launcher (base/test/launcher/test_launcher.cc) makes a
child that a sanitizer killed a CRASH result of its test, and ClusterFuzz groups reports by crash state.

Layout of a daemon lane's reports (linux_run.sh sanitize writes it through its binary wrapper, whose
CMUX_ORACLE_CASE_ID names the case): REPORTS/<suite>/<case>/report.<pid>, and REPORTS/_unattributed/ for a
process that ran outside a case. An unattributed stack is always red: no line can expect it.

Usage:
  sanitizer_results.py daemon --run DIR --reports DIR --build asan|tsan --out FILE --races FILE
                              [--verdicts FILE] [--build-receipt FILE] [--oracle-exit N] [--root DIR]
      The lane receipt (cmux-oracle-sanitize-v1), the MQ-races queue (JSON lines) and the verdict records
      (`expectations.py lint --results`). Exit 0 when nothing is red, 1 otherwise, 2 on bad input.
  sanitizer_results.py keys PATH...    one JSON line per report in the files or directories
  sanitizer_results.py selftest

Stdlib only, safe under `python3 -I`.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import sys
import tempfile
from pathlib import Path
from typing import Iterable, Mapping, Sequence

HERE = Path(__file__).resolve().parent
for _path in (HERE, HERE.parent):  # expectations.py beside this file, crash_signature.py one level up
    if str(_path) not in sys.path:
        sys.path.insert(0, str(_path))

import crash_signature  # noqa: E402
import expectations as oracle_exp  # noqa: E402

RECEIPT_SCHEMA = "cmux-oracle-sanitize-v1"
QUEUE = "MQ-races"
UNATTRIBUTED = "_unattributed"
EXAMPLES = 3
SANITIZER_OF_BUILD = {"asan": "address", "tsan": "thread"}
STACK_KINDS = frozenset(kind for kinds in oracle_exp.STACK_KINDS_BY_BUILD.values() for kind in kinds)


@dataclasses.dataclass(frozen=True)
class Report:
    """One sanitizer report: its stack key and the signature fields a queue line shows."""

    key: str
    kind: str
    exception: str
    frames: tuple[str, ...]
    process: str
    source: str
    message: str


def reports_from_text(text: str, source: str = "", process: str = "") -> list[Report]:
    """Every ASan, LSan and TSan report in `text`; other report kinds have no stack key and are left out."""
    out = []
    for sig in crash_signature.signatures_from_text(text, Path(source).name, process=process):
        if sig.kind in STACK_KINDS:
            out.append(Report(f"{sig.kind}-{sig.hash12}", sig.kind, sig.exception, tuple(sig.frames),
                              sig.process, source, sig.message))
    return out


def reports_in(path: Path, process: str = "") -> list[Report]:
    """The reports of a file, or of every file under a directory (sorted); a missing path has none."""
    files = sorted(p for p in path.rglob("*") if p.is_file()) if path.is_dir() else [path] if path.is_file() else []
    out: list[Report] = []
    for file in files:
        out.extend(reports_from_text(file.read_text(encoding="utf-8", errors="replace"), str(file), process))
    return out


def stack_keys(reports: Iterable[Report]) -> tuple[str, ...]:
    """The keys of the reports, once each, sorted (the Actual a Crash with stacks is judged by)."""
    return tuple(sorted({r.key for r in reports}))


def crash_of(reports: Sequence[Report]) -> oracle_exp.Actual:
    return oracle_exp.Actual(oracle_exp.CRASH, stacks=stack_keys(reports))


# ------------------------------------------------------------------------------------------ queue


@dataclasses.dataclass
class Stack:
    """Every occurrence of one stack key in a run."""

    first: Report
    cases: dict[str, bool] = dataclasses.field(default_factory=dict)  # case -> expected there
    reports: int = 0
    examples: list[str] = dataclasses.field(default_factory=list)
    lines: set[str] = dataclasses.field(default_factory=set)

    def add(self, case: str, report: Report, expected: bool) -> None:
        self.cases[case] = self.cases.get(case, True) and expected
        self.reports += 1
        if report.source and report.source not in self.examples and len(self.examples) < EXAMPLES:
            self.examples.append(report.source)


def race_queue(stacks: Mapping[str, Stack], *, suite: str, build: str, sha: str) -> list[dict]:
    """One MQ-races line per unique stack: the unexpected ones first, then by how many cases they hit."""
    rows = []
    for key, stack in stacks.items():
        expected = all(stack.cases.values())
        rows.append({
            "id": f"{QUEUE}:{key}",
            "queue": QUEUE,
            "kind": "sanitizer",
            "key": key,
            "sanitizer": stack.first.kind,
            "exception": stack.first.exception,
            "frames": list(stack.first.frames),
            "process": stack.first.process,
            "message": stack.first.message,
            "suite": suite,
            "build": build,
            "sha": sha,
            "expected": expected,
            "lines": sorted(stack.lines),
            "cases": sorted(stack.cases),
            "unexpected_cases": sorted(c for c, ok in stack.cases.items() if not ok),
            "reports": stack.reports,
            "examples": stack.examples,
        })
    rows.sort(key=lambda r: (r["expected"], -len(r["cases"]), r["key"]))
    return rows


def record(stacks: dict[str, Stack], case: str, reports: Sequence[Report], covering: Mapping[str, Sequence[str]]) -> None:
    """Adds a case's reports to the per-stack totals. `covering` maps a stack key to the lines that expect it in
    this case; a stack with none is unexpected there (an unattributed report never has one)."""
    for report in reports:
        stack = stacks.setdefault(report.key, Stack(report))
        lines = covering.get(report.key, ())
        stack.add(case, report, bool(lines))
        stack.lines.update(lines)


def covering_lines(exp: oracle_exp.Expectations, suite: str, case: str, config: oracle_exp.RunConfig,
                   stacks: Sequence[str]) -> dict[str, list[str]]:
    """Stack key -> the lines that expect it, as Expectations.evaluate decides: a case-level Crash line expects
    every stack of the case, a stack line only its own."""
    case_lines = [e.key for e in exp.resolve(suite, case, config).lines if oracle_exp.CRASH in e.outcomes]
    stack_lines = exp.stack_lines(suite, case, config)
    return {s: case_lines or [e.key for e in stack_lines if e.stack == s] for s in stacks}


# ------------------------------------------------------------------------------------- daemon lane


def case_reports(reports_dir: Path, process: str) -> dict[str, list[Report]]:
    """Case id (or _unattributed) -> its reports, from REPORTS/<case id>/report.<pid>; a case id is the corpus
    path without .json, so it may hold more than one `/`."""
    out: dict[str, list[Report]] = {}
    if not reports_dir.is_dir():
        return out
    for path in sorted(p for p in reports_dir.rglob("*") if p.is_file()):
        rel = path.relative_to(reports_dir).parts
        case = UNATTRIBUTED if rel[0] == UNATTRIBUTED or len(rel) < 3 else "/".join(rel[:-1])
        found = reports_from_text(path.read_text(encoding="utf-8", errors="replace"), str(path), process)
        if found:
            out.setdefault(case, []).extend(found)
    return out


def judge_daemon(
    receipt: Mapping, by_case: Mapping[str, Sequence[Report]], exp: oracle_exp.Expectations, build: str
) -> tuple[dict, list[dict], list[dict]]:
    """The lane receipt fields, the race queue and the verdict records of one daemon run."""
    config = oracle_exp.RunConfig(platform=receipt.get("platform") or "linux", arm=receipt.get("arm"),
                                  product=receipt.get("product") or "daemon", build=build)
    records = receipt.get("cases") or {}
    stacks: dict[str, Stack] = {}
    cases: dict[str, dict] = {}
    unexpected: list[dict] = []
    verdicts: list[dict] = []
    for case in sorted(set(records) | set(by_case)):
        reports = by_case.get(case, ())
        oracle_record = records.get(case) or {}
        if case == UNATTRIBUTED:
            record(stacks, case, reports, {})
            unexpected.append({"case": case, "stacks": list(stack_keys(reports))})
            continue
        suite, _, name = case.partition("/")
        if reports:
            actual = crash_of(reports)
            verdict = exp.evaluate(suite, name, config, actual)
            record(stacks, case, reports, covering_lines(exp, suite, name, config, actual.stacks))
            cases[case] = {"result": oracle_exp.CRASH, "stacks": list(stack_keys(reports)), "reports": len(reports),
                           "oracle": {k: oracle_record.get(k) for k in ("outcome", "status")},
                           "verdict": verdict.to_json()}
            if verdict.fails_run:
                unexpected.append({"case": case, "stacks": list(verdict.uncovered_stacks or stack_keys(reports))})
        elif oracle_record.get("status") == oracle_exp.PASS and not oracle_record.get("flaky"):
            verdict = exp.evaluate(suite, name, config, oracle_exp.Actual(oracle_exp.PASS))
        else:
            continue
        if verdict.lines:
            verdicts.append({"case": case, **verdict.to_json()})
    # A case the oracle itself found red with no report: a sanitizer slowdown past a bound, or a real
    # failure the build shows. Neither is green; a [ Slow ] or result line on the build tag says why it may be.
    oracle_red = sorted(c for c in receipt.get("unexpected") or () if c not in by_case)
    leaked = sorted(c for c in receipt.get("leaked_state") or () if c not in by_case)
    infra = list(receipt.get("infra_errors") or ())
    fields = {
        "cases": cases,
        "unexpected": unexpected,
        "unexpectedOracle": oracle_red,
        "leakedState": leaked,
        "infraErrors": infra,
        "counts": {
            "cases": len(records),
            "withReports": sum(1 for c in by_case if c != UNATTRIBUTED),
            "reports": sum(len(r) for r in by_case.values()),
            "stacks": len(stacks),
            "unexpectedStacks": sum(1 for s in stacks.values() if not all(s.cases.values())),
        },
        "ok": not unexpected and not oracle_red and not leaked and not infra,
    }
    queue = race_queue(stacks, suite="daemon", build=build, sha=str(receipt.get("sha") or ""))
    return fields, queue, verdicts


def write_jsonl(path: Path, rows: Iterable[Mapping]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f".{path.name}.tmp")
    with tmp.open("w", encoding="utf-8") as handle:
        for row in rows:
            handle.write(json.dumps(row, sort_keys=True) + "\n")
    tmp.replace(path)


def write_json(path: Path, value: Mapping) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f".{path.name}.tmp")
    tmp.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    tmp.replace(path)


def cmd_daemon(args: argparse.Namespace) -> int:
    receipt_path = args.run / "receipt.json"
    try:
        receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        print(f"sanitizer_results: {receipt_path}: {error}", file=sys.stderr)
        return 2
    build_receipt = {}
    if args.build_receipt:
        try:
            build_receipt = json.loads(args.build_receipt.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            print(f"sanitizer_results: {args.build_receipt}: {error}", file=sys.stderr)
            return 2
    exp = oracle_exp.Expectations.load_root(args.root)
    if exp.diagnostics:
        for diag in exp.diagnostics:
            print(f"sanitizer_results: {diag.render()}", file=sys.stderr)
        return 2
    by_case = case_reports(args.reports, "cmux-tui")
    fields, queue, verdicts = judge_daemon(receipt, by_case, exp, args.build)
    ok = fields.pop("ok") and args.oracle_exit in (0, 1)
    doc = {
        "schema": RECEIPT_SCHEMA,
        "sha": receipt.get("sha"),
        "product": receipt.get("product"),
        "platform": receipt.get("platform"),
        "arm": receipt.get("arm"),
        "build": args.build,
        "sanitizer": SANITIZER_OF_BUILD[args.build],
        "binary": {k: build_receipt.get(k) for k in ("binarySha256", "rustc", "sanitizer", "toolchain", "targetDir")},
        "run": str(args.run),
        "reports": str(args.reports),
        "oracleExit": args.oracle_exit,
        "oracleOk": receipt.get("ok"),
        **fields,
        "races": str(args.races),
        "ok": ok,
    }
    write_json(args.out, doc)
    write_jsonl(args.races, queue)
    if args.verdicts:
        write_jsonl(args.verdicts, verdicts)
    counts = doc["counts"]
    print(f"sanitize {args.build}: {counts['cases']} cases, {counts['withReports']} with reports, "
          f"{counts['stacks']} unique stacks ({counts['unexpectedStacks']} unexpected), "
          f"{len(doc['unexpectedOracle'])} other reds; {'OK' if ok else 'FAILED'}", file=sys.stderr)
    for item in doc["unexpected"][:20]:
        print(f"  unexpected: {item['case']} {' '.join(item['stacks'])}", file=sys.stderr)
    return 0 if ok else 1


def cmd_keys(args: argparse.Namespace) -> int:
    for path in args.paths:
        for report in reports_in(path, args.process):
            print(json.dumps(dataclasses.asdict(report), sort_keys=True))
    return 0


# --------------------------------------------------------------------------------------- selftest

TSAN_REPORT = """==================
WARNING: ThreadSanitizer: data race (pid=4242)
  Write of size 8 at 0x7b0400000010 by thread T2:
    #0 cmux_tui_core::pane::Pane::resize /src/cmux-tui/crates/cmux-tui-core/src/pane.rs:120:9 (cmux-tui+0x1234)
    #1 cmux_tui_core::session::Session::apply /src/cmux-tui/crates/cmux-tui-core/src/session.rs:44:5 (cmux-tui+0x2234)
    #2 std::thread::Builder::spawn_unchecked_ /rustc/abc/library/std/src/thread/mod.rs:522:17 (cmux-tui+0x3234)

  Previous read of size 8 at 0x7b0400000010 by main thread:
    #0 cmux_tui_core::pane::Pane::size /src/cmux-tui/crates/cmux-tui-core/src/pane.rs:90:9 (cmux-tui+0x4234)

SUMMARY: ThreadSanitizer: data race /src/cmux-tui/crates/cmux-tui-core/src/pane.rs:120:9 in cmux_tui_core::pane::Pane::resize
==================
"""

ASAN_REPORT = """=================================================================
==77==ERROR: AddressSanitizer: heap-use-after-free on address 0x602000000010 at pc 0x1 bp 0x2 sp 0x3
READ of size 4 at 0x602000000010 thread T0
    #0 0x5555 in cmux_tui_core::grid::Grid::cell /src/cmux-tui/crates/cmux-tui-core/src/grid.rs:10:5
    #1 0x5556 in cmux_tui_core::render::frame /src/cmux-tui/crates/cmux-tui-core/src/render.rs:20:5
    #2 0x5557 in main /src/cmux-tui/crates/cmux-tui/src/main.rs:3:1

SUMMARY: AddressSanitizer: heap-use-after-free /src/cmux-tui/crates/cmux-tui-core/src/grid.rs:10:5 in cmux_tui_core::grid::Grid::cell
"""


def _selftest() -> int:
    failures: list[str] = []
    count = 0

    def check(name: str, condition: bool) -> None:
        nonlocal count
        count += 1
        if not condition:
            failures.append(name)

    race = reports_from_text(TSAN_REPORT, "report.4242", "cmux-tui")
    again = reports_from_text(TSAN_REPORT.replace("4242", "5151").replace("0x7b0400000010", "0x7b0400000099"),
                              "report.5151", "cmux-tui")
    uaf = reports_from_text(ASAN_REPORT, "report.77", "cmux-tui")
    check("tsan report keyed", len(race) == 1 and race[0].key.startswith("tsan-") and oracle_exp.STACK_RE.match(race[0].key))
    check("same stack, same key across pids and addresses", [r.key for r in again] == [r.key for r in race])
    check("asan report keyed", len(uaf) == 1 and uaf[0].key.startswith("asan-"))
    check("crash with stacks", crash_of(race + again).stacks == (race[0].key,))
    check("no report in plain output", reports_from_text("test passed\nSUMMARY: nothing\n") == [])

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        reports = root / "reports"
        for rel, text in (("daemon-ops/a/report.4242", TSAN_REPORT), ("daemon-ops/a/report.5151", TSAN_REPORT),
                          ("daemon-ops/b/report.9", TSAN_REPORT), ("daemon-ops/c/report.1", "no report here\n"),
                          (f"{UNATTRIBUTED}/report.3", ASAN_REPORT)):
            (reports / rel).parent.mkdir(parents=True, exist_ok=True)
            (reports / rel).write_text(text)
        nested = reports / "walks" / "min" / "seed-7" / "report.8"
        nested.parent.mkdir(parents=True)
        nested.write_text(TSAN_REPORT)
        by_case = case_reports(reports, "cmux-tui")
        check("reports by case", sorted(by_case) == sorted(["daemon-ops/a", "daemon-ops/b", "walks/min/seed-7",
                                                           UNATTRIBUTED])
              and len(by_case["daemon-ops/a"]) == 2)
        nested.unlink()
        by_case = case_reports(reports, "cmux-tui")
        key = race[0].key
        receipt = {"sha": "f" * 40, "platform": "linux", "product": "daemon", "arm": None,
                   "cases": {"daemon-ops/a": {"status": "Pass", "outcome": "expected", "flaky": False},
                             "daemon-ops/b": {"status": "Crash", "outcome": "unexpected", "flaky": False},
                             "daemon-ops/c": {"status": "Pass", "outcome": "expected", "flaky": False},
                             "daemon-ops/d": {"status": "Timeout", "outcome": "unexpected", "flaky": False}},
                   "unexpected": ["daemon-ops/b", "daemon-ops/d"], "leaked_state": [], "infra_errors": []}
        no_lines = oracle_exp.Expectations([], [])
        del by_case[UNATTRIBUTED]
        fields, queue, _ = judge_daemon(receipt, by_case, no_lines, "tsan")
        check("an unexpected stack is red once per stack", not fields["ok"] and len(queue) == 1
              and queue[0]["key"] == key and queue[0]["cases"] == ["daemon-ops/a", "daemon-ops/b"]
              and queue[0]["reports"] == 3 and not queue[0]["expected"]
              and fields["cases"]["daemon-ops/a"]["result"] == "Crash")
        check("an oracle red without a report stays red", fields["unexpectedOracle"] == ["daemon-ops/d"])
        lines, _ = oracle_exp.parse_text(f"p15-oracle-187 [ linux tsan ] daemon-ops/*@{key} [ Pass Crash ]\n"
                                         "p15-oracle-187 [ linux tsan ] daemon-ops/d [ Skip ]\n",
                                         "daemon-ops.txt", oracle_exp.SANITIZER_KIND)
        expected = oracle_exp.Expectations(lines, [])
        receipt_ok = {**receipt, "unexpected": ["daemon-ops/b"]}
        fields, queue, verdicts = judge_daemon(receipt_ok, by_case, expected, "tsan")
        check("an expected stack is green", fields["ok"] and queue[0]["expected"]
              and queue[0]["lines"] == [f"p15-oracle-187 [ linux tsan ] daemon-ops/*@{key} [ Pass Crash ]"])
        check("verdict records carry the line states", {v["case"] for v in verdicts}
              == {"daemon-ops/a", "daemon-ops/b", "daemon-ops/c"})
        fields, _, _ = judge_daemon(receipt_ok, by_case, expected, "asan")
        check("a tsan line does not cover an asan run", not fields["ok"])
        unattributed = {UNATTRIBUTED: reports_from_text(ASAN_REPORT, "report.3")}
        fields, queue, _ = judge_daemon({**receipt_ok, "cases": {}}, unattributed, expected, "asan")
        check("an unattributed stack is red", not fields["ok"] and queue[0]["cases"] == [UNATTRIBUTED])

        # The CLI writes the receipt, the queue and the verdicts.
        run = root / "run"
        run.mkdir()
        (run / "receipt.json").write_text(json.dumps(receipt_ok))
        exp_root = root / "oracle"
        (exp_root / "SanitizerExpectations.d").mkdir(parents=True)
        (exp_root / "SanitizerExpectations.d" / "daemon-ops.txt").write_text(
            "\n".join(e.key for e in lines) + "\n")
        (reports / UNATTRIBUTED / "report.3").unlink()
        out, races, verdict_file = root / "sanitize.json", root / "races.jsonl", root / "verdicts.jsonl"
        code = main(["daemon", "--run", str(run), "--reports", str(reports), "--build", "tsan", "--out", str(out),
                     "--races", str(races), "--verdicts", str(verdict_file), "--root", str(exp_root),
                     "--oracle-exit", "1"])
        doc = json.loads(out.read_text())
        check("cli green", code == 0 and doc["ok"] and doc["schema"] == RECEIPT_SCHEMA and doc["sanitizer"] == "thread")
        check("cli queue", [json.loads(line)["key"] for line in races.read_text().splitlines()] == [key])
        code = main(["daemon", "--run", str(run), "--reports", str(reports), "--build", "tsan", "--out", str(out),
                     "--races", str(races), "--root", str(exp_root), "--oracle-exit", "2"])
        check("an oracle infra exit is red", code == 1)

    for name in failures:
        print(f"FAIL {name}", file=sys.stderr)
    print(f"sanitizer_results selftest: {count - len(failures)}/{count} passed")
    return 1 if failures else 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    daemon = sub.add_parser("daemon", help="judge a daemon lane run (linux_run.sh sanitize)")
    daemon.add_argument("--run", type=Path, required=True, help="oracle.py run directory")
    daemon.add_argument("--reports", type=Path, required=True, help="REPORTS/<suite>/<case>/report.<pid>")
    daemon.add_argument("--build", choices=sorted(SANITIZER_OF_BUILD), required=True)
    daemon.add_argument("--out", type=Path, required=True, help="the lane receipt")
    daemon.add_argument("--races", type=Path, required=True, help="the MQ-races queue, JSON lines")
    daemon.add_argument("--verdicts", type=Path, help="verdict records for expectations.py lint --results")
    daemon.add_argument("--build-receipt", type=Path, help="linux_run.sh build.json of the binary")
    daemon.add_argument("--oracle-exit", type=int, default=0, help="oracle.py run's exit code")
    daemon.add_argument("--root", type=Path, default=oracle_exp.DEFAULT_ROOT)
    daemon.set_defaults(func=cmd_daemon)
    keys = sub.add_parser("keys", help="print the stack key of every report")
    keys.add_argument("paths", type=Path, nargs="+")
    keys.add_argument("--process", default="")
    keys.set_defaults(func=cmd_keys)
    sub.add_parser("selftest", help="run the self tests").set_defaults(func=lambda _args: _selftest())
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())

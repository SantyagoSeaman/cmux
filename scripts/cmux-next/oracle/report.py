#!/usr/bin/env python3
"""The oracle's report: one receipt.json and one index.md for a run, a comparison or a fleet run.

Spec: plans/cmux-next/chrome-parity/p15-oracle.md, sections 3G (outputs) and task 116. Stdlib only, Python
3.9+ (fleet minis run the system python3), safe under `python3 -I`. `oracle.py report ARGS` runs this file.

Usage:
    report.py DIR [--out OUT] [--corpus CORPUS] [--divergence FILE] [--limit N] [--json]

DIR is one of:
    a run dir         receipt.json (cmux-oracle-run-v1), events.jsonl, checkpoints.jsonl (`oracle.py run`,
                      or a merged run dir: its receipt's shard_runs name the shard dirs)
    a comparison dir  comparison.json (cmux-oracle-comparison-v2) and divergence.json (`oracle.py diff --out`)
    a fleet dir       fleet.json (cmux-oracle-fleet-v1): shards/<side>/<i>/{step.log, step.json, unpacked/run/},
                      <side>/ (the merged run dir; a one-shard side's is its shard's run dir), compare/
                      (`fleet.py run|diff --out`)
A path in fleet.json, comparison.json or divergence.json that is gone (the dir was copied from the host that
wrote it) is looked up at the same place under DIR instead: the record's own paths say where DIR was.

Outputs, in OUT (default DIR/report; never inside the checkout, spec 3G):
    receipt.json  {"schema": "cmux-oracle-report-v1", "source": {"kind", "path"}, "verdict", "counts",
                   "first_failure", "runs": [run], "failures": [finding], "comparison", "untested",
                   "artifacts": [artifact]}
    index.md      the same for a person: the first failure first, then every unexpected result, the
                   comparison, the flaky cases, what the run did not test and the artifacts.

A finding is one case of one run that did not end as expected, one divergence of the comparison, one case
that leaked state on one side of an order-parity comparison only ("order-leak"), a switch arm a run never
exercised ("arm"), a shard that did not run or merge ("shard"), a refused comparison ("refused") or an infra
error ("infra"): {"kind": "unexpected"|"leaked"|"divergence"|"order-leak"|"arm"|"unexpected-pass"|"not-run"|
"shard"|"refused"|"infra", "run", "case", "result", "expected", "step": {"id", "index", "op", "from"} | null, "message", "pointer":
{"pointer", "from", "a"?, "b"?} | null, "invariant", "artifacts", "replay"}. `step.from` says where the step
came from: "driver" (the driver named the step it failed in) or "last-checkpoint" (the last step the case
recorded a checkpoint for: the failure happened in it or after it). `pointer.from` is "divergence" (diff.py's
first differing pointer), "message" (the pointer the driver's message names), "leak" (the first pointer the
reset check found changed) or "invariant". An artifact is {"kind", "path", "line"?}: the path is relative
to DIR when it is under DIR, so a copied dir keeps its links; `line` is the 1-based line of a .jsonl file.

The first failure is the first finding in this order: unexpected results and leaks together in the run's case
order (the plan order, which a replay with the same seed repeats), then divergences in case order, order-parity
leaks, unexercised arms, unexpected passes, cases not run, then shard, refusal and infra errors. Each names the step, the pointer and the event and checkpoint lines,
so it is found without a video or a screenshot.

Untested: cases that did not run (skipped by Expectations, not run, planned in a dry run, not applicable to
the product), flaky cases (left out of every comparison), checkpoints with `notComparable` facets or marked
`uncertain`, a switch arm the run never exercised, the comparison's not-comparable cases, and each ran
case's corpus `untested` reasons (the states its steps do not check).

Exit codes: 0 the verdict is pass, 1 fail (unexpected results, leaks, divergences, order-parity leaks or an
unexercised arm), 2 bad input, 3 incomplete (a shard without a receipt or a side not merged, a refused or
crashed comparison, a fleet diff without a comparison result, a run with infra errors, nothing ran). A fail or
incomplete verdict from a source that ran cases always has a finding that says why.

Chrome reference (chromium/src):
- docs/testing/json_test_results_format.md: each test's `expected`, `actual`, `is_unexpected`, `is_flaky`
  and `artifacts`, and `num_failures_by_type`; the receipt keeps the same per-case fields.
- third_party/blink/tools/blinkpy/web_tests/views/buildbot_results.py: the summary lists the unexpected
  results first (regressions, then unexpected passes, then flakes), each with its result.
- third_party/blink/web_tests/fast/harness/results.html: one page with the unexpected failures first and a
  link to each test's artifacts (the actual, expected and diff outputs) next to it.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, Iterable, Iterator, List, Mapping, Optional, Sequence, Tuple

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import oracle  # noqa: E402  (the run format: RUN_SCHEMA, COMPARISON_SCHEMA, load_corpus, check_out_dir)

REPORT_SCHEMA = "cmux-oracle-report-v1"
FLEET_SCHEMA = "cmux-oracle-fleet-v1"
DIVERGENCE_SCHEMA = "oracle-divergence-v1"
RUN_FILES = frozenset({"receipt.json", "events.jsonl", "checkpoints.jsonl", "index.md"})
DEFAULT_LIMIT = 50
MESSAGE_CHARS = 300
# Run-level artifacts listed per run (app logs, step logs); the receipt says how many more there are.
ARTIFACTS_PER_RUN = 200
ARTIFACT_DEPTH = 4

EXIT_PASS, EXIT_FAIL, EXIT_BAD_INPUT, EXIT_INCOMPLETE = 0, 1, 2, 3

# The pointer a driver's failure message names: driver_daemon.py and driver_app.py write "result differs at
# /p", "waitFor /p not met", "the reply of s has nothing at /p"; an invariant names "pointer /p".
MESSAGE_POINTER = re.compile(r"(?:differs at|waitFor|nothing at|pointer)\s+(/[^\s,;]*|/)")
MESSAGE_INVARIANT = re.compile(r"(?:^|[;(]\s*)(I\d{1,2}):")
# Finding kinds in first-failure order; unexpected results and leaks share a rank, so they keep the case order.
KIND_ORDER = ("unexpected", "leaked", "divergence", "order-leak", "arm", "unexpected-pass", "not-run", "shard",
              "refused", "infra")
SAME_RANK = {"leaked": "unexpected"}
FAIL_KINDS = frozenset({"unexpected", "leaked", "divergence", "order-leak", "arm"})
INCOMPLETE_KINDS = frozenset({"not-run", "shard", "refused", "infra"})


def kind_rank(kind: str) -> int:
    return KIND_ORDER.index(SAME_RANK.get(kind, kind))


class ReportError(Exception):
    """Bad input: a DIR that is none of the three kinds, or an unreadable file in it (exit 2)."""


# ---------------------------------------------------------------------------------------------------------------
# Reading the run files


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise ReportError(f"{path}: not readable JSON ({error})") from None


def jsonl(path: Path) -> Iterator[Tuple[int, Dict[str, Any]]]:
    """(1-based line, row) of each object line; a missing file has none, a bad line is bad input."""
    if not path.is_file():
        return
    with path.open(encoding="utf-8") as handle:
        for number, line in enumerate(handle, 1):
            if not line.strip():
                continue
            try:
                row = json.loads(line)
            except ValueError as error:
                raise ReportError(f"{path}:{number}: not JSON ({error})") from None
            if isinstance(row, dict):
                yield number, row


@dataclass(frozen=True)
class Event:
    line: int
    iteration: int
    attempt: int
    status: Optional[str]
    step: Optional[str]
    message: str
    session: Optional[int]


@dataclass(frozen=True)
class CheckpointRow:
    line: int
    step: str
    index: Optional[int]
    checkpoint: Optional[str]
    uncertain: bool
    not_comparable: Tuple[str, ...]


@dataclass
class Run:
    """One run dir: its receipt and the rows of its two .jsonl files by case."""

    label: str
    dir: Path
    receipt: Dict[str, Any]
    events: Dict[str, List[Event]] = field(default_factory=dict)
    checkpoints: Dict[str, List[CheckpointRow]] = field(default_factory=dict)
    # case -> the shard dir that ran it (a merged run), and that shard's fleet step log when there is one.
    shard_of: Dict[str, Path] = field(default_factory=dict)
    step_logs: Dict[Path, Path] = field(default_factory=dict)

    @classmethod
    def load(cls, label: str, directory: Path) -> "Run":
        try:
            receipt = oracle.read_receipt(directory)
        except oracle.UsageError as error:
            raise ReportError(str(error)) from None
        run = cls(label, directory, receipt)
        for number, row in jsonl(directory / "events.jsonl"):
            run.events.setdefault(str(row.get("case")), []).append(Event(
                number, int(row.get("iteration") or 0), int(row.get("attempt") or 0), row.get("status"),
                row.get("step") if isinstance(row.get("step"), str) else None, str(row.get("message") or ""),
                row.get("session") if isinstance(row.get("session"), int) else None))
        for number, row in jsonl(directory / "checkpoints.jsonl"):
            index = row.get("index")
            run.checkpoints.setdefault(str(row.get("case")), []).append(CheckpointRow(
                number, str(row.get("step")), index if isinstance(index, int) else None, row.get("checkpoint"),
                bool(row.get("uncertain")), tuple(str(f) for f in row.get("notComparable") or ())))
        return run

    def attach_shards(self, shard_dirs: Sequence[Path], step_logs: Mapping[Path, Path]) -> None:
        """Maps each case to the shard dir whose receipt lists it (a merged run)."""
        for shard in shard_dirs:
            receipt = shard / "receipt.json"
            if not receipt.is_file():
                continue
            for cid in (read_json(receipt).get("cases") or {}):
                self.shard_of[cid] = shard
            if shard in step_logs:
                self.step_logs[shard] = step_logs[shard]

    def first_event(self, cid: str) -> Optional[Event]:
        events = [e for e in self.events.get(cid, ()) if e.iteration == 0]
        return min(events, key=lambda e: e.attempt) if events else None


# ---------------------------------------------------------------------------------------------------------------
# Sources: a run dir, a comparison dir, a fleet dir


def existing(*candidates: Optional[Path]) -> Optional[Path]:
    return next((c for c in candidates if c is not None and c.exists()), None)


def as_path(value: Any) -> Optional[Path]:
    return Path(value) if isinstance(value, str) and value else None


def origin_of(value: Any, tail: str) -> Optional[Path]:
    """Where a dir was when a record wrote `value`, a path that ends with `tail` in that dir's layout."""
    path = as_path(value)
    parts = Path(tail).parts
    if path is None or len(path.parts) <= len(parts) or path.parts[-len(parts):] != parts:
        return None
    return Path(*path.parts[: -len(parts)])


@dataclass
class Comparison:
    dir: Path
    record: Dict[str, Any]
    divergence: Optional[Dict[str, Any]]
    divergence_path: Optional[Path]

    @property
    def origin(self) -> Optional[Path]:
        """Where the comparison dir was when `oracle.py diff --out` wrote it."""
        return origin_of(self.record.get("divergence"), "divergence.json")


@dataclass
class Source:
    kind: str
    dir: Path
    runs: List[Run] = field(default_factory=list)
    comparison: Optional[Comparison] = None
    # Fleet steps without a merged run: {"run", "shard", "state", "error", "step_log"}.
    shard_errors: List[Dict[str, Any]] = field(default_factory=list)
    # Fleet sides fleet.py did not merge, one per error it recorded: {"run", "error"}.
    side_errors: List[Dict[str, Any]] = field(default_factory=list)
    fleet: Optional[Dict[str, Any]] = None
    # (where a dir was when a record named its paths, where that dir is now).
    moves: List[Tuple[Path, Path]] = field(default_factory=list)

    def locate(self, value: Any) -> Optional[Path]:
        """A recorded path, or the same place under the dir it moved to when it is gone (a copied dir)."""
        path = as_path(value)
        if path is None or path.exists():
            return path
        for origin, now in self.moves:
            try:
                moved = now / path.relative_to(origin)
            except ValueError:
                continue
            if moved.exists():
                return moved
        return path

    def moved(self, origin: Optional[Path], now: Path) -> None:
        if origin is not None and origin != now and (origin, now) not in self.moves:
            self.moves.append((origin, now))


def merged_run(label: str, directory: Path, source: Source, fallback_shards: Sequence[Path] = (),
               step_logs: Optional[Mapping[Path, Path]] = None) -> Run:
    """A run dir. A merged run's cases map to the shard dirs its receipt names, or to the fallback shards when
    those are gone; a one-shard fleet side's run dir is its own shard."""
    run = Run.load(label, directory)
    shard_dirs = [existing(source.locate(s.get("dir"))) for s in run.receipt.get("shard_runs") or ()]
    found = [d for d in shard_dirs if d is not None]
    run.attach_shards(found or list(fallback_shards), step_logs or {})
    return run


def load_comparison(directory: Path, extra: Optional[Path]) -> Comparison:
    record_path = directory / "comparison.json"
    record = read_json(record_path) if record_path.is_file() else {}
    if record and record.get("schema") != oracle.COMPARISON_SCHEMA:
        raise ReportError(f"{record_path}: not a {oracle.COMPARISON_SCHEMA} record (written by `oracle.py diff`)")
    path = existing(extra, as_path(record.get("divergence")), directory / "divergence.json")
    divergence = read_json(path) if path else None
    if divergence is not None and divergence.get("schema") != DIVERGENCE_SCHEMA:
        raise ReportError(f"{path}: not an {DIVERGENCE_SCHEMA} document (written by diff.py)")
    return Comparison(directory, record, divergence, path)


def comparison_runs(source: Source, comparison: Comparison) -> List[Run]:
    """The two sides diff.py compared, when their dirs are still there or were copied with the comparison dir
    (`oracle.py diff` writes a merged side to DIR/a-merged and DIR/b-merged)."""
    runs = []
    labels = comparison.record.get("a"), comparison.record.get("b")
    for side, label in zip(("a", "b"), labels):
        header = (comparison.divergence or {}).get(side) or {}
        path = existing(source.locate(header.get("path")), comparison.dir / f"{side}-merged")
        if path is not None and (path / "receipt.json").is_file():
            runs.append(merged_run(str(label or side), path, source))
    return runs


def attach_comparison(source: Source, directory: Path, extra: Optional[Path]) -> Comparison:
    comparison = load_comparison(directory, extra)
    source.moved(comparison.origin, comparison.dir)
    source.comparison = comparison
    return comparison


def fleet_origins(record: Mapping[str, Any]) -> List[Path]:
    """Where the fleet dir was when fleet.py wrote fleet.json, from each absolute path it records."""
    origins = [origin_of((record.get("comparison") or {}).get("dir"), "compare")]
    for label, side in (record.get("sides") or {}).items():
        origins.append(origin_of(side.get("merged"), f"{label}/receipt.json"))
        for shard in side.get("shards") or ():
            for layout in ("unpacked/run", "run"):
                origins.append(origin_of(shard.get("run"), f"shards/{label}/{shard.get('index')}/{layout}"))
    return [o for o in dict.fromkeys(origins) if o is not None]


def load_fleet(directory: Path, extra_divergence: Optional[Path]) -> Source:
    record = read_json(directory / "fleet.json")
    if record.get("schema") != FLEET_SCHEMA:
        raise ReportError(f"{directory}/fleet.json: not a {FLEET_SCHEMA} record (written by fleet.py)")
    source = Source("fleet", directory, fleet=record)
    for origin in fleet_origins(record):
        source.moved(origin, directory)
    for label, side in sorted((record.get("sides") or {}).items()):
        shards = list(side.get("shards") or ())
        shard_runs: List[Tuple[Any, Path]] = []
        step_logs: Dict[Path, Path] = {}
        for shard in shards:
            index = shard.get("index")
            local = directory / "shards" / label / str(index)
            # fleet.py unpacks a shard's artifact to shards/<side>/<i>/unpacked/run.
            run_dir = existing(source.locate(shard.get("run")), local / "unpacked" / "run", local / "run")
            log = existing(local / "step.log")
            if run_dir is not None and (run_dir / "receipt.json").is_file():
                shard_runs.append((index, run_dir))
                if log is not None:
                    step_logs[run_dir] = log
            else:
                source.shard_errors.append({
                    "run": label, "shard": f"{index}/{shard.get('total')}", "state": shard.get("state"),
                    "error": shard.get("error") or "no shard receipt", "step_id": shard.get("step_id"),
                    "worker": shard.get("worker"), "step_log": log})
        shard_dirs = [run_dir for _, run_dir in shard_runs]
        merged = existing(source.locate(side.get("merged")), directory / label / "receipt.json")
        if merged is None and len(shards) == 1 and shards[0].get("total") == 1 and shard_dirs:
            # fleet.py's merged run dir of a one-shard side is that shard's run dir.
            merged = shard_dirs[0] / "receipt.json"
        if merged is not None:
            source.runs.append(merged_run(label, merged.parent, source, shard_dirs, step_logs))
            continue
        for error in side.get("errors") or ():
            # fleet.py names a shard without a receipt "shard i/N: ..."; that shard is a shard error already.
            if not any(e["run"] == label and str(error).startswith(f"shard {e['shard']}:") for e in source.shard_errors):
                source.side_errors.append({"run": label, "error": str(error)})
        for index, run_dir in shard_runs:
            run = Run.load(f"{label} shard {index}", run_dir)
            run.attach_shards([run_dir], step_logs)
            source.runs.append(run)
    compare = (record.get("comparison") or {})
    compare_dir = existing(source.locate(compare.get("dir")), directory / "compare")
    if compare_dir is not None or extra_divergence is not None:
        attach_comparison(source, compare_dir or directory, extra_divergence)
    return source


def load_source(directory: Path, divergence: Optional[Path] = None) -> Source:
    if not directory.is_dir():
        raise ReportError(f"{directory}: not a directory")
    if (directory / "fleet.json").is_file():
        return load_fleet(directory, divergence)
    if (directory / "receipt.json").is_file():
        source = Source("run", directory)
        source.runs = [merged_run("run", directory, source)]
        if divergence is not None:
            attach_comparison(source, directory, divergence)
        return source
    if (directory / "comparison.json").is_file() or (directory / "divergence.json").is_file():
        source = Source("comparison", directory)
        source.runs = comparison_runs(source, attach_comparison(source, directory, divergence))
        return source
    raise ReportError(f"{directory}: no receipt.json, comparison.json, divergence.json or fleet.json "
                      "(not a run, comparison or fleet dir)")


# ---------------------------------------------------------------------------------------------------------------
# Findings


@dataclass
class Corpus:
    cases: Dict[str, Dict[str, Any]]
    path: Path

    @classmethod
    def load(cls, root: Path) -> "Corpus":
        cases, _ = oracle.load_corpus(root)
        return cls({c.id: c.data for c in cases}, root)

    def step(self, cid: str, step_id: Optional[str]) -> Tuple[Optional[int], Optional[str]]:
        """(0-based index, op) of a step of a case, or (None, None) when the corpus has no such step."""
        for index, step in enumerate((self.cases.get(cid) or {}).get("steps") or ()):
            if isinstance(step, dict) and step.get("id") == step_id:
                return index, step.get("op")
        return None, None

    def untested(self, cid: str) -> List[str]:
        return [str(r) for r in (self.cases.get(cid) or {}).get("untested") or ()]


class Paths:
    """Artifact paths relative to the source dir when they are under it."""

    def __init__(self, root: Path) -> None:
        self.root = root.resolve()

    def rel(self, path: Path) -> str:
        resolved = path.resolve()
        try:
            return resolved.relative_to(self.root).as_posix()
        except ValueError:
            return str(resolved)

    def artifact(self, kind: str, path: Path, line: Optional[int] = None) -> Dict[str, Any]:
        out: Dict[str, Any] = {"kind": kind, "path": self.rel(path)}
        if line is not None:
            out["line"] = line
        return out


def message_pointer(message: str) -> Optional[str]:
    match = MESSAGE_POINTER.search(message)
    if match is None:
        return None
    return match.group(1).rstrip(":).") or "/"


def message_invariant(message: str) -> Optional[str]:
    match = MESSAGE_INVARIANT.search(message)
    return match.group(1) if match else None


def replay_command(run: Run, cid: str) -> str:
    """The command that runs this one case again with the run's config (a filtered run: no retries)."""
    receipt = run.receipt
    config = receipt.get("config") or {}
    product = receipt.get("product") or config.get("product")
    sha = receipt.get("sha") or config.get("ref") or ""
    switch, arm = config.get("switch"), config.get("arm")
    if product == "app":
        argv = ["python3", "scripts/cmux-next/oracle/fleet.py", "run", "--ref", sha or "SHA", "--filter", cid,
                "--config", str(config.get("build") or "bench")]
        return shlex.join(argv)
    run_args = ["--product", str(product), "--filter", cid]
    if product in oracle.RUST_PRODUCTS and re.fullmatch(r"[0-9a-f]{40}", sha):
        argv = ["bash", "scripts/cmux-next/oracle/linux_run.sh", "run", sha]
        if switch and arm:
            argv += ["--arm", f"{switch}={arm}"]
        elif arm:
            argv += ["--arm", str(arm)]
        return shlex.join(argv + ["--", *run_args])
    argv = ["python3", "scripts/cmux-next/oracle/oracle.py", "run", *run_args,
            "--platform", str(receipt.get("platform") or config.get("platform")), "--build", str(config.get("build"))]
    if switch:
        argv += ["--switch", str(switch)]
    if arm:
        argv += ["--arm", str(arm)]
    if config.get("binary"):
        argv += ["--binary", str(config["binary"])]
    if sha:
        argv += ["--ref", sha]
    return shlex.join(argv)


def case_kind(record: Mapping[str, Any]) -> Optional[str]:
    """The finding kind of a case record, or None for a case that ended as expected."""
    outcome = record.get("outcome")
    if outcome == "unexpected":
        return "unexpected"
    if record.get("leaked_state"):
        return "leaked"
    if outcome in ("unexpected-pass", "not-run"):
        return outcome
    return None


@dataclass
class Builder:
    source: Source
    corpus: Corpus
    paths: Paths
    divergent: Dict[str, Dict[str, Any]] = field(default_factory=dict)

    def __post_init__(self) -> None:
        document = (self.source.comparison.divergence if self.source.comparison else None) or {}
        for item in document.get("divergences") or ():
            self.divergent.setdefault(str(item.get("journey")), item)

    def step_of(self, run: Run, cid: str, event: Optional[Event]) -> Optional[Dict[str, Any]]:
        step, origin = (event.step, "driver") if event is not None and event.step else (None, None)
        if step is None:
            rows = run.checkpoints.get(cid, ())
            if rows:
                step, origin = rows[-1].step, "last-checkpoint"
        if step is None:
            return None
        index, op = self.corpus.step(cid, step)
        return {"id": step, "index": index, "op": op, "from": origin}

    def pointer_of(self, cid: str, record: Mapping[str, Any], message: str) -> Optional[Dict[str, Any]]:
        item = self.divergent.get(cid)
        if item is not None and item.get("pointer") is not None:
            return {"pointer": item["pointer"], "from": "divergence", "a": item.get("a"), "b": item.get("b")}
        pointer = message_pointer(message)
        if pointer is not None:
            return {"pointer": pointer, "from": "invariant" if message_invariant(message) else "message"}
        leaks = record.get("leaked_state") or ()
        if leaks and (leaks[0].get("pointers") or ()):
            return {"pointer": leaks[0]["pointers"][0], "from": "leak"}
        return None

    def case_artifacts(self, run: Run, cid: str, event: Optional[Event], step: Optional[Dict[str, Any]]) -> List[Dict[str, Any]]:
        out = []
        if event is not None:
            out.append(self.paths.artifact("event", run.dir / "events.jsonl", event.line))
        rows = run.checkpoints.get(cid, ())
        at_step = [r for r in rows if step is not None and r.step == step["id"]] or list(rows[-1:])
        if at_step:
            out.append(self.paths.artifact("checkpoint", run.dir / "checkpoints.jsonl", at_step[-1].line))
        shard = run.shard_of.get(cid)
        if shard is not None:
            out.append(self.paths.artifact("shard-run", shard))
            if shard in run.step_logs:
                out.append(self.paths.artifact("step-log", run.step_logs[shard]))
        item = self.divergent.get(cid)
        if item is not None and self.source.comparison and self.source.comparison.divergence_path:
            out.append(self.paths.artifact("divergence", self.source.comparison.divergence_path))
        return out

    def case_finding(self, run: Run, cid: str, record: Mapping[str, Any], kind: str) -> Dict[str, Any]:
        event = run.first_event(cid)
        message = event.message if event is not None else ""
        leaks = record.get("leaked_state") or ()
        if kind == "leaked" and leaks:
            message = oracle.leak_line(leaks[0])
        step = self.step_of(run, cid, event) if kind != "leaked" else None
        return {
            "kind": kind, "run": run.label, "case": cid, "result": record.get("status"),
            "expected": list(record.get("expected") or ()), "attempts": record.get("attempts"),
            "step": step, "message": message, "pointer": self.pointer_of(cid, record, message),
            "invariant": message_invariant(message), "artifacts": self.case_artifacts(run, cid, event, step),
            "replay": replay_command(run, cid),
        }

    def run_findings(self, run: Run) -> List[Dict[str, Any]]:
        cases = run.receipt.get("cases") or {}
        order = [cid for cid in run.receipt.get("order") or () if cid in cases]
        order += sorted(set(cases) - set(order))
        findings = []
        for cid in order:
            kind = case_kind(cases[cid])
            if kind is not None:
                findings.append(self.case_finding(run, cid, cases[cid], kind))
        receipt_artifact = [self.paths.artifact("receipt", run.dir / "receipt.json")]
        if run.receipt.get("arm_exercised") is False:
            config = run.receipt.get("config") or {}
            switch, arm = config.get("switch"), config.get("arm")
            findings.append({"kind": "arm", "run": run.label, "case": None, "switch": switch, "arm": arm,
                             "message": f"switch {switch} arm {arm} never ran: no case hit it, so the run did not "
                                        "test the arm (receipt switch_hits)",
                             "artifacts": receipt_artifact})
        for error in run.receipt.get("infra_errors") or ():
            findings.append({"kind": "infra", "run": run.label, "case": None, "message": str(error),
                             "artifacts": receipt_artifact})
        return findings

    def divergence_findings(self) -> List[Dict[str, Any]]:
        comparison = self.source.comparison
        if comparison is None:
            return []
        out = []
        cases = {row.get("case"): row for row in (comparison.divergence or {}).get("cases") or ()}
        for cid, item in sorted(self.divergent.items()):
            row = cases.get(cid) or {}
            artifacts = [self.paths.artifact("divergence", comparison.divergence_path)] if comparison.divergence_path else []
            for evidence in item.get("evidence") or ():
                path, _, line = str(evidence).rpartition("#")
                located = self.source.locate(path) or Path(path)
                artifacts.append(self.paths.artifact("checkpoint", located, int(line) if line.isdigit() else None))
            for run in self.source.runs:
                shard = run.shard_of.get(cid)
                if shard is not None and shard in run.step_logs:
                    artifacts.append(self.paths.artifact("step-log", run.step_logs[shard]))
            index, op = self.corpus.step(cid, item.get("step"))
            step = {"id": item.get("step"), "index": index, "op": op, "from": "divergence"} if item.get("step") else None
            out.append({
                "kind": "divergence", "run": "comparison", "case": cid, "result": row.get("result"),
                "checkpoint": item.get("checkpoint"), "step": step, "message": item.get("observed") or "",
                "pointer": {"pointer": item.get("pointer"), "from": "divergence", "a": item.get("a"), "b": item.get("b")},
                "count": item.get("count"), "invariant": None, "artifacts": artifacts, "replay": item.get("replay"),
            })
        return out

    def shard_findings(self) -> List[Dict[str, Any]]:
        out = []
        for shard in self.source.shard_errors:
            log = shard.get("step_log")
            out.append({"kind": "shard", "run": shard["run"], "case": None, "shard": shard["shard"],
                        "message": f"shard {shard['shard']} {shard.get('state')}: {shard.get('error')}"
                                   + (f" (step {shard['step_id']} on {shard.get('worker') or 'no worker'})"
                                      if shard.get("step_id") else ""),
                        "artifacts": [self.paths.artifact("step-log", log)] if log else []})
        fleet_json = [self.paths.artifact("fleet", self.source.dir / "fleet.json")] if self.source.fleet else []
        for side in self.source.side_errors:
            out.append({"kind": "shard", "run": side["run"], "case": None,
                        "message": f"{side['run']} not merged: {side['error']}", "artifacts": fleet_json})
        return out

    def comparison_findings(self) -> List[Dict[str, Any]]:
        """What keeps a comparison from passing besides its divergences: a refusal, a diff.py error, an
        order-parity leak, or no result at all."""
        comparison = self.source.comparison
        if comparison is None:
            return []
        record, document = comparison.record, comparison.divergence or {}
        record_artifact = [self.paths.artifact("comparison", comparison.dir / "comparison.json")] if record else []
        document_artifact = ([self.paths.artifact("divergence", comparison.divergence_path)]
                             if comparison.divergence_path else [])

        def finding(kind: str, message: str, artifacts: List[Dict[str, Any]]) -> Dict[str, Any]:
            return {"kind": kind, "run": "comparison", "case": None, "message": message, "artifacts": artifacts}

        if not record:
            # diff.py's own document; its verdict is pass, divergent, host-mismatch, bad-input or internal-error.
            state = document.get("verdict")
            if not document:
                return [finding("infra", f"{self.paths.rel(comparison.dir)} has no comparison.json and no "
                                         "divergence.json: the comparison wrote no result", [])]
            if state in ("pass", "divergent"):
                return []
            if state == "host-mismatch":
                hosts = "; ".join(f"{case}: {', '.join(problems)}"
                                  for case, problems in sorted((document.get("mismatched") or {}).items()))
                return [finding("refused", f"comparison refused: host-mismatch: {hosts}", document_artifact)]
            return [finding("infra", f"diff.py {state or 'wrote no verdict'}: {document.get('error') or 'no error named'}",
                            document_artifact)]
        out = []
        if record.get("refused"):
            out.append(finding("refused", f"comparison refused: {record['refused']}", record_artifact))
        elif record.get("diff_exit") not in (oracle.DIFF_OK, oracle.DIFF_DIVERGENT):
            out.append(finding("infra", f"diff.py exited {record.get('diff_exit')}: "
                                        f"{document.get('error') or 'no comparison result'}",
                               record_artifact + document_artifact))
        leaks = record.get("leaked_state") or {}
        for cid in record.get("leaks_differ") or ():
            side = "a" if cid in (leaks.get("a") or ()) else "b"
            label = str(record.get(side) or side)
            run = next((r for r in self.source.runs if r.label == label and cid in (r.receipt.get("cases") or {})), None)
            case = (run.receipt.get("cases") or {}).get(cid) or {} if run is not None else {}
            out.append({
                "kind": "order-leak", "run": "comparison", "case": cid, "side": side, "result": None,
                "step": None, "invariant": None, "pointer": self.pointer_of(cid, case, ""),
                "message": f"LeakedState on {label} only: the leak depends on the case order or the launch sharing",
                "artifacts": record_artifact + (self.case_artifacts(run, cid, run.first_event(cid), None) if run else []),
                "replay": replay_command(run, cid) if run else None,
            })
        if not record.get("ok") and not out and not self.divergent:
            out.append(finding("infra", "comparison.json says ok false and names no refusal, divergence or leak",
                               record_artifact))
        return out

    def fleet_findings(self, found: Sequence[Dict[str, Any]]) -> List[Dict[str, Any]]:
        """A fleet diff without a comparison result, and a fleet run fleet.py did not pass that no other
        finding explains."""
        fleet = self.source.fleet
        if fleet is None:
            return []
        fleet_json = [self.paths.artifact("fleet", self.source.dir / "fleet.json")]
        out: List[Dict[str, Any]] = []
        compare = fleet.get("comparison")
        if compare is not None and compare.get("exit") not in (oracle.DIFF_OK, oracle.DIFF_DIVERGENT):
            code = compare.get("exit")
            explaining = [f for f in found if f["run"] == "comparison" and f["kind"] in INCOMPLETE_KINDS]
            for item in explaining:
                item["message"] += f" (the fleet diff exited {code})"
            if not explaining:
                out.append({"kind": "infra", "run": "comparison", "case": None, "artifacts": fleet_json,
                            "message": f"the fleet diff exited {code} without a comparison result"})
        elif compare is not None and self.source.comparison is None:
            out.append({"kind": "infra", "run": "comparison", "case": None, "artifacts": fleet_json,
                        "message": f"the fleet diff exited {compare.get('exit')} and its comparison dir is gone"})
        if fleet.get("ok") is False and not any(f["kind"] in FAIL_KINDS | INCOMPLETE_KINDS for f in [*found, *out]):
            out.append({"kind": "infra", "run": "fleet", "case": None, "artifacts": fleet_json,
                        "message": "fleet.json says ok false and no shard, case or comparison finding explains it"})
        return out

    def findings(self) -> List[Dict[str, Any]]:
        found = [f for run in self.source.runs for f in self.run_findings(run)]
        found += self.divergence_findings() + self.comparison_findings() + self.shard_findings()
        found += self.fleet_findings(found)
        # Stable sort: the run's case order stays inside each rank.
        return sorted(found, key=lambda f: kind_rank(f["kind"]))


# ---------------------------------------------------------------------------------------------------------------
# Untested states, artifacts and the verdict


def untested(source: Source, corpus: Corpus) -> Dict[str, Any]:
    out: Dict[str, Any] = {"skipped": [], "not_run": [], "planned": [], "flaky": [], "not_applicable": {},
                           "not_comparable": [], "uncertain": [], "arm_not_exercised": [], "case_reasons": [],
                           "comparison": {}}
    reasons: Dict[str, List[str]] = {}
    for run in source.runs:
        receipt = run.receipt
        out["not_applicable"][run.label] = int(receipt.get("not_applicable") or 0)
        for cid, record in sorted((receipt.get("cases") or {}).items()):
            outcome = record.get("outcome")
            key = {"skipped": "skipped", "not-run": "not_run", "planned": "planned"}.get(outcome)
            if key:
                out[key].append({"run": run.label, "case": cid})
            if record.get("flaky"):
                out["flaky"].append({"run": run.label, "case": cid, "reasons": list(record.get("reasons") or ())})
            if record.get("attempts") and corpus.untested(cid):
                reasons.setdefault(cid, corpus.untested(cid))
        for cid, rows in sorted(run.checkpoints.items()):
            for row in rows:
                where = {"run": run.label, "case": cid, "step": row.step, "checkpoint": row.checkpoint, "line": row.line}
                if row.not_comparable:
                    out["not_comparable"].append({**where, "facets": list(row.not_comparable)})
                if row.uncertain:
                    out["uncertain"].append(where)
        if receipt.get("arm_exercised") is False:
            config = receipt.get("config") or {}
            out["arm_not_exercised"].append({"run": run.label, "switch": config.get("switch"), "arm": config.get("arm")})
    out["case_reasons"] = [{"case": cid, "reasons": r} for cid, r in sorted(reasons.items())]
    record = source.comparison.record if source.comparison else {}
    if record:
        out["comparison"] = {"flaky_excluded": list(record.get("flaky_excluded") or ()),
                             "not_comparable": dict(record.get("not_comparable") or {}),
                             "partial": bool(record.get("partial")),
                             "skipped": int((record.get("summary") or {}).get("skipped") or 0)}
    return out


def run_artifacts(source: Source, paths: Paths) -> List[Dict[str, Any]]:
    """Files beyond the run format: app logs under run/app/, fleet step logs and job records."""
    seen = set()
    out: List[Dict[str, Any]] = []
    roots = [run.dir for run in source.runs] + [d for run in source.runs for d in set(run.shard_of.values())]
    if source.kind == "fleet":
        roots.append(source.dir / "shards")
    run_dirs = set(roots)
    for root in roots:
        listed = 0
        for path in walk(root, ARTIFACT_DEPTH):
            if path in seen or (path.name in RUN_FILES and path.parent in run_dirs):
                continue
            seen.add(path)
            if listed == ARTIFACTS_PER_RUN:
                out.append({"kind": "more", "path": paths.rel(root), "note": f"more files under {paths.rel(root)}"})
                break
            listed += 1
            out.append({**paths.artifact(artifact_kind(path), path), "bytes": path.stat().st_size})
    return out


def walk(root: Path, depth: int) -> Iterator[Path]:
    if depth < 0 or not root.is_dir():
        return
    for entry in sorted(root.iterdir()):
        if entry.is_symlink():
            continue
        if entry.is_dir():
            yield from walk(entry, depth - 1)
        elif entry.is_file():
            yield entry


def artifact_kind(path: Path) -> str:
    if path.name == "step.log":
        return "step-log"
    if path.name in ("step.json", "job.json", "fleet-step.json"):
        return "step-record"
    if path.suffix == ".log":
        return "log"
    if path.name in RUN_FILES:
        return "run-file"
    return "file"


def verdict(source: Source, findings: Sequence[Mapping[str, Any]]) -> str:
    """fail and incomplete follow from the findings, so the report names their cause; a source that ran no
    case (no run, a dry run, an empty selection) is incomplete with its cases under Untested."""
    kinds = {f["kind"] for f in findings}
    if kinds & FAIL_KINDS:
        return "fail"
    if kinds & INCOMPLETE_KINDS or not source.runs and source.comparison is None:
        return "incomplete"
    if any(not (run.receipt.get("cases") or {}) or run.receipt.get("dry_run") for run in source.runs):
        return "incomplete"
    return "pass"


def run_summary(run: Run, paths: Paths) -> Dict[str, Any]:
    receipt = run.receipt
    return {
        "label": run.label, "path": paths.rel(run.dir), "runId": receipt.get("runId"), "sha": receipt.get("sha"),
        "product": receipt.get("product"), "platform": receipt.get("platform"),
        "build": (receipt.get("config") or {}).get("build"), "arm": receipt.get("arm"),
        "host": receipt.get("host"), "dry_run": bool(receipt.get("dry_run")), "ok": receipt.get("ok"),
        "cases": len(receipt.get("cases") or {}), "counts": dict(receipt.get("counts") or {}),
        "shards": list(receipt.get("shards") or ()), "flaky": len(receipt.get("flaky") or ()),
        "leaked_state": len(receipt.get("leaked_state") or ()), "infra_errors": len(receipt.get("infra_errors") or ()),
    }


def build_report(source: Source, corpus: Corpus) -> Dict[str, Any]:
    paths = Paths(source.dir)
    builder = Builder(source, corpus, paths)
    findings = builder.findings()
    counts: Dict[str, int] = {}
    for finding in findings:
        counts[finding["kind"]] = counts.get(finding["kind"], 0) + 1
    comparison = None
    if source.comparison is not None:
        record = source.comparison.record
        divergence = source.comparison.divergence or {}
        comparison = {"path": paths.rel(source.comparison.dir), "verdict": divergence.get("verdict"),
                      "summary": divergence.get("summary") or record.get("summary") or {},
                      "labels": [record.get("a"), record.get("b")], "refused": record.get("refused"),
                      "ok": record.get("ok"), "explained": list(record.get("explained") or ()),
                      "divergence": paths.rel(source.comparison.divergence_path) if source.comparison.divergence_path else None}
    return {
        "schema": REPORT_SCHEMA,
        "created": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "source": {"kind": source.kind, "path": str(source.dir.resolve()),
                   "fleet_ok": (source.fleet or {}).get("ok") if source.fleet else None},
        "corpus": str(corpus.path),
        "verdict": verdict(source, findings),
        "counts": counts,
        "first_failure": findings[0] if findings else None,
        "runs": [run_summary(run, paths) for run in source.runs],
        "failures": findings,
        "comparison": comparison,
        "untested": untested(source, corpus),
        "artifacts": run_artifacts(source, paths),
    }


# ---------------------------------------------------------------------------------------------------------------
# index.md


def cell(value: Any, limit: int = MESSAGE_CHARS) -> str:
    text = value if isinstance(value, str) else json.dumps(value, sort_keys=True)
    text = " ".join(text.split())
    if len(text) > limit:
        text = text[: limit - 3] + "..."
    return text.replace("|", "\\|").replace("`", "'")


def href(path: str, base: Path, root: Path) -> str:
    """A Markdown link target from index.md's dir to an artifact path (relative to the source dir)."""
    target = Path(path) if Path(path).is_absolute() else root / path
    return os.path.relpath(target, base).replace(" ", "%20")


def link(artifact: Mapping[str, Any], base: Path, root: Path) -> str:
    label = artifact["path"] + (f" line {artifact['line']}" if artifact.get("line") else "")
    return f"[{artifact['kind']}: {label}]({href(artifact['path'], base, root)})"


def step_text(step: Optional[Mapping[str, Any]]) -> str:
    if not step:
        return "no step (the driver named none and the case recorded no checkpoint)"
    where = f"#{step['index']}" if step.get("index") is not None else "not in the corpus"
    op = f", {step['op']}" if step.get("op") else ""
    after = " (the last step with a checkpoint: the failure is in it or after it)" if step.get("from") == "last-checkpoint" else ""
    return f"`{step['id']}` ({where}{op}){after}"


def pointer_text(pointer: Optional[Mapping[str, Any]]) -> str:
    if not pointer:
        return "none named"
    shown = pointer["pointer"] or '""'
    text = f"`{shown}` (from the {pointer['from']})"
    if "a" in pointer or "b" in pointer:
        base, head = (cell(json.dumps(pointer.get(side), sort_keys=True), 120) for side in ("a", "b"))
        text += f": base `{base}`, head `{head}`"
    return text


def finding_lines(finding: Mapping[str, Any], base: Path, root: Path) -> List[str]:
    who = f"case `{finding['case']}`" if finding.get("case") else finding["kind"]
    lines = [f"- {who} in {finding['run']}: **{finding['kind']}**"
             + (f", result {finding.get('result')} (expected {', '.join(finding.get('expected') or ()) or 'Pass'})"
                if finding.get("result") is not None and finding["kind"] != "divergence" else "")]
    if finding.get("case"):
        leak = finding["kind"] in ("leaked", "order-leak") and not finding.get("step")
        lines.append("- step: none (the reset check after the case found the leak)" if leak else
                     f"- step: {step_text(finding.get('step'))}"
                     + (f", checkpoint {finding['checkpoint']}" if finding.get("checkpoint") else ""))
        lines.append(f"- pointer: {pointer_text(finding.get('pointer'))}")
    if finding.get("invariant"):
        lines.append(f"- invariant: {finding['invariant']} (`python3 scripts/cmux-next/oracle/invariants.py --list`)")
    lines.append(f"- message: {cell(finding.get('message') or 'none')}")
    if finding.get("artifacts"):
        lines.append("- evidence: " + ", ".join(link(a, base, root) for a in finding["artifacts"]))
    if finding.get("replay"):
        lines.append(f"- replay: `{finding['replay']}`")
    return lines


def result_text(result: Any) -> str:
    """A case result, or a divergence's {"a", "b"} results as base -> head."""
    if isinstance(result, dict):
        return f"{result.get('a')} -> {result.get('b')}"
    return str(result) if result is not None else "-"


def table_row(finding: Mapping[str, Any], base: Path, root: Path) -> str:
    step = finding.get("step") or {}
    pointer = (finding.get("pointer") or {}).get("pointer")
    evidence = ", ".join(link(a, base, root) for a in (finding.get("artifacts") or ())[:3])
    return (f"| {finding['run']} | `{finding.get('case') or '-'}` | {finding['kind']} | {result_text(finding.get('result'))} | "
            f"{('`' + step['id'] + '`') if step.get('id') else '-'} | {('`' + pointer + '`') if pointer else '-'} | "
            f"{cell(finding.get('message') or '', 160)} | {evidence} |")


def render_index(report: Mapping[str, Any], out: Path, limit: int) -> str:
    root = Path(report["source"]["path"])
    base = out.resolve()
    lines = [f"# oracle report: {report['source']['kind']} {root.name}", "",
             f"Verdict: **{report['verdict']}**. Findings: "
             + (", ".join(f"{k} {v}" for k, v in sorted(report["counts"].items())) or "none") + ".", ""]
    lines += ["## First failure", ""]
    first = report["first_failure"]
    if first:
        lines += finding_lines(first, base, root)
    elif any(run.get("dry_run") for run in report["runs"]):
        lines.append("None found: no case ran (a dry run plans the cases only).")
    elif report["verdict"] == "pass":
        lines.append("None: every case ended as expected.")
    else:
        lines.append("None found, but the run is incomplete: see Untested.")
    lines += ["", "## Runs", "", "| run | sha | product | platform | build | host | cases | outcomes | flaky | ok |",
              "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"]
    for run in report["runs"]:
        host = run.get("host") or {}
        lines.append(f"| [{run['label']}]({href(run['path'], base, root)}) "
                     f"| {(run.get('sha') or '-')[:12]} | {run.get('product')} | {run.get('platform')} | {run.get('build')} "
                     f"| {cell(host.get('name') or '-', 40)} {cell(host.get('osVersion') or '', 20)} | {run['cases']} "
                     f"| {cell(', '.join(f'{k} {v}' for k, v in sorted(run['counts'].items())) or 'none', 120)} "
                     f"| {run['flaky']} | {run.get('ok')}{' (dry run)' if run.get('dry_run') else ''} |")
    failures = report["failures"]
    lines += ["", f"## Findings ({len(failures)})", ""]
    if failures:
        lines += ["| run | case | kind | result | step | pointer | message | evidence |",
                  "| --- | --- | --- | --- | --- | --- | --- | --- |"]
        lines += [table_row(f, base, root) for f in failures[:limit]]
        if len(failures) > limit:
            lines.append(f"\n{len(failures) - limit} more in receipt.json `failures`.")
    else:
        lines.append("None.")
    lines += comparison_lines(report.get("comparison"), base, root)
    lines += untested_lines(report["untested"], limit)
    lines += artifact_lines(report["artifacts"], base, root, limit)
    return "\n".join(lines) + "\n"


def comparison_lines(comparison: Optional[Mapping[str, Any]], base: Path, root: Path) -> List[str]:
    if not comparison:
        return []
    summary = comparison.get("summary") or {}
    lines = ["", "## Comparison", "",
             f"- {comparison['labels'][0]} vs {comparison['labels'][1]}: verdict {comparison.get('verdict')}, "
             + ", ".join(f"{k} {v}" for k, v in sorted(summary.items()))]
    if comparison.get("refused"):
        lines.append(f"- refused: {cell(comparison['refused'])}")
    if comparison.get("explained"):
        lines.append(f"- explained (an expected difference covers them): {len(comparison['explained'])}")
    if comparison.get("divergence"):
        lines.append("- " + link({"kind": "divergence", "path": comparison["divergence"]}, base, root))
    return lines


def untested_lines(data: Mapping[str, Any], limit: int) -> List[str]:
    lines = ["", "## Untested", ""]
    for key, title in (("skipped", "skipped by Expectations"), ("not_run", "not run (no launch read its fixture)"),
                       ("planned", "planned only (dry run)")):
        if data[key]:
            lines.append(f"- {title}: {len(data[key])}: " + ", ".join(f"`{c['case']}`" for c in data[key][:limit]))
    applicable = {k: v for k, v in data["not_applicable"].items() if v}
    if applicable:
        lines.append("- not applicable to the product or platform: "
                     + ", ".join(f"{k} {v}" for k, v in sorted(applicable.items())))
    if data["flaky"]:
        lines.append(f"- flaky, left out of every comparison: {len(data['flaky'])}")
        lines += [f"  - `{c['case']}` ({c['run']}): {cell('; '.join(c['reasons']), 200)}" for c in data["flaky"][:limit]]
    for key, title in (("not_comparable", "checkpoints with facets not comparable"), ("uncertain", "uncertain checkpoints")):
        if data[key]:
            lines.append(f"- {title}: {len(data[key])}")
            lines += [f"  - `{c['case']}` {c['step']} {c['checkpoint']}"
                      + (f": {', '.join(c['facets'])}" if c.get("facets") else "") for c in data[key][:limit]]
    for arm in data["arm_not_exercised"]:
        lines.append(f"- {arm['run']}: switch {arm['switch']} arm {arm['arm']} never ran (no switch hit)")
    comparison = data.get("comparison") or {}
    if comparison.get("not_comparable"):
        lines.append(f"- not comparable in the comparison: {len(comparison['not_comparable'])}")
        lines += [f"  - `{k}`: {cell(v, 200)}" for k, v in sorted(comparison["not_comparable"].items())[:limit]]
    if comparison.get("partial"):
        lines.append("- the comparison covers some shards only")
    if data["case_reasons"]:
        lines.append(f"- states the cases that ran do not check (corpus `untested`): {len(data['case_reasons'])} cases")
        lines += [f"  - `{c['case']}`: {cell('; '.join(c['reasons']), 240)}" for c in data["case_reasons"][:limit]]
    if len(lines) == 3:
        lines.append("Nothing recorded.")
    return lines


def artifact_lines(artifacts: Sequence[Mapping[str, Any]], base: Path, root: Path, limit: int) -> List[str]:
    if not artifacts:
        return []
    lines = ["", f"## Artifacts ({len(artifacts)})", ""]
    lines += [f"- {link(a, base, root)}" + (f" ({a['bytes']} bytes)" if "bytes" in a else "") for a in artifacts[:limit * 2]]
    if len(artifacts) > limit * 2:
        lines.append(f"- {len(artifacts) - limit * 2} more in receipt.json `artifacts`")
    return lines


# ---------------------------------------------------------------------------------------------------------------
# CLI


def exit_code(report: Mapping[str, Any]) -> int:
    return {"pass": EXIT_PASS, "fail": EXIT_FAIL, "incomplete": EXIT_INCOMPLETE}[report["verdict"]]


def write_report(directory: Path, *, out: Optional[Path] = None, corpus: Path = oracle.DEFAULT_CORPUS,
                 divergence: Optional[Path] = None, limit: int = DEFAULT_LIMIT) -> Tuple[Dict[str, Any], Path]:
    target = (out or directory / "report").resolve()
    try:
        oracle.check_out_dir(target)
    except oracle.UsageError as error:
        raise ReportError(str(error)) from None
    source = load_source(directory.resolve(), divergence.resolve() if divergence else None)
    report = build_report(source, Corpus.load(corpus))
    target.mkdir(parents=True, exist_ok=True)
    oracle.write_json(target / "receipt.json", report)
    (target / "index.md").write_text(render_index(report, target, limit), encoding="utf-8")
    return report, target


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(prog="report.py", description=__doc__.split("\n\n")[0])
    parser.add_argument("dir", type=Path, help="a run dir, a comparison dir or a fleet dir")
    parser.add_argument("--out", type=Path, help="report dir (default DIR/report), outside the checkout")
    parser.add_argument("--corpus", type=Path, default=oracle.DEFAULT_CORPUS,
                        help="the corpus the steps and untested reasons come from (default the checkout's)")
    parser.add_argument("--divergence", type=Path, help="a divergence.json to take pointers from (default DIR's)")
    parser.add_argument("--limit", type=int, default=DEFAULT_LIMIT, help="rows per list in index.md")
    parser.add_argument("--json", action="store_true", help="print the receipt")
    args = parser.parse_args(argv)
    try:
        report, target = write_report(args.dir, out=args.out, corpus=args.corpus, divergence=args.divergence,
                                      limit=max(1, args.limit))
    except ReportError as error:
        print(f"report.py: {error}", file=sys.stderr)
        return EXIT_BAD_INPUT
    if args.json:
        print(json.dumps(report, indent=1, sort_keys=True))
    else:
        first = report["first_failure"]
        print(f"oracle report {report['source']['kind']}: verdict {report['verdict']}, "
              + (", ".join(f"{k} {v}" for k, v in sorted(report["counts"].items())) or "no findings"))
        if first:
            step = (first.get("step") or {}).get("id")
            pointer = (first.get("pointer") or {}).get("pointer")
            print(f"  first failure: {first['kind']} {first.get('case') or ''} in {first['run']}"
                  + (f" at step {step}" if step else "") + (f" pointer {pointer}" if pointer is not None else "")
                  + f": {cell(first.get('message') or '', 200)}")
        print(f"  index: {target / 'index.md'}")
        print(f"  receipt: {target / 'receipt.json'}")
    return exit_code(report)


if __name__ == "__main__":
    sys.exit(main())

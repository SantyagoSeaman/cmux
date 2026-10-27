#!/usr/bin/env python3
"""cargo-mutants over the daemon store, judged by the oracle's daemon suites (p15-oracle-191).

Spec: plans/cmux-next/chrome-parity/p15-oracle.md, section 3F "Mutation strength", task 191 and the
MQ-mutants queue. `mutants.sh` is the entry point; it queues the work in runnerd and calls this file.
Stdlib only, Python 3.9+, safe under `python3 -I`.

cargo-mutants generates the mutants (`cargo mutants --list --json --diff`, configured by
cmux-tui/.cargo/mutants.toml); it never runs `cargo test` here. Each mutant is applied to a job
worktree, the daemon is built with the oracle profile and the oracle runs the daemon suites on it:

    caught    the mutant run diverges from the clean run at the same SHA (`oracle.py diff`), or a case
              that passed on the clean run is unexpected (a crash, a failed step) or leaks state
    missed    every compared case is equal: a survivor, one line of the MQ-mutants queue
    unviable  the mutant does not build
    timeout   the build or the suite run passed its time limit (cargo-mutants counts it as caught)
    error     the run could not judge it (no receipt, a refused comparison, infra errors): rerun

    mutants.py list --root CMUX_TUI [--file GLOB]... [--re RE]... [--json]
        The mutant list of a cmux-tui tree, sorted, with counts per file (or the records as JSON).
    mutants.py shard-job --sha SHA --shard K/N --base-run DIR --suites S [options]
        Inside a runnerd job (mutants.sh _shard): runs one shard and writes RUNNERD_OUT/shard.json.
    mutants.py report SHARD_DIR... --out DIR [--json]
        Merges shard.json files into DIR/mutants.json (counts per file and per store area, no times),
        DIR/survivors.json (the queue) and DIR/receipt.json (times, shard dirs). Refuses a gap, an
        overlap or shards of different lists. --json prints the survivors queue.

Exit: 0 every mutant judged (survivors do not fail: they are a queue, not a gate), 1 a mutant with
status error or an incomplete shard set, 2 bad usage.

Chrome reference (chromium/src):
- base/test/launcher/test_launcher.cc: ShouldRunInCurrentShard() puts a test in shard
  PersistentHash(name) % total, so shards cover each test once and a new test does not move the
  others. A mutant goes to shard sha256(name) % total the same way.
- testing/buildbot: a test step on a try bot builds the patched tree in its own checkout and
  compares it with the "(without patch)" run; here the patch is the mutant and the clean run is the
  base run at the same SHA.
Mutants stay out of the sources, as in Google's mutation testing (Petrovic and Ivankovic, "State of
Mutation Testing at Google", ICSE-SEIP 2018), and as the seeded patches of task 026 do.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import os
import queue
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Iterable, List, Mapping, Optional, Sequence, Tuple

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
TASK = "p15-oracle-191"
LIST_SCHEMA = "cmux-oracle-mutant-list-v1"
SHARD_SCHEMA = "cmux-oracle-mutants-shard-v1"
REPORT_SCHEMA = "cmux-oracle-mutants-v1"
QUEUE_SCHEMA = "cmux-oracle-mutants-survivors-v1"
CONFIG = Path(".cargo") / "mutants.toml"
FINAL = ("caught", "missed", "unviable", "timeout")
STATUSES = FINAL + ("error",)
# The store areas the spec's kill-rate target names (p15-oracle.md, acceptance 7), by path prefix
# under cmux-tui/crates/cmux-tui-core/src/.
AREAS = (
    ("resource_router", ("resource_router.rs", "resource_router/")),
    ("resource_mutation", ("resource_mutation",)),
    ("mux", ("mux.rs", "mux/")),
    ("layout", ("layout.rs",)),
    ("journal_reducers", ("journal_reducers.rs",)),
)
CORE_SRC = "crates/cmux-tui-core/src/"
BY_LIMIT = 5


class UsageError(Exception):
    pass


# --- the mutant list ----------------------------------------------------------------------


@dataclass(frozen=True)
class Mutant:
    id: str
    name: str
    file: str
    line: int
    column: int
    function: Optional[str]
    genre: str
    replacement: str
    patch: str

    def record(self) -> Dict[str, Any]:
        return {"id": self.id, "name": self.name, "file": self.file, "line": self.line, "column": self.column,
                "function": self.function, "genre": self.genre, "replacement": self.replacement}

    def sort_key(self) -> Tuple[str, int, int, str]:
        return (self.file, self.line, self.column, self.name)


def sha256_text(*parts: str) -> str:
    digest = hashlib.sha256()
    for part in parts:
        digest.update(part.encode("utf-8"))
        digest.update(b"\0")
    return digest.hexdigest()


def git_patch(file: str, diff: str) -> str:
    """cargo-mutants' diff (paths relative to cmux-tui/, a `+++` line that names the mutation) as a
    patch `git apply` takes at the repository root."""
    lines = diff.splitlines(keepends=True)
    if len(lines) < 3 or not lines[0].startswith("--- ") or not lines[1].startswith("+++ ") \
            or not lines[2].startswith("@@"):
        raise ValueError(f"{file}: not a cargo-mutants diff")
    if lines[0][4:].rstrip("\n") != file:
        raise ValueError(f"{file}: the diff names {lines[0][4:].rstrip()!r}")
    path = f"cmux-tui/{file}"
    return f"diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n" + "".join(lines[2:])


def parse_list(entries: Sequence[Mapping[str, Any]]) -> List[Mutant]:
    """cargo-mutants `--list --json --diff` records, sorted by place; refuses a duplicate name."""
    mutants: Dict[str, Mutant] = {}
    for entry in entries:
        name, file = str(entry["name"]), str(entry["file"])
        start = (entry.get("span") or {}).get("start") or {}
        function = entry.get("function") or {}
        mutant = Mutant(
            id=sha256_text(name, entry["diff"])[:20], name=name, file=file,
            line=int(start.get("line", 0)), column=int(start.get("column", 0)),
            function=function.get("function_name"), genre=str(entry.get("genre", "")),
            replacement=str(entry.get("replacement", "")), patch=git_patch(file, entry["diff"]))
        if name in mutants:
            raise ValueError(f"cargo-mutants listed {name!r} twice")
        mutants[name] = mutant
    return sorted(mutants.values(), key=Mutant.sort_key)


def list_digest(mutants: Sequence[Mutant]) -> str:
    return sha256_text(*(m.id for m in mutants))


def narrow(mutants: Sequence[Mutant], files: Sequence[str], regexes: Sequence[str]) -> List[Mutant]:
    """The mutants in FILES globs (a glob with a slash matches the whole path, one without the file
    name, as cargo-mutants' --file does) whose name matches every one of REGEXES. cargo-mutants itself
    adds a --file glob to the config's examine_globs, so it cannot narrow the configured list."""
    try:
        patterns = [re.compile(regex) for regex in regexes]
    except re.error as error:
        raise UsageError(f"--re: {error}") from error

    def in_files(path: str) -> bool:
        return not files or any(fnmatch.fnmatchcase(path if "/" in g else path.rsplit("/", 1)[-1], g) for g in files)

    return [m for m in mutants if in_files(m.file) and all(p.search(m.name) for p in patterns)]


def cargo_mutants_list(root: Path, files: Sequence[str] = (), regexes: Sequence[str] = ()) -> List[Mutant]:
    """Runs `cargo mutants --list` in the cmux-tui tree ROOT with its own .cargo/mutants.toml."""
    if not (root / CONFIG).is_file():
        raise UsageError(f"{root}: no {CONFIG} (the mutants config is {TASK}'s)")
    command = ["cargo", "mutants", "--list", "--json", "--diff", "--colors", "never", "-p", "cmux-tui-core"]
    proc = subprocess.run(command, cwd=root, check=False, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if proc.returncode != 0:
        tail = proc.stderr.decode("utf-8", "replace").strip().splitlines()[-5:]
        raise UsageError("cargo mutants --list failed (is cargo-mutants installed? "
                         "scripts/cmux-next/linux/setup-tools.sh): " + " | ".join(tail))
    return narrow(parse_list(json.loads(proc.stdout)), files, regexes)


def shard_of(name: str, total: int) -> int:
    return int(hashlib.sha256(name.encode("utf-8")).hexdigest()[:16], 16) % total


def parse_shard(text: str) -> Tuple[int, int]:
    index, sep, total = text.partition("/")
    if not sep or not index.isdigit() or not total.isdigit() or not 0 <= int(index) < int(total):
        raise UsageError(f"--shard {text}: INDEX/TOTAL, 0-based")
    return int(index), int(total)


def select_shard(mutants: Sequence[Mutant], index: int, total: int, limit: Optional[int]) -> List[Mutant]:
    chosen = [m for m in mutants if shard_of(m.name, total) == index]
    return chosen[:limit] if limit is not None else chosen


def area_of(file: str) -> str:
    rel = file[len(CORE_SRC):] if file.startswith(CORE_SRC) else file
    for area, prefixes in AREAS:
        if any(rel.startswith(prefix) for prefix in prefixes):
            return area
    return "other"


# --- judging one mutant -------------------------------------------------------------------


def load_json(path: Path) -> Optional[Dict[str, Any]]:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


@dataclass(frozen=True)
class Base:
    """What the clean run at the SHA already shows, so a mutant is not blamed for it."""

    unexpected: frozenset
    leaked: frozenset
    duration_s: float
    selection: str
    fingerprint: str

    @classmethod
    def from_receipt(cls, receipt: Mapping[str, Any]) -> "Base":
        return cls(frozenset(receipt.get("unexpected") or ()), frozenset(receipt.get("leaked_state") or ()),
                   float(receipt.get("duration_s") or 0.0), str((receipt.get("selection") or {}).get("digest", "")),
                   str(receipt.get("fingerprint", "")))


def classify(base: Base, head: Optional[Mapping[str, Any]], comparison: Optional[Mapping[str, Any]]) -> Dict[str, Any]:
    """The verdict on a mutant that built and whose suite run finished in time."""
    if head is None:
        return {"status": "error", "reason": "the mutant run wrote no receipt"}
    by = sorted(set(head.get("unexpected") or ()) - base.unexpected)
    if by:
        return {"status": "caught", "reason": "unexpected", "by": by[:BY_LIMIT]}
    by = sorted(set(head.get("leaked_state") or ()) - base.leaked)
    if by:
        return {"status": "caught", "reason": "leaked-state", "by": by[:BY_LIMIT]}
    if comparison is None:
        return {"status": "error", "reason": "oracle.py diff wrote no comparison"}
    if comparison.get("refused"):
        return {"status": "error", "reason": f"comparison refused: {comparison['refused']}"}
    if comparison.get("divergent"):
        return {"status": "caught", "reason": "divergence", "by": list(comparison["divergent"])[:BY_LIMIT]}
    if head.get("infra_errors"):
        return {"status": "error", "reason": "infra: " + str(head["infra_errors"][0])[:200]}
    if not comparison.get("compared"):
        return {"status": "error", "reason": "no case compared"}
    return {"status": "missed", "reason": f"{comparison['compared']} cases equal"}


def run_bounded(command: Sequence[str], *, cwd: Optional[Path] = None, env: Optional[Mapping[str, str]] = None,
                timeout: float, log: Path) -> Optional[int]:
    """Runs COMMAND in its own process group; None when it passed TIMEOUT (the whole group is killed)."""
    with log.open("ab") as handle:
        proc = subprocess.Popen(list(command), cwd=cwd, env=dict(env) if env is not None else None,
                                stdin=subprocess.DEVNULL, stdout=handle, stderr=subprocess.STDOUT,
                                start_new_session=True)
        try:
            return proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
            return None


def log_tail(log: Path, lines: int = 3) -> str:
    try:
        text = log.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""
    keep = [line for line in text.splitlines() if line.strip()]
    errors = [line for line in keep if line.startswith("error")]
    return " | ".join((errors or keep)[-lines:])[:400]


@dataclass(frozen=True)
class JobConfig:
    sha: str
    suites: str
    base_run: Path
    base: Base
    profile: str
    features: str
    jobs: int
    cargo_jobs: int
    build_timeout: float
    test_timeout: float
    cache: Path
    scratch: Path


class Worker:
    """One job worktree and one cargo target dir; judges mutants one at a time."""

    def __init__(self, index: int, config: JobConfig, worktree: Path, target: Path):
        self.index, self.config, self.worktree, self.target = index, config, worktree, target

    def git(self, *args: str, check: bool = True) -> subprocess.CompletedProcess:
        return subprocess.run(["git", "-C", str(self.worktree), *args], check=check,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def judge(self, mutant: Mutant) -> Dict[str, Any]:
        work = self.config.scratch / "m" / mutant.id
        shutil.rmtree(work, ignore_errors=True)
        work.mkdir(parents=True)
        patch = work / "mutant.patch"
        patch.write_text(mutant.patch, encoding="utf-8")
        applied = self.git("apply", str(patch), check=False)
        if applied.returncode != 0:
            return {"status": "error", "reason": "does not apply: " + applied.stderr.decode("utf-8", "replace")[:200]}
        try:
            return self.build_and_run(mutant, work)
        finally:
            self.git("checkout", "--", f"cmux-tui/{mutant.file}")
            if self.git("status", "--porcelain", "--untracked-files=no").stdout.strip():
                raise RuntimeError(f"worker {self.index}: worktree not clean after {mutant.name}")
            shutil.rmtree(work, ignore_errors=True)

    def build_and_run(self, mutant: Mutant, work: Path) -> Dict[str, Any]:
        c = self.config
        env = dict(os.environ, CARGO_TARGET_DIR=str(self.target), CARGO_INCREMENTAL="1")
        t0 = time.monotonic()
        code = run_bounded(["cargo", "build", "--locked", "--profile", c.profile, "--features", c.features,
                            "-p", "cmux-tui", "-j", str(c.cargo_jobs)],
                           cwd=self.worktree / "cmux-tui", env=env, timeout=c.build_timeout, log=work / "build.log")
        timing = {"buildMs": round((time.monotonic() - t0) * 1000)}
        if code is None:
            return {"status": "timeout", "reason": "build", **timing}
        if code != 0:
            return {"status": "unviable", "reason": log_tail(work / "build.log"), **timing}
        profile_dir = {"dev": "debug", "test": "debug", "bench": "release"}.get(c.profile, c.profile)
        binary = self.target / profile_dir / "cmux-tui"
        run_dir, cmp_dir = work / "run", work / "cmp"
        t1 = time.monotonic()
        code = run_bounded([sys.executable, "-I", str(HERE / "oracle.py"), "run", "--product", "daemon",
                            "--suite", c.suites, "--binary", str(binary), "--ref", c.sha, "--jobs", str(c.jobs),
                            "--out", str(run_dir)], timeout=c.test_timeout, log=work / "run.log")
        timing["testMs"] = round((time.monotonic() - t1) * 1000)
        if code is None:
            return {"status": "timeout", "reason": "suite run", **timing}
        head = load_json(run_dir / "receipt.json")
        if head is None:
            return {"status": "error", "reason": f"no receipt (exit {code}): {log_tail(work / 'run.log')}", **timing}
        run_bounded([sys.executable, "-I", str(HERE / "oracle.py"), "diff", "--base-run", str(c.base_run),
                     "--head-run", str(run_dir), "--out", str(cmp_dir)],
                    timeout=c.test_timeout, log=work / "diff.log")
        return {**classify(c.base, head, load_json(cmp_dir / "comparison.json")), **timing}


def prepare_worktree(sha: str, path: Path) -> None:
    """A `git clone --shared` of this repo at SHA with ghostty-next at its own gitlink, as
    linux_run.sh's build job makes one (p15-oracle-024): CMUX_GHOSTTY_SRC is never used."""
    def git(*args: str) -> str:
        return subprocess.run(["git", *args], check=True, stdout=subprocess.PIPE).stdout.decode().strip()

    common = git("-C", str(REPO), "rev-parse", "--path-format=absolute", "--git-common-dir")
    git("clone", "--quiet", "--shared", "--no-checkout", common, str(path))
    git("-C", str(path), "-c", "advice.detachedHead=false", "checkout", "--quiet", "--detach", sha)
    gitlink = git("-C", str(path), "rev-parse", "HEAD:ghostty-next")
    git("-C", str(path), "submodule", "init", "ghostty-next")
    update = ["-C", str(path), "-c", "protocol.file.allow=always", "submodule", "update", "--checkout"]
    refdir = Path(common) / "modules" / "ghostty-next"
    if refdir.is_dir():
        update += ["--reference", str(refdir)]
        known = subprocess.run(["git", "-C", str(refdir), "cat-file", "-e", f"{gitlink}^{{commit}}"],
                               check=False, stderr=subprocess.DEVNULL).returncode == 0
        if known:
            git("-C", str(path), "config", "submodule.ghostty-next.url", str(refdir))
    git(*update, "ghostty-next")
    if git("-C", str(path / "ghostty-next"), "rev-parse", "HEAD") != gitlink:
        raise RuntimeError(f"ghostty-next is not at its gitlink {gitlink}")


def cache_dir(root: Path, sha: str, suites: str, base: Base, profile: str, features: str) -> Path:
    """Verdicts hold for one SHA, suite set, clean run selection and build: ROOT/SHA/KEY."""
    return root / sha / sha256_text(sha, suites, base.selection, base.fingerprint, profile, features)[:16]


def run_shard(config: JobConfig, mutants: Sequence[Mutant], workers: int, *, fresh: bool,
              targets: Path) -> Dict[str, Any]:
    """Judges MUTANTS with WORKERS worktrees; a final verdict is cached under config.cache by mutant id,
    so a rerun of an interrupted shard judges only what is left."""
    config.cache.mkdir(parents=True, exist_ok=True)
    results: Dict[str, Dict[str, Any]] = {}
    todo: "queue.Queue[Mutant]" = queue.Queue()
    for mutant in mutants:
        cached = None if fresh else load_json(config.cache / f"{mutant.id}.json")
        if cached and cached.get("status") in FINAL and cached.get("name") == mutant.name:
            results[mutant.id] = {**cached, "cached": True}
        else:
            todo.put(mutant)
    lock = threading.Lock()
    failures: List[str] = []
    total, started = todo.qsize(), time.monotonic()

    def work(index: int) -> None:
        try:
            worktree = config.scratch / f"wt{index}"
            prepare_worktree(config.sha, worktree)
            worker = Worker(index, config, worktree, targets / f"w{index}")
        except (subprocess.CalledProcessError, RuntimeError) as error:
            with lock:
                failures.append(f"worker {index}: {error}")
            return
        while True:
            try:
                mutant = todo.get_nowait()
            except queue.Empty:
                return
            try:
                verdict = {**mutant.record(), **worker.judge(mutant)}
            except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
                # The worktree may still hold the mutant: this worker stops, the others go on.
                with lock:
                    results[mutant.id] = {**mutant.record(), "status": "error", "reason": str(error)[:400]}
                    failures.append(f"worker {index}: {error}")
                return
            with lock:
                results[mutant.id] = verdict
                if verdict["status"] in FINAL:
                    tmp = config.cache / f".{mutant.id}.{index}.tmp"
                    tmp.write_text(json.dumps(verdict, sort_keys=True) + "\n", encoding="utf-8")
                    os.replace(tmp, config.cache / f"{mutant.id}.json")
                done = sum(1 for r in results.values() if not r.get("cached"))
                print(f"mutants: [{done}/{total}] {verdict['status']:8} {mutant.name} "
                      f"({(time.monotonic() - started) / 60:.1f} min)", file=sys.stderr, flush=True)

    threads = [threading.Thread(target=work, args=(i,), daemon=True) for i in range(min(workers, total))]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    for mutant in mutants:
        results.setdefault(mutant.id, {**mutant.record(), "status": "error", "reason": "; ".join(failures) or "not run"})
    return {"results": [results[m.id] for m in mutants], "workerErrors": failures}


# --- report ---------------------------------------------------------------------------------


def stable_result(result: Mapping[str, Any]) -> Dict[str, Any]:
    """A result without times or cache marks: what mutants.json and the queue keep."""
    return {k: v for k, v in result.items() if k not in ("buildMs", "testMs", "cached")}


def tally(results: Iterable[Mapping[str, Any]]) -> Dict[str, Any]:
    counts = {status: 0 for status in STATUSES}
    for result in results:
        counts[result["status"]] += 1
    judged = counts["caught"] + counts["timeout"] + counts["missed"]
    counts["total"] = sum(counts[s] for s in STATUSES)
    counts["killRate"] = round((counts["caught"] + counts["timeout"]) / judged, 4) if judged else None
    return counts


def merge_shards(shards: Sequence[Mapping[str, Any]]) -> Dict[str, Any]:
    """One report from shard documents; raises ValueError on a gap, an overlap or a mismatch."""
    if not shards:
        raise ValueError("no shard")
    first = shards[0]
    same = ("sha", "suites", "listDigest", "listCount", "shardTotal", "baseSelection", "limit", "filters")
    for shard in shards[1:]:
        for key in same:
            if shard.get(key) != first.get(key):
                raise ValueError(f"shards differ in {key}: {first.get(key)!r} and {shard.get(key)!r}")
    indexes = sorted(int(s["shard"]) for s in shards)
    if indexes != sorted(set(indexes)):
        raise ValueError(f"a shard is given twice: {indexes}")
    missing = sorted(set(range(int(first["shardTotal"]))) - set(indexes))
    results: Dict[str, Dict[str, Any]] = {}
    for shard in shards:
        for result in shard["results"]:
            if result["id"] in results:
                raise ValueError(f"mutant {result['name']} is in two shards")
            results[result["id"]] = stable_result(result)
    ordered = sorted(results.values(), key=lambda r: (r["file"], r["line"], r["column"], r["name"]))
    files: Dict[str, List[Mapping[str, Any]]] = {}
    areas: Dict[str, List[Mapping[str, Any]]] = {}
    for result in ordered:
        files.setdefault(result["file"], []).append(result)
        areas.setdefault(area_of(result["file"]), []).append(result)
    complete = not missing and (first.get("limit") is not None or len(ordered) == int(first["listCount"]))
    return {
        "schema": REPORT_SCHEMA,
        "task": TASK,
        "sha": first["sha"],
        "suites": first["suites"],
        "baseSelection": first["baseSelection"],
        "listDigest": first["listDigest"],
        "listCount": first["listCount"],
        "filters": first.get("filters"),
        "limit": first.get("limit"),
        "shardTotal": first["shardTotal"],
        "missingShards": missing,
        "complete": complete,
        "counts": tally(ordered),
        "areas": {area: tally(rows) for area, rows in sorted(areas.items())},
        "files": {file: tally(rows) for file, rows in sorted(files.items())},
        "errors": [{"id": r["id"], "name": r["name"], "reason": r.get("reason", "")}
                   for r in ordered if r["status"] == "error"],
        "survivors": [r["id"] for r in ordered if r["status"] == "missed"],
    }


def survivors_queue(report: Mapping[str, Any], shards: Sequence[Mapping[str, Any]]) -> Dict[str, Any]:
    """MQ-mutants: one line per surviving mutant, routed to corpus-gap (p15-oracle.md, section 6)."""
    by_id = {r["id"]: r for shard in shards for r in shard["results"]}
    patches = {k: v for shard in shards for k, v in (shard.get("patches") or {}).items()}
    items = []
    for mutant_id in report["survivors"]:
        result = stable_result(by_id[mutant_id])
        result.pop("status", None)
        result.pop("reason", None)
        items.append({**result, "route": "corpus-gap", "patch": patches.get(mutant_id)})
    return {"schema": QUEUE_SCHEMA, "queue": "MQ-mutants", "sha": report["sha"], "suites": report["suites"],
            "listDigest": report["listDigest"], "items": items}


def dump(value: Any) -> str:
    return json.dumps(value, indent=1, sort_keys=True) + "\n"


def write_atomic(path: Path, text: str) -> None:
    tmp = path.with_name(f".{path.name}.tmp")
    tmp.write_text(text, encoding="utf-8")
    os.replace(tmp, path)


def print_counts(report: Mapping[str, Any], stream=sys.stdout) -> None:
    def row(label: str, c: Mapping[str, Any]) -> str:
        rate = f"{c['killRate'] * 100:5.1f}%" if c["killRate"] is not None else "    -"
        return (f"  {c['total']:5} {c['caught']:5} {c['missed']:5} {c['unviable']:5} {c['timeout']:5} "
                f"{c['error']:5} {rate}  {label}")
    print(f"mutants at {report['sha'][:12]} ({report['suites']}): "
          + ("complete" if report["complete"] else f"INCOMPLETE, missing shards {report['missingShards']}"), file=stream)
    print("  total caught missed unviable timeout error  kill  file", file=stream)
    for file, counts in report["files"].items():
        print(row(file, counts), file=stream)
    for area, counts in report["areas"].items():
        print(row(f"[{area}]", counts), file=stream)
    print(row("[all]", report["counts"]), file=stream)


# --- commands -------------------------------------------------------------------------------


def cmd_list(args: argparse.Namespace) -> int:
    mutants = cargo_mutants_list(args.root, args.file or [], args.re or [])
    if args.json:
        sys.stdout.write(dump({"schema": LIST_SCHEMA, "listDigest": list_digest(mutants), "count": len(mutants),
                               "mutants": [m.record() for m in mutants]}))
        return 0
    counts: Dict[str, int] = {}
    for mutant in mutants:
        counts[mutant.file] = counts.get(mutant.file, 0) + 1
    for file, count in sorted(counts.items()):
        print(f"{count:6} {file}")
    print(f"{len(mutants):6} total (list {list_digest(mutants)[:12]})")
    return 0


def cmd_shard_job(args: argparse.Namespace) -> int:
    if not os.environ.get("RUNNERD_JOB"):
        raise UsageError("shard-job runs only inside a runnerd job (mutants.sh run)")
    out = Path(os.environ["RUNNERD_OUT"])
    scratch = Path(os.environ["RUNNERD_SCRATCH"])
    targets = Path(os.environ["RUNNERD_TARGET_DIR"])
    index, total = parse_shard(args.shard)
    base_receipt = load_json(args.base_run / "receipt.json")
    if base_receipt is None:
        raise UsageError(f"--base-run {args.base_run}: no receipt.json")
    base = Base.from_receipt(base_receipt)
    test_timeout = args.test_timeout or max(300.0, 5 * base.duration_s)
    lister = scratch / "list"
    prepare_worktree(args.sha, lister)
    mutants = cargo_mutants_list(lister / "cmux-tui", args.file or [], args.re or [])
    shutil.rmtree(lister, ignore_errors=True)
    chosen = select_shard(mutants, index, total, args.limit)
    config = JobConfig(sha=args.sha, suites=args.suites, base_run=args.base_run, base=base, profile=args.profile,
                       features=args.features, jobs=args.jobs, cargo_jobs=args.cargo_jobs,
                       build_timeout=args.build_timeout, test_timeout=test_timeout,
                       cache=cache_dir(args.cache_root, args.sha, args.suites, base, args.profile, args.features),
                       scratch=scratch)
    print(f"mutants: shard {index}/{total}: {len(chosen)} of {len(mutants)} mutants, {args.workers} workers, "
          f"test timeout {test_timeout:.0f} s, cache {config.cache}", file=sys.stderr, flush=True)
    t0 = time.monotonic()
    shard = run_shard(config, chosen, args.workers, fresh=args.fresh, targets=targets)
    doc = {
        "schema": SHARD_SCHEMA, "task": TASK, "sha": args.sha, "suites": args.suites,
        "shard": index, "shardTotal": total, "limit": args.limit,
        "filters": {"file": sorted(args.file or []), "re": sorted(args.re or [])},
        "listDigest": list_digest(mutants), "listCount": len(mutants),
        "baseRun": str(args.base_run), "baseSelection": base.selection, "cache": str(config.cache),
        "testTimeoutS": test_timeout, "workers": args.workers, "durationS": round(time.monotonic() - t0, 1),
        "results": shard["results"], "workerErrors": shard["workerErrors"],
        "patches": {m.id: m.patch for m, r in zip(chosen, shard["results"]) if r["status"] == "missed"},
    }
    write_atomic(out / "shard.json", dump(doc))
    print(str(out))
    return 1 if any(r["status"] == "error" for r in shard["results"]) or shard["workerErrors"] else 0


def cmd_report(args: argparse.Namespace) -> int:
    shards = []
    for directory in args.dirs:
        doc = load_json(directory / "shard.json")
        if doc is None or doc.get("schema") != SHARD_SCHEMA:
            raise UsageError(f"{directory}: no {SHARD_SCHEMA} shard.json")
        shards.append(doc)
    try:
        report = merge_shards(shards)
    except ValueError as error:
        raise UsageError(str(error)) from error
    out = args.out
    out.mkdir(parents=True, exist_ok=True)
    queue_doc = survivors_queue(report, shards)
    write_atomic(out / "mutants.json", dump(report))
    write_atomic(out / "survivors.json", dump(queue_doc))
    write_atomic(out / "receipt.json", dump({
        "schema": "cmux-oracle-mutants-receipt-v1", "sha": report["sha"],
        "created": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "shards": [{"dir": str(d), "shard": s["shard"], "durationS": s.get("durationS"), "workers": s.get("workers"),
                    "baseRun": s.get("baseRun"), "cache": s.get("cache"), "workerErrors": s.get("workerErrors")}
                   for d, s in zip(args.dirs, shards)],
        "timesMs": {r["id"]: {k: r[k] for k in ("buildMs", "testMs") if k in r}
                    for s in shards for r in s["results"] if not r.get("cached")},
    }))
    if args.json:
        sys.stdout.write(dump(queue_doc))
    else:
        print_counts(report)
        print(f"  report: {out / 'mutants.json'}\n  survivors (MQ-mutants): {out / 'survivors.json'}")
    return 0 if report["complete"] and not report["errors"] else 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="mutants.py", description=__doc__.split("\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    lst = sub.add_parser("list", help="the mutant list of a cmux-tui tree")
    lst.add_argument("--root", type=Path, default=REPO / "cmux-tui")
    lst.add_argument("--file", action="append", help="only mutants in files matching this glob (repeat)")
    lst.add_argument("--re", action="append", help="only mutants whose name matches this regex (repeat: all)")
    lst.add_argument("--json", action="store_true")
    job = sub.add_parser("shard-job", help="one shard, inside a runnerd job")
    job.add_argument("--sha", required=True)
    job.add_argument("--shard", required=True)
    job.add_argument("--base-run", type=Path, required=True)
    job.add_argument("--suites", required=True)
    job.add_argument("--workers", type=int, default=4)
    job.add_argument("--jobs", type=int, default=25, help="oracle.py run --jobs per worker")
    job.add_argument("--cargo-jobs", type=int, default=30, help="cargo -j per worker")
    job.add_argument("--profile", default="oracle")
    job.add_argument("--features", default="oracle")
    job.add_argument("--build-timeout", type=float, default=3600.0)
    job.add_argument("--test-timeout", type=float, help="default max(300 s, 5x the base run)")
    job.add_argument("--file", action="append")
    job.add_argument("--re", action="append")
    job.add_argument("--limit", type=int)
    job.add_argument("--cache-root", type=Path, default=Path("/tmp/oracle/mutants/cache"))
    job.add_argument("--fresh", action="store_true", help="judge cached mutants again")
    rep = sub.add_parser("report", help="merge shard dirs into the report and the survivors queue")
    rep.add_argument("dirs", type=Path, nargs="+")
    rep.add_argument("--out", type=Path, required=True)
    rep.add_argument("--json", action="store_true", help="print the survivors queue")
    return parser


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return {"list": cmd_list, "shard-job": cmd_shard_job, "report": cmd_report}[args.command](args)
    except UsageError as error:
        print(f"mutants.py: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

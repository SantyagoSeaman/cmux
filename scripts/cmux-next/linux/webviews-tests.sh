#!/usr/bin/env bash
# Run the webviews/ and web/ bun tests on the Linux VM: bun install
# --frozen-lockfile, then every test file in its own `bun test` process, 300 at
# once, three times with no retries, and list the flaky tests. A test is flaky
# when its results differ between the repeats, as in Chromium's flakiness model
# (several results on one TestExpectations line mean flaky; a retry would hide
# the flake, so there is none). One process per file is the batch size of 1 of
# base/test/launcher/test_launcher.cc, and the same rule as
# scripts/ci/run-webviews-tests.sh: a single `bun test` stalls on state one file
# leaks into the next. Counts come from bun's own JUnit reporter.
# Context: plans/cmux-next/chrome-parity/p15-oracle.md (task 214, MQ-flakes).
#
# Usage: scripts/cmux-next/linux/webviews-tests.sh [--package webviews|web]... [--jobs N]
#            [--repeats N] [--out DIR] [--flaky-out FILE] [--filter REGEX]
#            [--test-timeout-ms MS] [--file-timeout S] [--browser-lane]
#        scripts/cmux-next/linux/webviews-tests.sh --self-test
#   --package          a package to test; repeat for both (default: webviews and web).
#   --jobs             test files at once (default 300). Files that start a Vite dev
#                      server run after the rest, a quarter of the jobs at once.
#   --repeats          full runs (default 3, at least 2).
#   --out              results directory (default /tmp/oracle/webviews-tests/<UTC>-<sha12>).
#   --flaky-out        file that gets one JSON line per flaky test, appended (default
#                      /tmp/oracle/flaky.jsonl, the MQ-flakes queue that oracle.py and
#                      rust-tests.sh also feed).
#   --filter           a Python regex: run only the test files (package-relative) it matches.
#   --test-timeout-ms  bun's per-test limit (default 30000, as run-webviews-tests.sh).
#   --file-timeout     seconds one file may run (default 120); a file may name more with a
#                      `// cmux-web-test-timeout: SECONDS` line in its first 20 lines.
#   --browser-lane     run the Playwright browser tests too (CMUX_BROWSER_TESTS=1); installs
#                      the chromium and webkit shells first. Off by default: they skip.
#   --self-test        check the runner and the classifier with a stand-in bun; runs no test.
# Results: <out>/receipt.json, <out>/<package>/install.log,
#   <out>/<package>/repeat-<i>/{files.jsonl,logs/,junit/}.
# Exit 0 when no test failed in every repeat (flaky tests do not fail the run; they go
# to --flaky-out), 1 when a test or file failed in every repeat, 2 on a usage error, 75
# when the install, the bun version or a reporter file is wrong, 130 or 143 on SIGINT
# or SIGTERM: no new file starts, every running file's process group is killed (TERM,
# then KILL) and no receipt is written.
#
# Linux only: never test on a person's Mac (CLAUDE.md). Needs bun at the version that
# webviews/package.json pins (devEngines). Runs at nice 10: the VM also runs agents.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"

for arg in "$@"; do
  if [[ "$arg" == -h || "$arg" == --help ]]; then
    sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
    exit 0
  fi
done

exec python3 -I - "$root" "$0" "$@" <<'PY'
import argparse, concurrent.futures, datetime, json, os, re, shutil, signal, socket
import subprocess, sys, tempfile, threading, time
import xml.etree.ElementTree as ET

ROOT, SELF = sys.argv[1], sys.argv[2]
PACKAGES = ("webviews", "web")
RED = {"FAIL", "TIMEOUT", "CRASH", "NOTRUN"}
GREEN = {"PASS", "SKIP", "TODO"}
FILE_KEY = "(file)"
# bun prints this for an error no test owns; its JUnit file does not show it.
OUTSIDE_ERROR = b"# Unhandled error between tests"


def say(text):
    print(f"webviews-tests: {text}", flush=True)


class UsageError(Exception):
    pass


class SetupError(Exception):
    pass


class Interrupted(Exception):
    """SIGINT or SIGTERM reached the runner."""

    def __init__(self, signum):
        super().__init__(signal.Signals(signum).name)
        self.signum = signum


class Stopped(Exception):
    """A file did not start, or its result is void, because the run is stopping."""


# ---------------------------------------------------------------- test files

# webviews: bun's default names from the git index, as scripts/ci/run-webviews-tests.sh.
WEBVIEWS_GLOBS = ["*.test.ts", "*.test.tsx", "*.test.js", "*.test.jsx", "*.test.mjs",
                  "*_test.ts", "*_test.tsx", "*.spec.ts", "*.spec.tsx", "*_spec.ts", "*_spec.tsx"]
# web: the names web/scripts/run-tests.sh finds (no node_modules, no dot folders), from the index.
WEB_NAME = re.compile(r"(\.test|_test|\.spec|_spec)\.(js|jsx|ts|tsx|mjs|cjs|mts|cts)$")
# A file that starts a Vite dev server (scripts/ci/run-webviews-tests.sh, dev_servers).
DEV_SERVER = re.compile(r'vite\.config\.(acpmux-pane\.mjs|gallery-dev\.ts)|"dev",|configFile: path\.join\(webviews, "vite\.config')
DECLARED_LIMIT = re.compile(r"^// cmux-web-test-timeout: ([1-9][0-9]*)$")


def git_files(package_dir, globs):
    out = subprocess.run(["git", "-C", package_dir, "ls-files", "-z", "--", *globs],
                         check=True, capture_output=True).stdout
    return sorted(item.decode() for item in out.split(b"\0") if item)


def test_files(package, package_dir, pattern):
    if package == "webviews":
        files = git_files(package_dir, WEBVIEWS_GLOBS)
    else:
        files = [f for f in git_files(package_dir, []) if WEB_NAME.search(f)
                 and not any(part.startswith(".") or part == "node_modules" for part in f.split("/")[:-1])]
    if pattern:
        files = [f for f in files if pattern.search(f)]
    return files


def head(path, lines=20):
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            return [handle.readline().rstrip("\n") for _ in range(lines)]
    except OSError:
        return []


def file_limit(path, default):
    for line in head(path):
        match = DECLARED_LIMIT.match(line)
        if match:
            return max(default, int(match.group(1)))
    return default


def vite_config(path):
    text = open(path, encoding="utf-8", errors="replace").read()
    if "vite.config.acpmux-pane.mjs" in text:
        return "pane"
    if "vite.config.gallery-dev.ts" in text:
        return "gallery"
    if "vite.config.settings-page.mjs" in text:
        return "settings"
    return "default"


# ---------------------------------------------------------------- one file

def kill_group(pgid, sig):
    try:
        os.killpg(pgid, sig)
    except (ProcessLookupError, PermissionError):
        pass


class Children:
    """The process groups of the test files running now. stop() lets no new file start
    and kills every live group: TERM, up to `grace` seconds for its leader to exit, then
    KILL, as a file's timeout does (test_launcher.cc kills a child's whole job object)."""

    def __init__(self, grace=10):
        self.lock, self.live, self.killed = threading.Lock(), {}, set()
        self.stopped, self.grace = False, grace

    def spawn(self, command, **kwargs):
        if self.stopped:
            raise Stopped()
        child = subprocess.Popen(command, start_new_session=True, **kwargs)
        with self.lock:
            if not self.stopped:
                self.live[child.pid] = child
                return child
        kill_group(child.pid, signal.SIGKILL)
        child.wait()
        raise Stopped()

    def release(self, child):
        kill_group(child.pid, signal.SIGKILL)
        with self.lock:
            self.live.pop(child.pid, None)

    def stop(self):
        with self.lock:
            self.stopped = True
            live = list(self.live.values())
        self.killed.update(child.pid for child in live)
        for child in live:
            kill_group(child.pid, signal.SIGTERM)
        deadline = time.monotonic() + self.grace
        for child in live:
            try:
                child.wait(timeout=max(0, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                pass
        for child in live:
            kill_group(child.pid, signal.SIGKILL)


def outside_errors(log):
    with open(log, "rb") as handle:
        return sum(1 for line in handle if line.rstrip() == OUTSIDE_ERROR)


def run_file(children, bun, package_dir, rel, rdir, env, test_timeout_ms, limit, cache=None, seed=None):
    """Run one test file in its own process group; return its record. The group is
    killed after the file ends, so a dev server or browser it leaves behind dies with it."""
    if children.stopped:
        raise Stopped()
    flat = rel.replace("/", "_")
    log = os.path.join(rdir, "logs", flat + ".log")
    junit = os.path.join(rdir, "junit", flat + ".xml")
    child_env = dict(env)
    if cache:
        shutil.rmtree(cache, ignore_errors=True)
        os.makedirs(os.path.dirname(cache), exist_ok=True)
        if seed and os.path.isdir(seed):
            shutil.copytree(seed, cache, symlinks=True)
        os.makedirs(cache, exist_ok=True)
        child_env["CMUX_VITE_CACHE_DIR"] = cache
    command = [bun, "test", "--timeout", str(test_timeout_ms), "--reporter=junit",
               f"--reporter-outfile={junit}", "./" + rel]
    started = time.monotonic()
    timed_out = False
    with open(log, "wb") as output:
        child = children.spawn(command, cwd=package_dir, env=child_env, stdin=subprocess.DEVNULL,
                               stdout=output, stderr=subprocess.STDOUT)
        try:
            code = child.wait(timeout=limit)
        except subprocess.TimeoutExpired:
            timed_out = True
            kill_group(child.pid, signal.SIGTERM)
            try:
                code = child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                kill_group(child.pid, signal.SIGKILL)
                code = child.wait()
        finally:
            children.release(child)
    if children.stopped:
        raise Stopped()
    if timed_out:
        verdict = "TIMEOUT"
    elif code < 0 or code >= 128:
        verdict = "CRASH"
    else:
        verdict = "PASS" if code == 0 else "FAIL"
    return {"file": rel, "verdict": verdict, "exit": code, "seconds": round(time.monotonic() - started, 3),
            "limit": limit, "log": log, "junit": junit if os.path.exists(junit) else None,
            "outside_errors": outside_errors(log), "vite_cache": "warm" if seed else ("own" if cache else None)}


class Progress:
    def __init__(self, label, total):
        self.label, self.total, self.done, self.lock = label, total, 0, threading.Lock()
        self.step = max(1, total // 10)

    def tick(self, record):
        with self.lock:
            self.done += 1
            if record["verdict"] != "PASS":
                say(f"{self.label}: {record['verdict']} {record['file']} ({record['seconds']:.0f} s)")
            if self.done % self.step == 0 or self.done == self.total:
                say(f"{self.label}: {self.done}/{self.total} files")


def pool(children, jobs, run, tasks, progress):
    """Run the (args, kwargs) tasks through `run`, at most `jobs` at once, on worker
    threads, so no file starts on the main thread, where the signal handler raises. On
    any exception the running groups are killed and the pending tasks are dropped."""
    records = []
    if not tasks:
        return records
    executor = concurrent.futures.ThreadPoolExecutor(max_workers=jobs)
    try:
        futures = [executor.submit(run, *args, **kwargs) for args, kwargs in tasks]
        for future in concurrent.futures.as_completed(futures):
            record = future.result()
            progress.tick(record)
            records.append(record)
    except BaseException:
        children.stop()
        raise
    finally:
        executor.shutdown(wait=True, cancel_futures=True)
    return records


def run_and_drop(*args, **kwargs):
    record = run_file(*args, **kwargs)
    shutil.rmtree(kwargs["cache"], ignore_errors=True)
    return record


def run_repeat(children, bun, package, package_dir, files, rdir, env, args):
    """One full run of a package's files. webviews dev-server files run after the rest:
    one file per Vite config warms that config's cache alone, then the rest run a quarter
    of the jobs at once on copies of it (scripts/ci/run-webviews-tests.sh says why)."""
    for sub in ("logs", "junit"):
        os.makedirs(os.path.join(rdir, sub), exist_ok=True)
    progress = Progress(f"{package} {os.path.basename(rdir)}", len(files))
    base = (children, bun, package_dir)
    rest = (env, args.test_timeout_ms)
    limit = lambda rel: file_limit(os.path.join(package_dir, rel), args.file_timeout)
    dev = [f for f in files if package == "webviews"
           and DEV_SERVER.search(open(os.path.join(package_dir, f), encoding="utf-8", errors="replace").read())]
    dev_set = set(dev)
    plain = [f for f in files if f not in dev_set]
    records = pool(children, args.jobs, run_file, [((*base, f, rdir, *rest, limit(f)), {}) for f in plain], progress)
    if not dev:
        return records
    vite = tempfile.mkdtemp(prefix="vite-", dir=rdir)
    try:
        warmed, tries, waiting = {}, {}, []
        for rel in dev:
            config = vite_config(os.path.join(package_dir, rel))
            if config in warmed or tries.get(config, 0) >= args.warm_attempts:
                waiting.append((config, rel))
                continue
            tries[config] = tries.get(config, 0) + 1
            seed = os.path.join(vite, f"seed-{config}-{tries[config]}")
            [record] = pool(children, 1, run_file, [((*base, rel, rdir, *rest, limit(rel)), {"cache": seed})],
                            progress)
            records.append(record)
            if record["verdict"] == "PASS":
                warmed[config] = seed
            else:
                shutil.rmtree(seed, ignore_errors=True)
        warm, cold = [], []
        for index, (config, rel) in enumerate(waiting):
            cache = os.path.join(vite, str(index))
            task = ((*base, rel, rdir, *rest, limit(rel)), {"cache": cache, "seed": warmed.get(config)})
            (warm if config in warmed else cold).append(task)
        records += pool(children, max(1, args.jobs // 4), run_and_drop, warm, progress)
        records += pool(children, 1, run_and_drop, cold, progress)
    finally:
        shutil.rmtree(vite, ignore_errors=True)
    return records


# ---------------------------------------------------------------- classify

def case_result(case):
    """bun's JUnit: no child = PASS; <failure type="TimeoutError"> = TIMEOUT; other
    <failure>/<error> = FAIL; <skipped message="TODO"> = TODO; other <skipped> = SKIP."""
    for child in case:
        if child.tag == "skipped":
            return "TODO" if child.get("message") == "TODO" else "SKIP"
        if child.tag in ("failure", "error"):
            return "TIMEOUT" if child.get("type") == "TimeoutError" else "FAIL"
    return "PASS"


def read_junit(path):
    """(reporter counts from <testsuites>, [(classname, name, result)])."""
    tree = ET.parse(path).getroot()
    roots = [tree] if tree.tag == "testsuites" else []
    reporter = {k: sum(int(r.get(k, 0)) for r in roots) for k in ("tests", "failures", "skipped", "assertions")}
    cases = [(c.get("classname", ""), c.get("name", ""), case_result(c)) for c in tree.iter("testcase")]
    return reporter, cases


def file_outcome(record, cases):
    """What one repeat says about the file apart from its tests: TIMEOUT or CRASH; NOREPORT
    when bun wrote no readable JUnit file; ERROR when bun saw an error no test owns (an exit
    1 with no red test, or "# Unhandled error between tests" in the log); else OK. A red
    test is not file-level evidence: its own entry carries it."""
    if record["verdict"] in ("TIMEOUT", "CRASH"):
        return record["verdict"]
    if cases is None:
        return "NOREPORT"
    if record["outside_errors"] or (record["verdict"] == "FAIL" and not any(r in RED for _, _, r in cases)):
        return "ERROR"
    return "OK"


def analyze(meta, runs):
    """runs: {package: [[file record, ...] per repeat]}. Returns (receipt, flaky lines, exit code).
    A test's flakiness is judged over the repeats in which its file wrote a report. The
    file's own (file) entry is judged on file_outcome only, so a test that flips gives one
    flake (the test's), and a file-level failure gives one entry, not a NOTRUN per test."""
    errors, packages, flaky_lines = [], {}, []
    for package, repeats in runs.items():
        count = len(repeats)
        by_test, by_file, reported, summaries = {}, {}, {}, []
        for index, records in enumerate(repeats):
            reporter = {"tests": 0, "failures": 0, "skipped": 0, "assertions": 0}
            tests, verdicts = {}, {}
            for record in sorted(records, key=lambda r: r["file"]):
                rel = record["file"]
                verdicts[record["verdict"]] = verdicts.get(record["verdict"], 0) + 1
                cases = None
                if record["junit"] is None:
                    if record["verdict"] == "PASS":
                        errors.append(f"{package} repeat {index + 1}: {rel} exited 0 but bun wrote no JUnit file")
                else:
                    try:
                        counts, cases = read_junit(record["junit"])
                    except (ET.ParseError, OSError, ValueError) as exc:
                        errors.append(f"{package} repeat {index + 1}: {rel}: unreadable JUnit file ({exc})")
                by_file.setdefault(rel, [None] * count)[index] = file_outcome(record, cases)
                reported.setdefault(rel, [False] * count)[index] = cases is not None
                if cases is None:
                    continue
                for key in reporter:
                    reporter[key] += counts[key]
                if len(cases) != counts["tests"]:
                    errors.append(f"{package} repeat {index + 1}: {rel}: bun reported {counts['tests']} tests, "
                                  f"its file lists {len(cases)}")
                if record["verdict"] == "PASS" and any(r in RED for _, _, r in cases):
                    errors.append(f"{package} repeat {index + 1}: {rel} exited 0 with a failed test")
                seen = {}
                for classname, name, result in cases:
                    seen[(classname, name)] = seen.get((classname, name), 0) + 1
                    ordinal = seen[(classname, name)]
                    key = (rel, classname, name if ordinal == 1 else f"{name} #{ordinal}")
                    by_test.setdefault(key, [None] * count)[index] = result
                    tests[result] = tests.get(result, 0) + 1
            seconds = max((r["seconds"] for r in records), default=0)
            summaries.append({"repeat": index + 1, "files": len(records), "file_verdicts": verdicts,
                              "reporter": reporter, "tests": tests,
                              "slowest_file_seconds": seconds})
        stable_red, flaky, stable_pass = [], [], 0
        for rel, outcomes in sorted(by_file.items()):
            results = [v or "NOTRUN" for v in outcomes]
            entry = {"file": rel, "test": f"{rel} {FILE_KEY}", "results": results}
            if len(set(results)) > 1:
                flaky.append(entry)
            elif results[0] != "OK":
                stable_red.append(entry)
        for (rel, classname, name), results in sorted(by_test.items()):
            outcomes = by_file[rel]
            # A repeat whose file wrote no report says nothing about the test (the file entry does).
            judged = [r or "NOTRUN" for r, has_report in zip(results, reported[rel]) if has_report]
            shown = [r or ("FILE_" + (outcomes[i] or "NOTRUN")) for i, r in enumerate(results)]
            label = " > ".join(p for p in (classname, name) if p)
            entry = {"file": rel, "classname": classname, "name": name, "test": f"{rel} {label}", "results": shown}
            distinct = set(judged)
            if len(distinct) > 1:
                flaky.append(entry)
            elif distinct and distinct <= GREEN:
                stable_pass += 1
            elif distinct:
                stable_red.append(entry)
        packages[package] = {"repeats": summaries, "tests": len(by_test), "files": len(by_file),
                             "stable_pass": stable_pass, "stable_red": stable_red, "flaky": flaky}
        for entry in flaky:
            flaky_lines.append({"suite": "webviews-tests", "product": "bun", "package": package,
                                "test": f"{package}/{entry['test']}", "pointer": None, "results": entry["results"],
                                "run_id": meta["run_id"], "sha": meta["sha"], "jobs": meta["jobs"]})
    red = any(p["stable_red"] for p in packages.values())
    receipt = dict(meta)
    receipt.update({"packages": packages, "errors": errors + meta.get("errors", []),
                    "ok": not errors and not meta.get("errors") and not red})
    code = 75 if receipt["errors"] else 1 if red else 0
    return receipt, flaky_lines, code


def write_results(receipt_path, flaky_path, receipt, flaky_lines):
    os.makedirs(os.path.dirname(os.path.abspath(receipt_path)), exist_ok=True)
    with open(receipt_path, "w", encoding="utf-8") as handle:
        json.dump(receipt, handle, indent=2, sort_keys=True)
        handle.write("\n")
    if flaky_lines and flaky_path:
        os.makedirs(os.path.dirname(os.path.abspath(flaky_path)), exist_ok=True)
        with open(flaky_path, "a", encoding="utf-8") as handle:
            for line in flaky_lines:
                handle.write(json.dumps(line, sort_keys=True) + "\n")


def report(receipt, receipt_path):
    for package, data in receipt["packages"].items():
        for rep in data["repeats"]:
            r = rep["reporter"]
            say(f"{package} repeat {rep['repeat']}: {rep['files']} files {rep['file_verdicts']}; bun reported "
                f"{r['tests']} tests, {r['failures']} failed, {r['skipped']} skipped")
        for entry in data["flaky"]:
            say(f"FLAKY: {package}/{entry['test']}: {' '.join(entry['results'])}")
        for entry in data["stable_red"]:
            say(f"RED: {package}/{entry['test']}: {' '.join(entry['results'])}")
        say(f"{package}: {data['files']} files, {data['tests']} tests, {data['stable_pass']} passed every repeat, "
            f"{len(data['stable_red'])} red, {len(data['flaky'])} flaky")
    for line in receipt["errors"]:
        say(f"error: {line}")
    say(f"receipt {receipt_path}")


# ---------------------------------------------------------------- self-test

STAND_IN_BUN = """#!/usr/bin/env bash
# Stand-in bun: test --timeout MS --reporter=junit --reporter-outfile=FILE ./NAME
out="${5#--reporter-outfile=}"
junit() { printf '<testsuites name="bun test" tests="1" failures="%s" skipped="0" assertions="1"><testsuite name="x"><testcase name="t" classname="c">%s</testcase></testsuite></testsuites>' "$1" "$2" > "$out"; }
case "$6" in
  ./pass.test.ts) junit 0 "" ;;
  ./fail.test.ts) junit 1 '<failure type="AssertionError"/>'; exit 1 ;;
  ./between.test.ts) printf '\\n# Unhandled error between tests\\n---\\nerror: x\\n'; junit 1 '<failure type="AssertionError"/>'; exit 1 ;;
  ./hang.test.ts) sleep 30 ;;
  ./crash.test.ts) kill -SEGV $$ ;;
  ./orphan.test.ts) (sleep 30 & echo $! > "$(dirname "$out")/../orphan.pid"); junit 0 "" ;;
  ./silent.test.ts) exit 0 ;;
  ./env.test.ts) [ -n "$CMUX_VITE_CACHE_DIR" ] && [ -e "$CMUX_VITE_CACHE_DIR/seeded" ] && junit 0 "" || exit 1 ;;
  ./block*)
    # A file that never ends, with a child that ignores SIGTERM; records both pids.
    started="$(dirname "$out")/../started"; name="${6#./}"
    mkdir -p "$started"
    bash -c 'trap "" TERM; exec sleep 60' &
    echo $! > "$started/$name.child"
    echo $$ > "$started/$name.tmp" && mv "$started/$name.tmp" "$started/$name.pid"
    sleep 60 ;;
esac
"""


def alive(pid):
    try:
        with open(f"/proc/{pid}/stat") as handle:
            return handle.read().split(") ")[1][0] != "Z"
    except OSError:
        return False


def interrupt_fixture(children, work):
    """The runner half of the interrupt self-test: six never-ending files, two at once."""
    args = argparse.Namespace(jobs=2, test_timeout_ms=5000, file_timeout=60, warm_attempts=1)
    files = [f"block{i}.test.ts" for i in range(6)]
    run_repeat(children, os.path.join(work, "bun"), "web", work, files, os.path.join(work, "repeat-1"),
               {"PATH": os.environ["PATH"]}, args)
    return 0


def interrupted_run(work, sig):
    """Start a runner on interrupt_fixture, send it `sig` once two files run, and return
    (exit code, files started, pids of those files and their children still alive)."""
    rdir = os.path.join(work, "repeat-1")
    shutil.rmtree(rdir, ignore_errors=True)
    started = os.path.join(rdir, "started")
    pid_files = lambda suffix: [os.path.join(started, n) for n in sorted(os.listdir(started))
                                if n.endswith(suffix)] if os.path.isdir(started) else []
    runner = subprocess.Popen(["bash", SELF, "--self-test-interrupt", work], stdin=subprocess.DEVNULL,
                              stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        deadline = time.monotonic() + 30
        while len(pid_files(".pid")) < 2 and runner.poll() is None and time.monotonic() < deadline:
            time.sleep(0.05)
        runner.send_signal(sig)
        try:
            code = runner.wait(timeout=30)
        except subprocess.TimeoutExpired:
            code = None
    finally:
        kill_group(runner.pid, signal.SIGKILL)
    pids = [int(open(path).read()) for path in pid_files(".pid") + pid_files(".child")]
    time.sleep(0.2)
    return code, len(pid_files(".pid")), [pid for pid in pids if alive(pid)]


def self_test():
    """The runner with a stand-in bun (pass, fail, hang, crash, leftover child, no
    report, an interrupted run), then the classifier on made-up repeats. Exit 0 when
    every check holds."""
    work = tempfile.mkdtemp(prefix="webviews-tests-selftest-")
    try:
        bun = os.path.join(work, "bun")
        with open(bun, "w") as handle:
            handle.write(STAND_IN_BUN)
        os.chmod(bun, 0o755)
        rdir = os.path.join(work, "repeat-1")
        for sub in ("logs", "junit"):
            os.makedirs(os.path.join(rdir, sub))
        seed = os.path.join(work, "seed")
        os.makedirs(seed)
        open(os.path.join(seed, "seeded"), "w").close()
        got = {}
        for name in ("pass", "fail", "between", "hang", "crash", "orphan", "silent", "env"):
            kwargs = {"cache": os.path.join(work, "cache"), "seed": seed} if name == "env" else {}
            got[name] = run_file(Children(), bun, work, f"{name}.test.ts", rdir, {"PATH": os.environ["PATH"]}, 5000,
                                 2 if name == "hang" else 20, **kwargs)
        orphan_pid = int(open(os.path.join(rdir, "orphan.pid")).read())
        time.sleep(0.2)
        verdicts = {k: v["verdict"] for k, v in got.items()}
        checks = {
            "runner verdicts": verdicts == {"pass": "PASS", "fail": "FAIL", "between": "FAIL", "hang": "TIMEOUT",
                                            "crash": "CRASH", "orphan": "PASS", "silent": "PASS", "env": "PASS"},
            "a hung file is killed at its limit": got["hang"]["seconds"] < 15,
            "the file's process group dies with it": not alive(orphan_pid),
            "a dev-server file gets a copy of the warm cache": got["env"]["vite_cache"] == "warm",
            "no report when bun wrote none": got["silent"]["junit"] is None and got["pass"]["junit"] is not None,
            "bun's error between tests is counted from the log": got["between"]["outside_errors"] == 1
                and got["fail"]["outside_errors"] == 0,
        }
        for sig, want in ((signal.SIGTERM, 143), (signal.SIGINT, 130)):
            name = signal.Signals(sig).name
            code, count, left = interrupted_run(work, sig)
            checks[f"{name}: the runner exits {want}"] = code == want
            checks[f"{name}: no new file starts after it"] = count == 2
            checks[f"{name}: no file or child of one is left alive"] = not left

        # Classifier: three repeats of made-up reports.
        def junit_file(tag, cases):
            path = os.path.join(work, f"{tag}.xml")
            body = "".join(f'<testcase name="{n}" classname="g">{x}</testcase>' for n, x in cases)
            fails = sum(1 for _, x in cases if "failure" in x)
            with open(path, "w") as handle:
                handle.write(f'<testsuites name="bun test" tests="{len(cases)}" failures="{fails}" skipped="0" '
                             f'assertions="0"><testsuite name="f">{body}</testsuite></testsuites>')
            return path
        ok, bad, slow, todo = "", '<failure type="AssertionError"/>', '<failure type="TimeoutError"/>', '<skipped message="TODO"/>'

        def rec(rel, verdict, junit, outside=0):
            return {"file": rel, "verdict": verdict, "exit": 0 if verdict == "PASS" else 1, "seconds": 1,
                    "limit": 120, "log": "", "junit": junit, "outside_errors": outside, "vite_cache": None}
        repeats = []
        for i, (a_flip, b_verdict, c_ok) in enumerate((("ok", "PASS", True), ("bad", "TIMEOUT", True), ("ok", "PASS", False))):
            a = junit_file(f"a{i}", [("steady", ok), ("flip", ok if a_flip == "ok" else bad), ("red", bad),
                                     ("slow", slow if i == 0 else bad), ("later", todo), ("dup", ok), ("dup", ok)])
            records = [rec("a.test.ts", "FAIL", a),
                       rec("b.test.ts", b_verdict, junit_file(f"b{i}", [("t", ok)]) if b_verdict == "PASS" else None),
                       rec("c.test.ts", "PASS" if c_ok else "FAIL", junit_file(f"c{i}", [("t", ok)]) if c_ok else None),
                       # d: one test flips, the file always writes a report.
                       rec("d.test.ts", "FAIL" if i == 1 else "PASS", junit_file(f"d{i}", [("t", bad if i == 1 else ok)])),
                       # e: a test red in every repeat, and in repeat 2 an error between tests.
                       rec("e.test.ts", "FAIL", junit_file(f"e{i}", [("red", bad), ("fine", ok)]), 1 if i == 1 else 0),
                       # f: in repeat 2 the file fails with every test green (an error outside a test).
                       rec("f.test.ts", "FAIL" if i == 1 else "PASS", junit_file(f"f{i}", [("t", ok)]))]
            repeats.append(records)
        meta = {"run_id": "selftest", "sha": "0" * 40, "jobs": 300}
        receipt, lines, code = analyze(meta, {"webviews": repeats})
        pkg = receipt["packages"]["webviews"]
        flaky = {e["test"]: e["results"] for e in pkg["flaky"]}
        red = {e["test"] for e in pkg["stable_red"]}
        checks.update({
            "exit 1 (a test red in every repeat)": code == 1 and not receipt["errors"],
            "flaky = flip, slow, d's t and the files with a file-level failure in one repeat": set(flaky) == {
                "a.test.ts g > flip", "a.test.ts g > slow", "b.test.ts (file)", "c.test.ts (file)",
                "d.test.ts g > t", "e.test.ts (file)", "f.test.ts (file)"},
            "TIMEOUT differs from FAIL": flaky.get("a.test.ts g > slow") == ["TIMEOUT", "FAIL", "FAIL"],
            "file entries show file-level outcomes": flaky.get("b.test.ts (file)") == ["OK", "TIMEOUT", "OK"]
                and flaky.get("c.test.ts (file)") == ["OK", "OK", "NOREPORT"]
                and flaky.get("e.test.ts (file)") == ["OK", "ERROR", "OK"]
                and flaky.get("f.test.ts (file)") == ["OK", "ERROR", "OK"],
            "a test that flips in a file that always reports is one flaky line":
                [l["test"] for l in lines if l["test"].startswith("webviews/d.test.ts")] == ["webviews/d.test.ts g > t"],
            "tests of a file without a report are not flakes": "b.test.ts g > t" not in flaky
                and "c.test.ts g > t" not in flaky,
            "red = the tests red in every repeat, not their files": red == {"a.test.ts g > red", "e.test.ts g > red"},
            "steady, dup, dup #2, later (TODO), b, c and f's t, e's fine pass": pkg["stable_pass"] == 8,
            "counts are bun's": pkg["repeats"][0]["reporter"]["tests"] == 13
                and pkg["repeats"][1]["reporter"]["tests"] == 12,
            "one flaky line per flaky test": sorted(l["test"] for l in lines) == sorted(f"webviews/{t}" for t in flaky),
        })
        # A file that exits 0 without a report is an error, not a pass.
        receipt, _, code = analyze(meta, {"web": [[rec("s.test.ts", "PASS", None)]] * 2})
        checks["exit 0 without a report is an error (75)"] = code == 75 and "wrote no JUnit file" in receipt["errors"][0]
        failed = [k for k, v in checks.items() if not v]
        for k in failed:
            say(f"self-test FAILED: {k}")
        if failed:
            say(f"runner records: {json.dumps(got, indent=1)}")
            return 1
        say(f"self-test: all {len(checks)} checks pass")
        return 0
    finally:
        shutil.rmtree(work, ignore_errors=True)


# ---------------------------------------------------------------- main

def positive(text):
    if not re.fullmatch(r"[1-9][0-9]*", text):
        raise argparse.ArgumentTypeError(f"needs a positive integer, got '{text}'")
    return int(text)


def parse(argv):
    parser = argparse.ArgumentParser(prog="webviews-tests.sh", add_help=False)
    parser.add_argument("--package", action="append", choices=PACKAGES)
    parser.add_argument("--jobs", type=positive, default=300)
    parser.add_argument("--repeats", type=positive, default=3)
    parser.add_argument("--out")
    parser.add_argument("--flaky-out", default="/tmp/oracle/flaky.jsonl")
    parser.add_argument("--filter")
    parser.add_argument("--test-timeout-ms", type=positive, default=30000)
    parser.add_argument("--file-timeout", type=positive, default=120)
    parser.add_argument("--warm-attempts", type=positive, default=3, help=argparse.SUPPRESS)
    parser.add_argument("--browser-lane", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--self-test-interrupt", metavar="DIR", help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if args.repeats < 2 and not args.self_test:
        raise UsageError("--repeats must be at least 2: one run cannot show a flake")
    try:
        args.pattern = re.compile(args.filter) if args.filter else None
    except re.error as exc:
        raise UsageError(f"--filter is not a regex: {exc}")
    args.package = args.package or list(PACKAGES)
    return args


def check(command, cwd, log):
    with open(log, "ab") as output:
        output.write(f"$ {' '.join(command)}\n".encode())
        output.flush()
        code = subprocess.run(command, cwd=cwd, stdin=subprocess.DEVNULL, stdout=output,
                              stderr=subprocess.STDOUT).returncode
    if code:
        raise SetupError(f"`{' '.join(command)}` in {os.path.relpath(cwd, ROOT)} exited {code}; see {log}")


def on_signal(signum, frame):
    # Once only: a second signal must not cut the cleanup of the first short.
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, signal.SIG_IGN)
    raise Interrupted(signum)


def main(argv):
    args = parse(argv)
    if args.self_test:
        return self_test()
    children = Children()
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, on_signal)
    try:
        if args.self_test_interrupt:
            return interrupt_fixture(children, args.self_test_interrupt)
        return run(children, args)
    except Interrupted as exc:
        children.stop()
        say(f"stopped by {exc}: killed the process groups of {len(children.killed)} running files; no receipt")
        return 128 + exc.signum
    finally:
        children.stop()


def run(children, args):
    if os.uname().sysname != "Linux":
        raise UsageError("Linux only: never test on a person's Mac (CLAUDE.md)")
    os.nice(10)
    bun = shutil.which("bun")
    want = json.load(open(os.path.join(ROOT, "webviews/package.json")))["devEngines"]["packageManager"]["version"]
    have = subprocess.run([bun, "--version"], capture_output=True, text=True).stdout.strip() if bun else ""
    if have != want:
        raise SetupError(f"bun is '{have or 'missing'}', webviews/package.json pins {want}")
    sha = subprocess.run(["git", "-C", ROOT, "rev-parse", "HEAD"], check=True, capture_output=True, text=True).stdout.strip()
    dirty = bool(subprocess.run(["git", "-C", ROOT, "status", "--porcelain", "--", *args.package],
                                check=True, capture_output=True, text=True).stdout.strip())
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    run_id = f"{stamp}-{sha[:12]}"
    out = os.path.abspath(args.out or f"/tmp/oracle/webviews-tests/{run_id}")
    os.makedirs(out, exist_ok=True)
    # The child environment is fixed here: CI=true would turn the browser lane on, and
    # feat-cmux-next's style guards warn (ci-web.yml sets the same on its pushes).
    env = {k: v for k, v in os.environ.items() if k not in ("CI", "GITHUB_ACTIONS", "CMUX_BROWSER_TESTS")}
    env.setdefault("CMUX_NEXT_STYLE_GUARDS", "warn")
    if args.browser_lane:
        env["CMUX_BROWSER_TESTS"] = "1"
    say(f"sha {sha[:12]}{' (dirty)' if dirty else ''}, bun {have}, {args.jobs} files at once, "
        f"{args.repeats} repeats, out {out}")
    meta = {"schema": "cmux-webviews-tests/v1", "run_id": run_id, "sha": sha, "dirty": dirty,
            "host": socket.gethostname(), "cores": os.cpu_count(),
            "loadavg_at_start": " ".join(open("/proc/loadavg").read().split()[:3]),
            "started": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "bun": have, "jobs": args.jobs, "dev_server_jobs": max(1, args.jobs // 4), "repeats": args.repeats,
            "retries": 0, "test_timeout_ms": args.test_timeout_ms, "file_timeout_seconds": args.file_timeout,
            "filter": args.filter, "browser_lane": args.browser_lane,
            "style_guards": env["CMUX_NEXT_STYLE_GUARDS"], "install": {}, "errors": []}
    runs = {}
    for package in args.package:
        package_dir = os.path.join(ROOT, package)
        pdir = os.path.join(out, package)
        os.makedirs(pdir, exist_ok=True)
        started = time.monotonic()
        check([bun, "install", "--frozen-lockfile"], package_dir, os.path.join(pdir, "install.log"))
        if args.browser_lane and package == "webviews":
            check(["./node_modules/.bin/playwright", "install", "--only-shell", "chromium", "webkit"],
                  package_dir, os.path.join(pdir, "install.log"))
        meta["install"][package] = round(time.monotonic() - started, 1)
        files = test_files(package, package_dir, args.pattern)
        if not files:
            raise UsageError(f"no {package} test files{' match --filter' if args.pattern else ''}")
        say(f"{package}: installed in {meta['install'][package]:.0f} s; {len(files)} test files")
        runs[package] = []
        for repeat in range(1, args.repeats + 1):
            rdir = os.path.join(pdir, f"repeat-{repeat}")
            shutil.rmtree(rdir, ignore_errors=True)
            started = time.monotonic()
            records = run_repeat(children, bun, package, package_dir, files, rdir, env, args)
            with open(os.path.join(rdir, "files.jsonl"), "w", encoding="utf-8") as handle:
                for record in sorted(records, key=lambda r: r["file"]):
                    handle.write(json.dumps(record, sort_keys=True) + "\n")
            say(f"{package} repeat {repeat} of {args.repeats}: {time.monotonic() - started:.0f} s")
            meta.setdefault("seconds", {}).setdefault(package, []).append(round(time.monotonic() - started, 1))
            runs[package].append(records)
    receipt, flaky_lines, code = analyze(meta, runs)
    receipt_path = os.path.join(out, "receipt.json")
    write_results(receipt_path, args.flaky_out, receipt, flaky_lines)
    report(receipt, receipt_path)
    return code


try:
    sys.exit(main(sys.argv[3:]))
except UsageError as exc:
    say(f"error: {exc}")
    sys.exit(2)
except SetupError as exc:
    say(f"error: {exc}")
    sys.exit(75)
PY

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
# when the install, the bun version or a reporter file is wrong.
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


def say(text):
    print(f"webviews-tests: {text}", flush=True)


class UsageError(Exception):
    pass


class SetupError(Exception):
    pass


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


def run_file(bun, package_dir, rel, rdir, env, test_timeout_ms, limit, cache=None, seed=None):
    """Run one test file in its own process group; return its record. The group is
    killed after the file ends, so a dev server or browser it leaves behind dies with it."""
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
        child = subprocess.Popen(command, cwd=package_dir, env=child_env, stdin=subprocess.DEVNULL,
                                 stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
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
        kill_group(child.pid, signal.SIGKILL)
    if timed_out:
        verdict = "TIMEOUT"
    elif code < 0 or code >= 128:
        verdict = "CRASH"
    else:
        verdict = "PASS" if code == 0 else "FAIL"
    return {"file": rel, "verdict": verdict, "exit": code, "seconds": round(time.monotonic() - started, 3),
            "limit": limit, "log": log, "junit": junit if os.path.exists(junit) else None,
            "vite_cache": "warm" if seed else ("own" if cache else None)}


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


def pool(jobs, tasks, progress):
    """Run the (args, kwargs) tasks through run_file, at most `jobs` at once."""
    records = []
    if not tasks:
        return records
    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as executor:
        futures = [executor.submit(run_file, *args, **kwargs) for args, kwargs in tasks]
        for future in concurrent.futures.as_completed(futures):
            record = future.result()
            progress.tick(record)
            records.append(record)
    return records


def run_repeat(bun, package, package_dir, files, rdir, env, args):
    """One full run of a package's files. webviews dev-server files run after the rest:
    one file per Vite config warms that config's cache alone, then the rest run a quarter
    of the jobs at once on copies of it (scripts/ci/run-webviews-tests.sh says why)."""
    for sub in ("logs", "junit"):
        os.makedirs(os.path.join(rdir, sub), exist_ok=True)
    progress = Progress(f"{package} {os.path.basename(rdir)}", len(files))
    base = (bun, package_dir)
    rest = (env, args.test_timeout_ms)
    limit = lambda rel: file_limit(os.path.join(package_dir, rel), args.file_timeout)
    dev = [f for f in files if package == "webviews"
           and DEV_SERVER.search(open(os.path.join(package_dir, f), encoding="utf-8", errors="replace").read())]
    dev_set = set(dev)
    plain = [f for f in files if f not in dev_set]
    records = pool(args.jobs, [((*base, f, rdir, *rest, limit(f)), {}) for f in plain], progress)
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
            record = run_file(*base, rel, rdir, *rest, limit(rel), cache=seed)
            progress.tick(record)
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
        dev_jobs = max(1, args.jobs // 4)

        def run_and_drop(*a, **kw):
            record = run_file(*a, **kw)
            shutil.rmtree(kw["cache"], ignore_errors=True)
            return record

        for tasks, jobs in ((warm, dev_jobs), (cold, 1)):
            if not tasks:
                continue
            with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as executor:
                futures = [executor.submit(run_and_drop, *a, **kw) for a, kw in tasks]
                for future in concurrent.futures.as_completed(futures):
                    record = future.result()
                    progress.tick(record)
                    records.append(record)
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


def analyze(meta, runs):
    """runs: {package: [[file record, ...] per repeat]}. Returns (receipt, flaky lines, exit code).
    A test's flakiness is judged over the repeats in which its file wrote a report; a file
    that timed out, crashed or failed to load in some repeat is one (file) entry instead of
    one NOTRUN flake per test in it."""
    errors, packages, flaky_lines = [], {}, []
    for package, repeats in runs.items():
        count = len(repeats)
        by_test, by_file, summaries = {}, {}, []
        for index, records in enumerate(repeats):
            reporter = {"tests": 0, "failures": 0, "skipped": 0, "assertions": 0}
            tests, verdicts = {}, {}
            for record in sorted(records, key=lambda r: r["file"]):
                rel = record["file"]
                verdicts[record["verdict"]] = verdicts.get(record["verdict"], 0) + 1
                by_file.setdefault(rel, [None] * count)[index] = record["verdict"]
                if record["junit"] is None:
                    if record["verdict"] == "PASS":
                        errors.append(f"{package} repeat {index + 1}: {rel} exited 0 but bun wrote no JUnit file")
                    continue
                try:
                    counts, cases = read_junit(record["junit"])
                except (ET.ParseError, OSError, ValueError) as exc:
                    errors.append(f"{package} repeat {index + 1}: {rel}: unreadable JUnit file ({exc})")
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
        for rel, verdicts in sorted(by_file.items()):
            results = [v or "NOTRUN" for v in verdicts]
            entry = {"file": rel, "test": f"{rel} {FILE_KEY}", "results": results}
            if len(set(results)) > 1:
                flaky.append(entry)
            elif results[0] in RED:
                stable_red.append(entry)
        reported = {}
        for index, records in enumerate(repeats):
            for record in records:
                reported.setdefault(record["file"], [False] * count)[index] = record["junit"] is not None
        for (rel, classname, name), results in sorted(by_test.items()):
            file_verdicts = by_file[rel]
            # A repeat whose file wrote no report says nothing about the test (the file entry does).
            judged = [r or "NOTRUN" for r, has_report in zip(results, reported[rel]) if has_report]
            shown = [r or ("FILE_" + (file_verdicts[i] or "NOTRUN")) for i, r in enumerate(results)]
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

def self_test():
    """The runner with a stand-in bun (pass, fail, hang, crash, leftover child, no
    report), then the classifier on made-up repeats. Exit 0 when every check holds."""
    work = tempfile.mkdtemp(prefix="webviews-tests-selftest-")
    try:
        bun = os.path.join(work, "bun")
        with open(bun, "w") as handle:
            handle.write("""#!/usr/bin/env bash
# Stand-in bun: test --timeout MS --reporter=junit --reporter-outfile=FILE ./NAME
out="${5#--reporter-outfile=}"
junit() { printf '<testsuites name="bun test" tests="1" failures="%s" skipped="0" assertions="1"><testsuite name="x"><testcase name="t" classname="c">%s</testcase></testsuite></testsuites>' "$1" "$2" > "$out"; }
case "$6" in
  ./pass.test.ts) junit 0 "" ;;
  ./fail.test.ts) junit 1 '<failure type="AssertionError"/>'; exit 1 ;;
  ./hang.test.ts) sleep 30 ;;
  ./crash.test.ts) kill -SEGV $$ ;;
  ./orphan.test.ts) (sleep 30 & echo $! > "$(dirname "$out")/../orphan.pid"); junit 0 "" ;;
  ./silent.test.ts) exit 0 ;;
  ./env.test.ts) [ -n "$CMUX_VITE_CACHE_DIR" ] && [ -e "$CMUX_VITE_CACHE_DIR/seeded" ] && junit 0 "" || exit 1 ;;
esac
""")
        os.chmod(bun, 0o755)
        rdir = os.path.join(work, "repeat-1")
        for sub in ("logs", "junit"):
            os.makedirs(os.path.join(rdir, sub))
        seed = os.path.join(work, "seed")
        os.makedirs(seed)
        open(os.path.join(seed, "seeded"), "w").close()
        got = {}
        for name in ("pass", "fail", "hang", "crash", "orphan", "silent", "env"):
            kwargs = {"cache": os.path.join(work, "cache"), "seed": seed} if name == "env" else {}
            got[name] = run_file(bun, work, f"{name}.test.ts", rdir, {"PATH": os.environ["PATH"]}, 5000,
                                 2 if name == "hang" else 20, **kwargs)
        orphan_pid = int(open(os.path.join(rdir, "orphan.pid")).read())
        time.sleep(0.2)
        orphan_alive = os.path.exists(f"/proc/{orphan_pid}") and \
            open(f"/proc/{orphan_pid}/stat").read().split(") ")[1][0] != "Z"
        verdicts = {k: v["verdict"] for k, v in got.items()}
        checks = {
            "runner verdicts": verdicts == {"pass": "PASS", "fail": "FAIL", "hang": "TIMEOUT", "crash": "CRASH",
                                            "orphan": "PASS", "silent": "PASS", "env": "PASS"},
            "a hung file is killed at its limit": got["hang"]["seconds"] < 15,
            "the file's process group dies with it": not orphan_alive,
            "a dev-server file gets a copy of the warm cache": got["env"]["vite_cache"] == "warm",
            "no report when bun wrote none": got["silent"]["junit"] is None and got["pass"]["junit"] is not None,
        }

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

        def rec(rel, verdict, junit):
            return {"file": rel, "verdict": verdict, "exit": 0 if verdict == "PASS" else 1, "seconds": 1,
                    "limit": 120, "log": "", "junit": junit, "vite_cache": None}
        repeats = []
        for i, (a_flip, b_verdict, c_ok) in enumerate((("ok", "PASS", True), ("bad", "TIMEOUT", True), ("ok", "PASS", False))):
            a = junit_file(f"a{i}", [("steady", ok), ("flip", ok if a_flip == "ok" else bad), ("red", bad),
                                     ("slow", slow if i == 0 else bad), ("later", todo), ("dup", ok), ("dup", ok)])
            records = [rec("a.test.ts", "FAIL", a),
                       rec("b.test.ts", b_verdict, junit_file(f"b{i}", [("t", ok)]) if b_verdict == "PASS" else None),
                       rec("c.test.ts", "PASS" if c_ok else "FAIL", junit_file(f"c{i}", [("t", ok)]) if c_ok else None)]
            repeats.append(records)
        meta = {"run_id": "selftest", "sha": "0" * 40, "jobs": 300}
        receipt, lines, code = analyze(meta, {"webviews": repeats})
        pkg = receipt["packages"]["webviews"]
        flaky = {e["test"]: e["results"] for e in pkg["flaky"]}
        red = {e["test"] for e in pkg["stable_red"]}
        checks.update({
            "exit 1 (a test red in every repeat)": code == 1 and not receipt["errors"],
            "flaky = flip, slow, the timed-out file, the file that failed to load": set(flaky) == {
                "a.test.ts g > flip", "a.test.ts g > slow", "b.test.ts (file)", "c.test.ts (file)"},
            "TIMEOUT differs from FAIL": flaky.get("a.test.ts g > slow") == ["TIMEOUT", "FAIL", "FAIL"],
            "tests of a file without a report are not flakes": "b.test.ts g > t" not in flaky
                and "c.test.ts g > t" not in flaky,
            "red = red and the file that always fails": red == {"a.test.ts g > red", "a.test.ts (file)"},
            "steady, dup, dup #2, later (TODO), b and c's t pass": pkg["stable_pass"] == 6,
            "counts are bun's": pkg["repeats"][0]["reporter"]["tests"] == 9
                and pkg["repeats"][1]["reporter"]["tests"] == 8,
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


def main(argv):
    args = parse(argv)
    if args.self_test:
        return self_test()
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
            records = run_repeat(bun, package, package_dir, files, rdir, env, args)
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

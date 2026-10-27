#!/usr/bin/env bash
# Run the cmux-tui workspace tests on the Linux VM under cargo-nextest at 300
# test threads, three times with no retries, and list the flaky tests. A test
# is flaky when it passed in one repeat and not in another (FAIL, TIMEOUT, CRASH
# or not run), as in Chromium's flakiness model: a PASS and a failure for one
# test mean flaky (testing/flake_suppressor, typ's expectations;
# plans/cmux-next/chrome-parity/p15-oracle.md, MQ-flakes). A test that passed in
# no repeat is red, also when its failure kind changed (TIMEOUT, then FAIL).
# A retry would hide a flake, so there is none. Each repeat starts with no
# processes of the earlier ones: its commands carry their own marker in the
# environment, and the processes with that marker (daemons that the fixtures
# left) are stopped after the repeat, as the Rust gate does (cx-leo45,
# scripts/ci/lib/reap-leaked.py). The receipt records each repeat's leak count.
#
# Usage: scripts/cmux-next/linux/rust-tests.sh [--jobs N] [--repeats N] [--out DIR]
#            [--flaky-out FILE] [--filter FILTERSET]
#        scripts/cmux-next/linux/rust-tests.sh --self-test
#   --jobs       nextest test threads (default 300).
#   --repeats    full runs of the same build (default 3, at least 2).
#   --out        results directory (default /tmp/oracle/rust-tests/<UTC>-<sha12>).
#   --flaky-out  file that gets one JSON line per flaky test, appended (default
#                /tmp/oracle/flaky.jsonl, the MQ-flakes queue that oracle.py also feeds).
#   --filter     a nextest filterset (-E) to run a part of the workspace.
#   --self-test  check the result classifier on made-up JUnit files; builds nothing.
# Results: <out>/receipt.json, <out>/build.log,
#   <out>/repeat-<i>/{nextest.log,junit.xml,leaked.json,leaked.log}.
# Exit 0 when every test passed in at least one repeat (flaky tests do not fail
# the run; they go to --flaky-out), 1 when a test passed in no repeat, 2 on a usage
# error, 75 when the build or a run failed for another reason than a red test.
#
# Doc tests are not run: nextest does not run them (the Rust gate runs them as
# their own step). Needs cargo-nextest at the Rust gate's pin, which
# scripts/cmux-next/linux/setup-tools.sh installs. Linux only: never compile or
# test on a person's Mac (CLAUDE.md); a fleet Mac uses scripts/ci/cmux-tui-rust-check.sh.
# The build and the repeats run at nice 10: the VM also runs agents.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
workspace="$root/cmux-tui"
gate="$root/scripts/ci/cmux-tui-rust-check.sh"
reaper="$root/scripts/ci/lib/reap-leaked.py"

jobs=300 repeats=3 out="" flaky_out=/tmp/oracle/flaky.jsonl filter="" self_test=0

usage() {
  sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2
  exit 2
}
fail() { printf 'rust-tests: error: %s\n' "$*" >&2; exit "${2:-75}"; }
note() { printf 'rust-tests: %s\n' "$*"; }
need_value() { [[ $# -ge 2 && -n "$2" ]] || fail "$1 needs a value" 2; }
need_number() { [[ "$2" =~ ^[1-9][0-9]*$ ]] || fail "$1 needs a positive integer, got '$2'" 2; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --jobs) need_value "$@"; need_number "$1" "$2"; jobs="$2"; shift 2 ;;
    --repeats) need_value "$@"; need_number "$1" "$2"; repeats="$2"; shift 2 ;;
    --out) need_value "$@"; out="$2"; shift 2 ;;
    --flaky-out) need_value "$@"; flaky_out="$2"; shift 2 ;;
    --filter) need_value "$@"; filter="$2"; shift 2 ;;
    --self-test) self_test=1; shift ;;
    -h | --help) usage ;;
    *) printf 'rust-tests: unknown argument %s\n' "$1" >&2; usage ;;
  esac
done
((repeats >= 2)) || fail "--repeats must be at least 2: one run cannot show a flake" 2

# analyze RECEIPT FLAKY_OUT META_JSON RUNS_TSV: reads each repeat's nextest log
# and JUnit file (the RUNS_TSV rows: repeat, exit code, seconds, log, junit,
# the repeat's reap-leaked.py --json summary),
# writes the receipt and appends the flaky tests. Exit 0, 1 (a test red in every
# repeat) or 75 (a run that is not a test result).
analyze() {
  python3 -I - "$@" <<'PY'
import json, os, re, sys
import xml.etree.ElementTree as ET

receipt_path, flaky_path, meta_path, runs_path = sys.argv[1:5]
meta = json.load(open(meta_path, encoding="utf-8"))

def result(case):
    # nextest JUnit (quick-junit): no child = PASS; <failure type="test timeout">,
    # <failure type="test abort"> (a signal), other <failure>/<error> = FAIL.
    for child in case:
        if child.tag == "skipped":
            return "SKIP"
        if child.tag in ("failure", "error"):
            kind = (child.get("type") or "").lower()
            if "timeout" in kind:
                return "TIMEOUT"
            if "abort" in kind:
                return "CRASH"
            return "FAIL"
    return "PASS"

errors, runs, by_test = [], [], {}
rows = [line.rstrip("\n").split("\t") for line in open(runs_path, encoding="utf-8") if line.strip()]
for index, (repeat, code, seconds, log, junit, leaks) in enumerate(rows):
    run = {"repeat": int(repeat), "exit": int(code), "seconds": float(seconds), "log": log, "junit": junit}
    runs.append(run)
    # What the repeat left running (stopped before the next repeat); None when the check failed.
    try:
        run["leaked"] = json.load(open(leaks, encoding="utf-8"))
    except (OSError, ValueError):
        run["leaked"] = None
    if run["leaked"] is None or run["leaked"].get("still_running"):
        # The later repeats then ran beside this one's processes: not the same experiment.
        errors.append(f"repeat {repeat}: its leftover processes were not all stopped (see {os.path.dirname(leaks)}/leaked.log)")
    # nextest exits 0 (all passed) or 100 (a test failed); anything else is a build,
    # list or setup failure, and its JUnit file (if any) is not a full result.
    if run["exit"] not in (0, 100):
        errors.append(f"repeat {repeat}: nextest exit {code} (not a test result; see {log})")
        continue
    text = open(log, encoding="utf-8", errors="replace").read() if os.path.exists(log) else ""
    ids = re.findall(r"Nextest run ID ([0-9a-f-]{36})", text)
    if not os.path.exists(junit):
        errors.append(f"repeat {repeat}: no JUnit file at {junit}")
        continue
    root = ET.parse(junit).getroot()
    # The JUnit file is under the workspace's target/, shared with any other nextest
    # run in this checkout: it is this repeat's only when its uuid is the logged run ID.
    if not ids or root.get("uuid") != ids[-1]:
        errors.append(f"repeat {repeat}: JUnit uuid {root.get('uuid')} is not the logged run ID {ids[-1] if ids else None}")
        continue
    run["run_id"] = ids[-1]
    counts = {}
    for case in root.iter("testcase"):
        key = (case.get("classname", ""), case.get("name", ""))
        status = result(case)
        counts[status] = counts.get(status, 0) + 1
        by_test.setdefault(key, [None] * len(rows))[index] = status
    run["counts"] = counts
    if run["exit"] == 100 and all(k in ("PASS", "SKIP") for k in counts):
        errors.append(f"repeat {repeat}: nextest exit 100 but no failed test in its JUnit file")

stable_red, flaky, stable_pass = [], [], 0
if not errors:
    for (binary, name), results in sorted(by_test.items()):
        results = [r or "NOTRUN" for r in results]
        distinct = set(results)
        entry = {"binary_id": binary, "name": name, "test": f"{binary} {name}", "results": results}
        if distinct <= {"PASS", "SKIP"}:
            stable_pass += 1
        elif "PASS" in distinct:
            flaky.append(entry)
        else:  # no PASS in any repeat: red, whatever its failure kinds were
            stable_red.append(entry)

receipt = dict(meta)
receipt.update({
    "runs": runs,
    "tests": len(by_test),
    "stable_pass": stable_pass,
    "stable_red": stable_red,
    "flaky": flaky,
    "errors": errors,
    "ok": not errors and not stable_red,
})
os.makedirs(os.path.dirname(os.path.abspath(receipt_path)), exist_ok=True)
with open(receipt_path, "w", encoding="utf-8") as handle:
    json.dump(receipt, handle, indent=2, sort_keys=True)
    handle.write("\n")
if flaky and flaky_path:
    os.makedirs(os.path.dirname(os.path.abspath(flaky_path)), exist_ok=True)
    with open(flaky_path, "a", encoding="utf-8") as handle:
        for entry in flaky:
            handle.write(json.dumps({"suite": "rust-tests", "product": "rust", "test": entry["test"],
                                     "pointer": None, "results": entry["results"], "run_id": meta["run_id"],
                                     "sha": meta["sha"], "jobs": meta["jobs"]}, sort_keys=True) + "\n")

for run in runs:
    counts = ", ".join(f"{k} {v}" for k, v in sorted(run.get("counts", {}).items()))
    leaked = "leak check failed" if run["leaked"] is None else f"{run['leaked']['processes']} processes leaked"
    print(f"rust-tests: repeat {run['repeat']}: exit {run['exit']}, {run['seconds']:.0f} s, "
          f"{counts or 'no results'}; {leaked}")
for line in errors:
    print(f"rust-tests: error: {line}")
for entry in flaky:
    print(f"rust-tests: FLAKY: {entry['test']}: {' '.join(entry['results'])}")
for entry in stable_red[:40]:
    print(f"rust-tests: RED: {entry['test']}: {' '.join(entry['results'])}")
if len(stable_red) > 40:
    by_crate = {}
    for entry in stable_red:
        crate = entry["binary_id"].split("::", 1)[0]
        by_crate[crate] = by_crate.get(crate, 0) + 1
    crates = ", ".join(f"{name} {n}" for name, n in sorted(by_crate.items(), key=lambda kv: -kv[1]))
    print(f"rust-tests: {len(stable_red) - 40} more red tests in the receipt (red by crate: {crates})")
print(f"rust-tests: {len(by_test)} tests, {stable_pass} passed every repeat, {len(stable_red)} red, "
      f"{len(flaky)} flaky; receipt {receipt_path}")
sys.exit(75 if errors else 1 if stable_red else 0)
PY
}

# reap MARKER WHEN DIR: stops the processes whose environment holds MARKER (the
# daemons a repeat's tests left), writes DIR/leaked.json and DIR/leaked.log and
# prints the summary line. The marker alone selects them: the checkout is an
# agent's worktree, where other processes of the agent work too.
reap() {
  local rc=0
  rm -f "$3/leaked.json"
  python3 -I "$reaper" --marker "$1" --when "$2" --json "$3/leaked.json" >"$3/leaked.log" 2>&1 || rc=$?
  note "$(head -n 1 "$3/leaked.log")"
  return "$rc"
}

# reap_self_test: reap stops a daemon of the marked commands that left their
# session (setsid -f, as the fixtures' daemons do) and keeps a process without
# the marker. Needs Linux's setsid.
reap_self_test() {
  if [[ "$(uname -s)" != Linux ]] || ! command -v setsid >/dev/null; then
    note "self-test: the leak check needs Linux and setsid; not run"
    return 0
  fi
  local marker="CMUX_RUST_TESTS_SELFTEST=$$-$RANDOM" other result
  mkdir -p "$dir/reap" "$dir/reap-again"
  env "$marker" setsid -f sleep 120 >/dev/null 2>&1
  sleep 120 >/dev/null 2>&1 &
  other=$!
  reap "$marker" "in the self-test" "$dir/reap" >/dev/null || true
  reap "$marker" "in the self-test, again" "$dir/reap-again" >/dev/null || true
  result="$(python3 -I -c 'import json, sys; print(*(json.load(open(p))["processes"] for p in sys.argv[1:]))' \
    "$dir/reap/leaked.json" "$dir/reap-again/leaked.json" 2>/dev/null || echo missing)"
  local alive=0
  kill -0 "$other" 2>/dev/null && alive=1
  kill "$other" 2>/dev/null || true
  if [[ "$result" != "1 0" || "$alive" != 1 ]]; then
    note "self-test FAILED: the leak check stops the marked daemon only (leaked: $result, unmarked alive: $alive)"
    return 1
  fi
}

# self_test: the classifier on made-up repeats; exit 0 when every verdict matches.
self_test() {
  local dir rc
  dir="$(mktemp -d "${TMPDIR:-/tmp}/rust-tests-selftest.XXXXXX")"
  trap 'rm -rf "$dir"' EXIT
  python3 -I - "$dir" <<'PY'
import json, os, sys
d = sys.argv[1]
case = {
    "PASS": "",
    "FAIL": '<failure type="test failure with exit code 101" message="panicked"/>',
    "TIMEOUT": '<failure type="test timeout"/>',
    "CRASH": '<failure type="test abort" message="process aborted with signal 6 (SIGABRT)"/>',
}
# test -> result per repeat (None: not in that repeat's list)
plan = {
    "steady": ["PASS", "PASS", "PASS"],
    "flip": ["PASS", "FAIL", "PASS"],
    "red": ["FAIL", "FAIL", "FAIL"],
    "slow": ["TIMEOUT", "FAIL", "FAIL"],
    "hang": ["PASS", "TIMEOUT", "PASS"],
    "crashy": ["CRASH", "CRASH", "CRASH"],
    "gone": ["PASS", None, "PASS"],
}
def write(folder, repeat, uuid, logged, results, code=100, still_running=0):
    """One repeat's nextest.log, junit.xml and leaked.json (repeat N leaked N processes); returns its runs.tsv row."""
    path = f"{d}/{folder}"
    os.makedirs(path, exist_ok=True)
    json.dump({"when": f"after repeat {repeat}", "processes": repeat, "ptys_reclaimed": 0,
               "still_running": still_running, "by_command": {}}, open(f"{path}/leaked.json", "w"))
    body = "".join(f'<testcase classname="crate::bin" name="{t}">{case[r]}</testcase>' for t, r in results.items() if r)
    open(f"{path}/junit.xml", "w").write(f'<testsuites uuid="{uuid}"><testsuite name="crate::bin">{body}</testsuite></testsuites>')
    open(f"{path}/nextest.log", "w").write(f" Nextest run ID {logged} with nextest profile: ci\n")
    return f"{repeat}\t{code}\t1\t{path}/nextest.log\t{path}/junit.xml\t{path}/leaked.json\n"
ids = [f"0000000{i}-0000-4000-8000-000000000000" for i in range(4)]
rows = [write(f"r{i}", i + 1, ids[i], ids[i], {t: v[i] for t, v in plan.items()}) for i in range(3)]
open(f"{d}/main.tsv", "w").write("".join(rows))
# A JUnit file another run wrote (its uuid is not the logged run ID), and a build failure.
stale = write("stale", 1, ids[3], ids[0], {"steady": "PASS"}, code=0)
build = write("build", 2, ids[1], ids[1], {}, code=101)
open(f"{d}/stale.tsv", "w").write(stale + rows[1])
open(f"{d}/build.tsv", "w").write(rows[0] + build)
# A repeat whose leftovers survived the reaping: the later repeats ran beside them.
alive = write("alive", 1, ids[0], ids[0], {"steady": "PASS"}, code=0, still_running=2)
open(f"{d}/alive.tsv", "w").write(alive + rows[1])
json.dump({"run_id": "selftest", "sha": "0" * 40, "jobs": 300}, open(f"{d}/meta.json", "w"))
PY
  local ok=1
  rc=0; analyze "$dir/main.json" "$dir/flaky.jsonl" "$dir/meta.json" "$dir/main.tsv" >"$dir/main.out" || rc=$?
  python3 -I - "$dir" "$rc" <<'PY' || ok=0
import json, sys
d, rc = sys.argv[1], int(sys.argv[2])
r = json.load(open(f"{d}/main.json"))
flaky = {e["name"]: e["results"] for e in r["flaky"]}
red = {e["name"]: e["results"] for e in r["stable_red"]}
lines = [json.loads(l) for l in open(f"{d}/flaky.jsonl")]
checks = {
    "exit 1 (a test that passed in no repeat)": rc == 1,
    "flaky = flip, hang, gone (a PASS and a failure)": set(flaky) == {"flip", "hang", "gone"},
    "a missing result is NOTRUN": flaky.get("gone") == ["PASS", "NOTRUN", "PASS"],
    "a PASS and a TIMEOUT are flaky": flaky.get("hang") == ["PASS", "TIMEOUT", "PASS"],
    "red = red, slow, crashy (no PASS in any repeat)": set(red) == {"red", "slow", "crashy"},
    "a red test keeps each repeat's result": red.get("slow") == ["TIMEOUT", "FAIL", "FAIL"],
    "each repeat records its leak count": [run["leaked"]["processes"] for run in r["runs"]] == [1, 2, 3],
    "steady passes": r["stable_pass"] == 1,
    "one flaky.jsonl line per flaky test": sorted(l["test"] for l in lines) == sorted(f"crate::bin {n}" for n in flaky),
    "flaky line names the suite": all(l["suite"] == "rust-tests" and l["run_id"] == "selftest" for l in lines),
    "not ok": r["ok"] is False and not r["errors"],
}
bad = [k for k, v in checks.items() if not v]
for k in bad:
    print(f"rust-tests: self-test FAILED: {k}")
sys.exit(1 if bad else 0)
PY
  rc=0; analyze "$dir/stale.json" "$dir/stale.jsonl" "$dir/meta.json" "$dir/stale.tsv" >"$dir/stale.out" || rc=$?
  if [[ "$rc" != 75 || -e "$dir/stale.jsonl" ]] || ! grep -q 'is not the logged run ID' "$dir/stale.out"; then
    note "self-test FAILED: a JUnit file of another run is refused (exit $rc)"; ok=0
  fi
  rc=0; analyze "$dir/build.json" "$dir/build.jsonl" "$dir/meta.json" "$dir/build.tsv" >"$dir/build.out" || rc=$?
  if [[ "$rc" != 75 ]] || ! grep -q 'nextest exit 101' "$dir/build.out"; then
    note "self-test FAILED: a nextest exit that is not a test result is an error (exit $rc)"; ok=0
  fi
  rc=0; analyze "$dir/alive.json" "$dir/alive.jsonl" "$dir/meta.json" "$dir/alive.tsv" >"$dir/alive.out" || rc=$?
  if [[ "$rc" != 75 ]] || ! grep -q 'leftover processes were not all stopped' "$dir/alive.out"; then
    note "self-test FAILED: a repeat whose leftovers survived is an error (exit $rc)"; ok=0
  fi
  reap_self_test || ok=0
  if ((ok)); then note "self-test: all checks pass"; exit 0; fi
  exit 1
}

if ((self_test)); then self_test; fi

[[ "$(uname -s)" == Linux ]] || fail "Linux only: never compile or test on a person's Mac; use scripts/ci/cmux-tui-rust-check.sh on the fleet" 2
want="$(sed -nE 's/^nextest_version=([0-9.]+)$/\1/p' "$gate")"
[[ -n "$want" ]] || fail "no nextest_version= line in $gate"
have="$(cargo nextest --version 2>/dev/null | head -n 1 || true)"
[[ "$have" == "cargo-nextest $want "* ]] \
  || fail "cargo nextest is '${have:-missing}', not $want; run scripts/cmux-next/linux/setup-tools.sh"

sha="$(git -C "$root" rev-parse HEAD)"
dirty=false
[[ -z "$(git -C "$root" status --porcelain -- cmux-tui)" ]] || dirty=true
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
out="${out:-/tmp/oracle/rust-tests/$stamp-${sha:0:12}}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

# nextest writes the JUnit file under the workspace's target/, not CARGO_TARGET_DIR.
junit="$workspace/target/nextest/ci/junit.xml"
select=(--workspace)
[[ -z "$filter" ]] || select+=(-E "$filter")

cd "$workspace"
note "sha ${sha:0:12}$([[ "$dirty" == false ]] || echo ' (dirty)'), $jobs test threads, $repeats repeats, out $out"
started="$(date -u +%FT%TZ)"
load="$(cut -d' ' -f1-3 /proc/loadavg)"
t0="$SECONDS"
# One build for every repeat, at low priority: the VM also runs agents.
if ! nice -n 10 cargo nextest run "${select[@]}" --locked --profile ci --no-run >"$out/build.log" 2>&1; then
  tail -n 40 "$out/build.log" >&2
  fail "the build failed; see $out/build.log"
fi
build_seconds=$((SECONDS - t0))
note "built in ${build_seconds} s"

# Each repeat's commands carry their own marker, so the next repeat starts with
# none of its processes; an interrupted run stops the current repeat's too.
run_tag="$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')-$$"
active_marker="" active_dir=""
reap_at_exit() { [[ -z "$active_marker" ]] || reap "$active_marker" "at exit" "$active_dir" || true; }
trap reap_at_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
: >"$out/runs.tsv"
for ((i = 1; i <= repeats; i++)); do
  dir="$out/repeat-$i"
  mkdir -p "$dir"
  rm -f "$junit"
  t0="$SECONDS"
  rc=0
  active_marker="CMUX_RUST_TESTS_RUN=$run_tag-$i" active_dir="$dir"
  nice -n 10 env "$active_marker" cargo nextest run "${select[@]}" --locked --profile ci --no-tests=fail \
    --no-fail-fast --retries 0 --test-threads "$jobs" >"$dir/nextest.log" 2>&1 || rc=$?
  seconds=$((SECONDS - t0))
  [[ ! -f "$junit" ]] || cp "$junit" "$dir/junit.xml"
  note "repeat $i of $repeats: nextest exit $rc in $seconds s ($(grep -m1 -oE '[0-9]+ tests? run:.*' "$dir/nextest.log" || echo 'no summary'))"
  reap "$active_marker" "after repeat $i" "$dir" || note "the leak check after repeat $i failed; see $dir/leaked.log"
  active_marker=""
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$i" "$rc" "$seconds" "$dir/nextest.log" "$dir/junit.xml" "$dir/leaked.json" >>"$out/runs.tsv"
done

python3 -I - "$out/meta.json" "$stamp-${sha:0:12}" "$sha" "$dirty" "$load" "$started" "$have" \
  "$(rustc --version)" "$jobs" "$repeats" "$filter" "$build_seconds" <<'PY'
import json, os, sys
path, run_id, sha, dirty, load, started, nextest, rustc, jobs, repeats, filterset, build_seconds = sys.argv[1:]
with open(path, "w", encoding="utf-8") as handle:
    json.dump({
        "schema": "cmux-rust-tests/v1",
        "run_id": run_id,
        "sha": sha,
        "dirty": dirty == "true",
        "host": os.uname().nodename,
        "cores": os.cpu_count(),
        "loadavg_at_start": load,
        "started": started,
        "nextest": nextest,
        "rustc": rustc,
        "jobs": int(jobs),
        "repeats": int(repeats),
        "retries": 0,
        "nice": 10,
        "leak_check": "after each repeat, its processes (marker CMUX_RUST_TESTS_RUN=<tag>-<repeat>) are stopped: runs[].leaked",
        "filter": filterset or None,
        "build_seconds": int(build_seconds),
        "doc_tests": "not run (nextest does not run doc tests)",
    }, handle)
PY
analyze "$out/receipt.json" "$flaky_out" "$out/meta.json" "$out/runs.tsv"

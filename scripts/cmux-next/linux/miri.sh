#!/usr/bin/env bash
# Runs the oracle's pure, unsafe-heavy Rust crates under Miri on the Linux VM
# and writes one receipt (p15-oracle-219):
#   cmux-layout-reducer-ffi  own workspace, C ABI over the sidebar reducer
#   cmux-rd-ffi              own workspace, C ABI of the remote desktop core
#   cmux-palette-ranker      cmux-tui workspace, ranker and its C ABI
#   cmux-layout-reducer      cmux-tui workspace, the pure reducer
# Each crate runs through `cargo +NIGHTLY miri nextest run` in the Miri lane of
# cmux-tui/scripts/sanitize.sh, so a Miri error becomes a signed finding there.
# Chromium has no Miri lane (its unsafe code is C++ under ASan/MSan/TSan,
# build/config/sanitizers/BUILD.gn); Miri is the Rust equivalent for code
# that only crosses its own C ABI.
#
# Acceptance: a test that Miri cannot run says why. The only way to skip one is
# #[cfg_attr(miri, ignore = "<reason>")]. A bare cfg_attr(miri, ignore) or a
# cfg(miri), cfg(not(miri)) or cfg!(miri) without a `// miri:` reason on the line before
# fails the run, and so does a test that hits an operation Miri cannot run
# and has no such ignore (sanitize.sh alone counts that as passed).
#
# The nightly is [nightly].channel in tool-versions.toml (setup-tools.sh
# installs it with miri and rust-src); sanitize.sh runs the one in
# cmux-tui/sanitize/rust-toolchain.toml, so this script refuses to run when
# the two differ. A workspace with no committed Cargo.lock gets one for the
# run, removed after, as cmux-next.yml does for cmux-layout-reducer-ffi.
#
# Usage: scripts/cmux-next/linux/miri.sh [--crate NAME]... [--work-dir DIR]
#          [--receipt FILE] [--check] [--list] [-- extra nextest args]
#   --check    only check the ignore reasons (no build); exit 1 on a problem
#   --list     print the toolchain, crates and Miri ignores, run nothing
# Exit: 0 when every crate passed and every ignore has a reason, 1 otherwise,
# 2 on a usage or setup error.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
tui="$root/cmux-tui"
pins="$here/tool-versions.toml"
lane_toolchain="$tui/sanitize/rust-toolchain.toml"
sanitize="$tui/scripts/sanitize.sh"

# NAME WORKSPACE (relative to cmux-tui). One sanitize.sh run per workspace.
CRATES=(
  "cmux-layout-reducer-ffi crates/cmux-layout-reducer-ffi"
  "cmux-rd-ffi crates/cmux-rd-ffi"
  "cmux-palette-ranker ."
  "cmux-layout-reducer ."
)

die() {
  printf 'miri.sh: %s\n' "$*" >&2
  exit 2
}

selected=()
work_dir="${CMUX_MIRI_WORK_DIR:-$tui/target/miri}"
receipt=""
check_only=0
list_only=0
extra=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --crate) [[ $# -ge 2 ]] || die "--crate needs a value"; selected+=("$2"); shift 2 ;;
    --crate=*) selected+=("${1#*=}"); shift ;;
    --work-dir) [[ $# -ge 2 ]] || die "--work-dir needs a value"; work_dir="$2"; shift 2 ;;
    --work-dir=*) work_dir="${1#*=}"; shift ;;
    --receipt) [[ $# -ge 2 ]] || die "--receipt needs a value"; receipt="$2"; shift 2 ;;
    --receipt=*) receipt="${1#*=}"; shift ;;
    --check) check_only=1; shift ;;
    --list) list_only=1; shift ;;
    -h | --help) sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    --) shift; extra=("$@"); break ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done

rows=()
for row in "${CRATES[@]}"; do
  read -r name _ <<<"$row"
  if [[ ${#selected[@]} -eq 0 || " ${selected[*]} " == *" $name "* ]]; then rows+=("$row"); fi
done
for name in ${selected[@]+"${selected[@]}"}; do
  [[ " ${CRATES[*]} " == *" $name "* ]] || die "unknown crate '$name' (see --list)"
done

toml_channel() {
  python3 -I - "$1" "$2" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as f:
    value = tomllib.load(f)
for part in sys.argv[2].split("."):
    value = value[part]
print(value)
PY
}
channel="$(toml_channel "$pins" nightly.channel)"
lane_channel="$(toml_channel "$lane_toolchain" toolchain.channel)"
[[ "$channel" == "$lane_channel" ]] \
  || die "tool-versions.toml pins $channel but cmux-tui/sanitize/rust-toolchain.toml pins $lane_channel; move both together"

# crate_dir NAME prints the crate's folder relative to the repo root.
crate_dir() { printf 'cmux-tui/crates/%s' "$1"; }

# ignores.py: lists every Miri skip in the selected crates as JSON lines and
# exits 1 when one has no reason.
scan_ignores() {
  local dirs=() row name
  for row in "${rows[@]}"; do
    read -r name _ <<<"$row"
    dirs+=("$(crate_dir "$name")")
  done
  python3 -I - "$root" "$@" "${dirs[@]}" <<'PY'
import json, re, subprocess, sys
root, *rest = sys.argv[1:]
mode, dirs = rest[0], rest[1:]
files = subprocess.run(["git", "-C", root, "ls-files", "--", *(f"{d}/*.rs" for d in dirs)],
                       check=True, capture_output=True, text=True).stdout.split()
# Whole-file patterns: rustfmt splits a long attribute over several lines.
IGNORE = re.compile(r'cfg_attr\(\s*miri\s*,\s*ignore\s*(?:=\s*"((?:[^"\\]|\\.)*)")?\s*,?\s*\)')
CFG = re.compile(r'\bcfg(?:_attr)?!?\(\s*(?:not\(\s*)?miri\b')
FN = re.compile(r'\bfn\s+([A-Za-z0-9_]+)')
records, problems = [], 0
for path in files:
    text = open(f"{root}/{path}", encoding="utf-8").read()
    lines = text.splitlines()
    for found in CFG.finditer(text):
        number = text.count("\n", 0, found.start()) + 1
        if lines[number - 1].lstrip().startswith("//"):
            continue
        item = FN.search(text, found.end())
        test = item.group(1) if item else ""
        match = IGNORE.match(text, found.start())
        if match:
            reason = re.sub(r"\s+", " ", match.group(1) or "").strip()
            kind = "ignore"
        else:
            above = lines[number - 2].strip() if number > 1 else ""
            reason = above.removeprefix("// miri:").strip() if above.startswith("// miri:") else ""
            kind = "cfg"
        record = {"file": path, "line": number, "kind": kind, "item": test, "reason": reason}
        if not reason:
            problems += 1
            want = 'ignore = "<why Miri cannot run it>"' if kind == "ignore" else "a `// miri: <why>` line above"
            record["problem"] = f"Miri skip with no reason; give it {want}"
            print(f"miri.sh: {path}:{number}: {record['problem']}", file=sys.stderr)
        records.append(record)
if mode == "--json":
    for record in records:
        print(json.dumps(record, sort_keys=True))
else:
    for r in records:
        print(f"  {r['file']}:{r['line']} {r['item'] or '-'}: {r['reason'] or 'NO REASON'}")
sys.exit(1 if problems else 0)
PY
}

if [[ $list_only -eq 1 ]]; then
  echo "toolchain $channel (tool-versions.toml, cmux-tui/sanitize/rust-toolchain.toml)"
  for row in "${rows[@]}"; do
    read -r name ws <<<"$row"
    echo "crate $name (workspace cmux-tui/$ws)"
  done
  echo "miri skips:"
  scan_ignores --text || true
  exit 0
fi
if [[ $check_only -eq 1 ]]; then
  scan_ignores --text >/dev/null
  echo "miri.sh: every Miri skip has a reason"
  exit 0
fi

[[ "$(uname -s)" == Linux ]] || die "this lane runs on the Linux VM (sanitize.sh --sanitizer miri runs elsewhere)"
rustup run "$channel" cargo miri --version >/dev/null 2>&1 \
  || die "$channel has no miri; run scripts/cmux-next/linux/setup-tools.sh"
mkdir -p "$work_dir"
work_dir="$(cd "$work_dir" && pwd)"
receipt="${receipt:-$work_dir/miri-receipt.json}"
started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# Lockfiles this run created, removed on exit.
created_locks=()
# shellcheck disable=SC2317 # run by the EXIT trap
cleanup() {
  local lock
  for lock in ${created_locks[@]+"${created_locks[@]}"}; do rm -f "$lock"; done
}
trap cleanup EXIT

workspaces=()
for row in "${rows[@]}"; do
  read -r _ ws <<<"$row"
  [[ " ${workspaces[*]-} " == *" $ws "* ]] || workspaces+=("$ws")
done

status=0
groups=()
for ws in "${workspaces[@]}"; do
  ws_dir="$tui/$ws"
  group="$(basename "$(cd "$ws_dir" && pwd)")"
  lock="$ws_dir/Cargo.lock"
  if [[ ! -e "$lock" ]]; then
    git -C "$root" ls-files --error-unmatch "$lock" >/dev/null 2>&1 && die "$lock is tracked but missing"
    created_locks+=("$lock")
    (cd "$ws_dir" && rustup run "$channel" cargo metadata --format-version 1 >/dev/null) \
      || die "could not resolve $lock"
  fi
  crate_args=()
  for row in "${rows[@]}"; do
    read -r name row_ws <<<"$row"
    [[ "$row_ws" == "$ws" ]] && crate_args+=(--crate "$name")
  done
  groups+=("$work_dir/$group")
  "$sanitize" --sanitizer miri --workspace "$ws_dir" "${crate_args[@]}" --work-dir "$work_dir/$group" \
    --out "$work_dir/$group/findings.jsonl" --runs "$work_dir/$group/runs.jsonl" \
    ${extra[@]+-- "${extra[@]}"} || status=1
done

scan_ignores --json >"$work_dir/ignores.jsonl" || status=1

(python3 -I - "$receipt" "$work_dir/ignores.jsonl" "$channel" \
  "$(rustup run "$channel" cargo miri --version 2>/dev/null | head -n 1)" \
  "$(git -C "$root" rev-parse HEAD)" "$started" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(hostname)" \
  "${groups[@]}" <<'PY'
import json, sys
from pathlib import Path
receipt, ignores, channel, miri, commit, started, finished, host, *groups = sys.argv[1:]
read = lambda p: [json.loads(l) for l in Path(p).read_text().splitlines() if l.strip()] if Path(p).exists() else []
runs = [r for g in groups for r in read(f"{g}/runs.jsonl")]
findings = [f for g in groups for f in read(f"{g}/findings.jsonl")]
skips = read(ignores)
problems = [f"{s['file']}:{s['line']}: {s['problem']}" for s in skips if "problem" in s]
for run in runs:
    if run["result"] != "pass":
        problems.append(f"{run['crate']}: {run['result']} (log {run['log']})")
    for test in run.get("unsupported_tests", {}):
        problems.append(f"{run['crate']} {test}: hits an operation Miri cannot run; "
                        'skip it with #[cfg_attr(miri, ignore = "<why>")]')
ok = not problems and bool(runs)
doc = {"schema": "cmux.oracle.miri-receipt/1", "task": "p15-oracle-219", "ok": ok, "commit": commit,
       "toolchain": channel, "miri": miri, "host": host, "started": started, "finished": finished,
       "runs": runs, "findings": findings, "skips": skips, "problems": problems}
Path(receipt).write_text(json.dumps(doc, indent=2, sort_keys=True) + "\n")
for run in runs:
    print(f"miri: {run['crate']}: {run['result']} ({run['passed']} passed, {run['findings']} findings)", file=sys.stderr)
for problem in problems:
    print(f"miri: problem: {problem}", file=sys.stderr)
print(f"miri: {'ok' if ok else 'NOT ok'}; receipt {receipt}", file=sys.stderr)
sys.exit(0 if ok else 1)
PY
) || status=1
exit "$status"

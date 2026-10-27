#!/usr/bin/env bash
# The correctness oracle's parser fuzzing on the Linux VM (plans/cmux-next/chrome-parity/
# p15-oracle.md, task 220 and acceptance 13): each parser target of cmux-tui/fuzz runs in
# libFuzzer's fork mode for an hour, seeded from the requests the oracle corpus and the
# spec fixtures already hold, and the run writes one receipt. It reports; it is not a CI gate.
#
# Chromium runs each fuzz target in many processes against one shared corpus (ClusterFuzz;
# testing/libfuzzer/README.md, `-fork=N` in libFuzzer itself); here each target gets
# --fork child processes, and a crash, an out-of-memory, a timeout or a leak in any child
# stops that target with its artifact. scripts/cmux-next/rust-fuzz.sh does the build, the
# seeding (cmux-tui/fuzz/seeds.py), the limits and the verdicts; this script picks the
# targets and the fork count and writes the receipt.
#
# Usage: scripts/cmux-next/linux/fuzz.sh [--target NAME[,NAME...]] [--seconds N] [--fork N]
#            [--parallel N] [--out DIR] [--receipt FILE] [--build-only] [-- EXTRA LIBFUZZER FLAGS]
#   --target    the targets (default: the p15 parser targets control_request, resource_params,
#               journal_record, pane_frame, api_compat).
#   --seconds   fuzzing time per target (default 3600, the acceptance hour).
#   --fork      libFuzzer child processes per target (default 60).
#   --parallel  targets fuzzed at once (default: all of them). Cores used: parallel x fork.
#   --out       results directory (default artifacts/oracle-fuzz/<UTC>); rust-fuzz.sh's
#               summary.jsonl, logs, artifacts and the grown corpus are under it.
#   --receipt   the receipt (default <out>/receipt.json).
#   --build-only  build the targets and stop.
# The receipt is one JSON object: the commit, whether the tree was dirty, the toolchain,
# the fork count, the requested seconds, and per target the verdict, the artifact, the corpus
# size, libFuzzer's last status line and the fuzzing seconds it reports. "complete" is true
# only when every target fuzzed its full time clean.
# Exit status: rust-fuzz.sh's (0 all clean, 1 a finding, 2 usage, 75 build or run failure).
#
# Linux VM only: it compiles cmux-tui (CLAUDE.md, "Agents never compile or test on the
# person's Mac"). scripts/cmux-next/linux/setup-tools.sh installs cargo-fuzz and the nightly.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
TARGETS="control_request,resource_params,journal_record,pane_frame,api_compat"
SECONDS_PER_TARGET=3600 FORK=60 PARALLEL="" OUT="" RECEIPT="" BUILD_ONLY=0
EXTRA=()

usage() {
  sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2
  exit 2
}

need_value() {
  [[ $# -ge 2 && -n "$2" ]] || { echo "fuzz.sh: $1 needs a value" >&2; exit 2; }
}

need_number() {
  [[ "$2" =~ ^[1-9][0-9]*$ ]] || { echo "fuzz.sh: $1 needs a positive integer, got '$2'" >&2; exit 2; }
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --target) need_value "$@"; TARGETS="$2"; shift 2 ;;
    --seconds) need_value "$@"; need_number "$1" "$2"; SECONDS_PER_TARGET="$2"; shift 2 ;;
    --fork) need_value "$@"; need_number "$1" "$2"; FORK="$2"; shift 2 ;;
    --parallel) need_value "$@"; need_number "$1" "$2"; PARALLEL="$2"; shift 2 ;;
    --out) need_value "$@"; OUT="$2"; shift 2 ;;
    --receipt) need_value "$@"; RECEIPT="$2"; shift 2 ;;
    --build-only) BUILD_ONLY=1; shift ;;
    -h|--help) usage ;;
    --) shift; EXTRA=("$@"); break ;;
    *) echo "fuzz.sh: unknown argument '$1'" >&2; usage ;;
  esac
done

[[ "$(uname -s)" == Linux ]] || { echo "fuzz.sh: runs on the Linux VM only" >&2; exit 2; }

IFS=, read -r -a SELECTED <<<"$TARGETS"
PARALLEL="${PARALLEL:-${#SELECTED[@]}}"
RUST_FUZZ="$REPO/scripts/cmux-next/rust-fuzz.sh"

if [[ "$BUILD_ONLY" == 1 ]]; then
  exec "$RUST_FUZZ" --target "$TARGETS" --build-only
fi

OUT="${OUT:-$REPO/artifacts/oracle-fuzz/$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
RECEIPT="${RECEIPT:-$OUT/receipt.json}"
started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

status=0
"$RUST_FUZZ" --target "$TARGETS" --seconds "$SECONDS_PER_TARGET" --parallel "$PARALLEL" --out "$OUT" \
  -- -fork="$FORK" "${EXTRA[@]}" || status=$?

python3 -I - "$REPO" "$OUT" "$RECEIPT" "$started" "$SECONDS_PER_TARGET" "$FORK" "$status" "${SELECTED[@]}" <<'PY'
import datetime, json, re, subprocess, sys
from pathlib import Path

repo, out, receipt, started, seconds, fork, status, *targets = sys.argv[1:]
out, seconds, fork, status = Path(out), int(seconds), int(fork), int(status)

def run(*argv):
    try:
        return subprocess.run(argv, cwd=repo, capture_output=True, text=True, check=False).stdout.strip()
    except OSError:
        return ""

summary = out / "summary.jsonl"
rows = {}
if summary.is_file():
    for line in summary.read_text().splitlines():
        if line.strip():
            row = json.loads(line)
            rows[row["target"]] = row

def coverage(log):
    # libFuzzer's last status line: "#N ... cov: C ft: F corp: U/S ... exec/s: E".
    lines = [line for line in Path(log).read_text(errors="replace").splitlines()
             if re.search(r"\bcov: \d+", line)] if log and Path(log).is_file() else []
    return lines[-1].strip() if lines else None

entries = []
for target in targets:
    row = rows.get(target, {"target": target, "verdict": "none"})
    corpus = out / "corpus" / target
    last = coverage(row.get("log"))
    # Fork mode's status line counts fuzzing time ("time: 3600s"); the row's
    # "seconds" also counts the build `cargo fuzz run` does first.
    fuzzed = re.search(r"\btime: (\d+)s", last or "")
    fuzzed = int(fuzzed.group(1)) if fuzzed else 0
    entries.append({
        **row,
        "corpus_files": sum(1 for path in corpus.iterdir() if path.is_file()) if corpus.is_dir() else 0,
        "last_status": last,
        "fuzzed_seconds": fuzzed,
        "full_time": fuzzed >= seconds,
    })

channel = re.search(r'^channel\s*=\s*"([^"]+)"',
                    (Path(repo) / "cmux-tui/fuzz/rust-toolchain.toml").read_text(), re.M)
document = {
    "task": "p15-oracle-220",
    "commit": run("git", "rev-parse", "HEAD"),
    "dirty": bool(run("git", "status", "--porcelain", "--", "cmux-tui", "scripts/cmux-next")),
    "toolchain": channel.group(1) if channel else None,
    "cargo_fuzz": run("cargo", "fuzz", "--version"),
    "host_cores": len(__import__("os").sched_getaffinity(0)),
    "started": started,
    "finished": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "seconds_per_target": seconds,
    "fork": fork,
    "exit_status": status,
    "targets": entries,
    "complete": status == 0 and all(entry["verdict"] == "clean" and entry["full_time"] for entry in entries),
}
Path(receipt).parent.mkdir(parents=True, exist_ok=True)
Path(receipt).write_text(json.dumps(document, indent=2) + "\n")
print(f"fuzz.sh: receipt {receipt} (complete: {str(document['complete']).lower()})")
PY

exit "$status"

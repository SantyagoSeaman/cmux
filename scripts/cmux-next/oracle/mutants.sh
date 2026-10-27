#!/usr/bin/env bash
# mutants.sh: cargo-mutants over the daemon store with the oracle's daemon suites as the test
# (p15-oracle-191; spec plans/cmux-next/chrome-parity/p15-oracle.md, section 3F "Mutation strength",
# queue MQ-mutants). Linux VM only; every build and suite run goes through the runnerd queue.
#
#   mutants.sh run [REF] [--shards N | --shard K/N] [--workers W] [--suite S] [--file GLOB]... [--re RE]...
#                  [--limit N] [--out DIR] [--fresh] [--json]
#       1. The clean run: `linux_run.sh run SHA --suite S` (reused when runnerd already has it).
#       2. One runnerd job per shard (`mutants.py shard-job`): W worktrees at SHA, each with its own
#          cargo target dir; per mutant from `cargo mutants --list` (cmux-tui/.cargo/mutants.toml):
#          apply it, build `--profile oracle --features oracle`, `oracle.py run` the suites,
#          `oracle.py diff` against the clean run. Verdicts are cached per SHA, so a rerun after a
#          timeout or an error judges only what is left.
#       3. `mutants.py report`: DIR/mutants.json (counts per file and store area), DIR/survivors.json
#          (the MQ-mutants queue, sorted, no times) and DIR/receipt.json. --json prints the queue.
#       Defaults: REF HEAD, suites daemon-ops,walks, 2 shards (runnerd runs 2 jobs at once), 4 workers per
#       shard, DIR /tmp/oracle/mutants/<sha12>. --limit N judges the first N mutants of each shard (a smoke
#       run); --file GLOB and --re REGEX narrow the configured list. --fresh judges cached mutants again.
#   mutants.sh report SHARD_DIR... [--out DIR] [--json]
#       Merges finished shard dirs again (the last stdout line of each shard job).
#   mutants.sh list [--file GLOB]... [--re RE]... [--json]
#       The mutant list of this checkout, with counts per file. Runs no build.
#
# Exit: 0 every mutant judged (survivors are a queue, not a failure), 1 a mutant the run could not judge
# or a missing shard, 2 bad usage.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
SELF="$HERE/mutants.sh"
VM_JOBS=200
CARGO_JOBS=240
RUNNERD_SLOTS=2
JOB_TIMEOUT=$((72 * 3600))

die() {
  echo "mutants.sh: $*" >&2
  exit 2
}

mutants_py() {
  python3 -I "$HERE/mutants.py" "$@"
}

cmd_run() {
  local ref=HEAD shards=2 shard="" workers=4 suites="daemon-ops,walks" limit="" out="" fresh=() json=0
  local filters=()
  while (($#)); do
    case "$1" in
      --shards) shards="${2:?--shards needs N}"; shift 2 ;;
      --shard) shard="${2:?--shard needs K/N}"; shift 2 ;;
      --workers) workers="${2:?--workers needs W}"; shift 2 ;;
      --suite) suites="${2:?--suite needs a value}"; shift 2 ;;
      --file | --re) filters+=("$1" "${2:?$1 needs a value}"); shift 2 ;;
      --limit) limit="${2:?--limit needs N}"; shift 2 ;;
      --out) out="${2:?--out needs a dir}"; shift 2 ;;
      --fresh) fresh=(--fresh); shift ;;
      --json) json=1; shift ;;
      -*) die "run: unknown option $1" ;;
      *) ref="$1"; shift ;;
    esac
  done
  [[ "$shards" =~ ^[1-9][0-9]*$ && "$workers" =~ ^[1-9][0-9]*$ ]] || die "--shards and --workers are positive numbers"
  [[ -z "$limit" || "$limit" =~ ^[1-9][0-9]*$ ]] || die "--limit is a positive number"
  local indexes=()
  if [[ -n "$shard" ]]; then
    if ! [[ "$shard" =~ ^([0-9]+)/([1-9][0-9]*)$ ]] || ((BASH_REMATCH[1] >= BASH_REMATCH[2])); then
      die "--shard $shard: K/N, 0-based"
    fi
    shards="${BASH_REMATCH[2]}"
    indexes=("${BASH_REMATCH[1]}")
  else
    mapfile -t indexes < <(seq 0 $((shards - 1)))
  fi
  local sha
  sha="$(git -C "$REPO" rev-parse --verify --quiet "$ref^{commit}")" || die "$ref: not a commit"
  git -C "$REPO" cat-file -e "$sha:cmux-tui/.cargo/mutants.toml" 2>/dev/null \
    || die "$sha has no cmux-tui/.cargo/mutants.toml (p15-oracle-191)"
  out="${out:-/tmp/oracle/mutants/${sha:0:12}}"
  case "$out" in /*) ;; *) out="$PWD/$out" ;; esac
  case "$out/" in "$REPO"/*) die "--out $out: output stays outside the checkout" ;; esac

  # Shards run side by side up to runnerd's slots; the workers of all of them share the oracle's 200
  # case sessions and cargo's 240 jobs.
  local running=$((${#indexes[@]} < RUNNERD_SLOTS ? ${#indexes[@]} : RUNNERD_SLOTS))
  local parallel=$((running * workers))
  local jobs=$((VM_JOBS / parallel > 0 ? VM_JOBS / parallel : 1))
  local cargo_jobs=$((CARGO_JOBS / parallel > 0 ? CARGO_JOBS / parallel : 1))

  # The clean run uses the same --jobs as a mutant run, so its time sets a fair suite timeout.
  local base_run base_code=0
  base_run="$(bash "$HERE/linux_run.sh" run "$sha" --suite "$suites" -- --jobs "$jobs" | tail -n 1)" || base_code=$?
  [[ -f "$base_run/receipt.json" ]] || die "the clean run of $sha wrote no receipt"
  ((base_code == 0)) || echo "mutants.sh: the clean run has unexpected results (exit $base_code);" \
    "its unexpected cases do not count against a mutant" >&2

  local key_args=() path
  for path in "$HERE" "$REPO/tests/oracle" "$REPO/schemas/oracle"; do
    [[ -e "$path" ]] && key_args+=(--key-path "$path")
  done
  local shard_args=(--suites "$suites" --base-run "$base_run" --workers "$workers" --jobs "$jobs"
    --cargo-jobs "$cargo_jobs" "${filters[@]}" "${fresh[@]}")
  [[ -z "$limit" ]] || shard_args+=(--limit "$limit")
  local tmp
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/oracle-mutants.XXXXXX")"
  trap 'rm -rf "$tmp"' EXIT
  local pids=() index extras=() arg
  for arg in "${shard_args[@]}"; do
    extras+=(--extra "arg=$arg")
  done
  for index in "${indexes[@]}"; do
    python3 -I "$HERE/runnerd.py" submit --sha "$sha" --suite "mutants:$suites" --target-cache \
      --target-tag "mutants-$index-of-$shards" --timeout "$JOB_TIMEOUT" --hold "$base_run" \
      "${key_args[@]}" --extra "shard=$index/$shards" "${extras[@]}" "${fresh[@]}" \
      -- bash "$SELF" _shard --sha "$sha" --shard "$index/$shards" "${shard_args[@]}" \
      > "$tmp/shard-$index" &
    pids+=($!)
  done
  local pid
  for pid in "${pids[@]}"; do
    wait "$pid" || true
  done
  local dirs=() dir
  for index in "${indexes[@]}"; do
    dir="$(tail -n 1 "$tmp/shard-$index" 2>/dev/null || true)"
    if [[ -f "$dir/shard.json" ]]; then
      dirs+=("$dir")
    else
      echo "mutants.sh: shard $index/$shards wrote no shard.json (runnerd status shows its log)" >&2
    fi
  done
  ((${#dirs[@]})) || die "no shard finished"
  local report_args=(--out "$out")
  ((json)) && report_args+=(--json)
  local code=0
  mutants_py report "${dirs[@]}" "${report_args[@]}" || code=$?
  ((${#dirs[@]} == ${#indexes[@]})) || code=1
  return "$code"
}

cmd_report() {
  local out="" dirs=() json=()
  while (($#)); do
    case "$1" in
      --out) out="${2:?--out needs a dir}"; shift 2 ;;
      --json) json=(--json); shift ;;
      -*) die "report: unknown option $1" ;;
      *) dirs+=("$1"); shift ;;
    esac
  done
  ((${#dirs[@]})) || die "report needs SHARD_DIR..."
  mutants_py report "${dirs[@]}" --out "${out:-/tmp/oracle/mutants/report-$$}" "${json[@]}"
}

job_shard() {
  [[ -n "${RUNNERD_JOB:-}" && -n "${RUNNERD_OUT:-}" ]] || die "_shard runs only inside a runnerd job (use run)"
  unset CMUX_GHOSTTY_SRC
  mutants_py shard-job "$@"
}

main() {
  local command="${1:-}"
  (($#)) && shift
  # Builds and suite runs never start on a Mac (CLAUDE.md: no local cargo builds).
  [[ "$(uname -s)" == Linux ]] || die "mutants run on the Linux VM only; nothing was queued"
  case "$command" in
    run) cmd_run "$@" ;;
    report) cmd_report "$@" ;;
    list) mutants_py list --root "$REPO/cmux-tui" "$@" ;;
    _shard) job_shard "$@" ;;
    *) sed -n '2,/^set -euo/p' "$SELF" | sed '$d; s/^# \{0,1\}//' >&2; exit 2 ;;
  esac
}

main "$@"

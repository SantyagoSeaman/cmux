# Running the correctness oracle

The oracle runs a corpus of cases on two sides and reports the first value
where the sides differ. Use it to show that a refactor does not change
behavior. The design is in
[p15-oracle.md](../../../plans/cmux-next/chrome-parity/p15-oracle.md)
(sections 3D-3I). This page tells you which commands to run.

All commands use `python3 -I scripts/cmux-next/oracle/oracle.py` (written
`oracle.py` below) from the root of a trusted checkout. Each command prints its
options with `--help`.

## Where it runs

| Product | Host | How |
| --- | --- | --- |
| `daemon`, `cli`, `mcp`, `tui` (cmux-tui) | the Linux VM | through the runnerd queue: `oracle.py diff --base/--head` or `linux_run.sh` |
| `app` | a fleet Mac | `oracle.py diff --product app ...` or `scripts/cmux-next/oracle/fleet.py` |

- Do not run the oracle on the person's Mac. App runs build and launch the app
  on the fleet only.
- On the VM, `oracle.py run` for a cmux-tui product starts only inside a
  runnerd job. You can use `--dry-run` anywhere. For other runs, use
  `scripts/cmux-next/oracle/linux_run.sh run REF [--suite S] [--arm A] [-- RUN_OPTIONS]`.
  The last line of its output is the run directory.
- The outputs are outside the checkout: run directories go in `/tmp/oracle/runs/<run id>/`,
  and fleet runs go in `/tmp/oracle/fleet/<run id>/`.
- The fleet controller refuses `scripts/measure/oracle.sh` until hq lists it, or
  until the commit is on `main` or `feat-cmux-next`. Until then, `fleet.py` exits 3
  and `fleet.json` says `refused`.

## Select what to run

`oracle.py select` (`select.py`, task p15-oracle-009) maps changed paths to suites
through `tests/oracle/suite-map.json`. It is not on the branch yet, and
`oracle.py select` exits 2 with that message. Until it is, select the suites yourself:

| Option | Effect |
| --- | --- |
| `--suite a,b` | the suites (directories of `tests/oracle/corpus/`), or `all` (the default) |
| `--filter 'suite/case*'` | case id globs. A filtered run has a retry limit of 0. |
| `--shard i/N` | shard `i` of `N`, 0-based. Each case is in exactly one shard. |
| `--repeat N` | run the selection N times. A case that gives different hashes is Flaky. |
| `--shuffle`, `--seed N` | a shuffled order. The receipt records the seed so you can run the same order again. |
| `--dry-run` | plan the selection and write the receipt. Nothing runs. |

To see the plan of a selection before you queue it, do a dry run per shard and merge it:

```bash
for i in 0 1 2 3; do
  python3 -I scripts/cmux-next/oracle/oracle.py run --product daemon --suite daemon-ops \
    --shard $i/4 --dry-run --out /tmp/oracle/runs/plan-$i
done
python3 -I scripts/cmux-next/oracle/oracle.py merge --out /tmp/oracle/runs/plan /tmp/oracle/runs/plan-{0,1,2,3}
```

`merge` refuses a shard set with a gap or an overlap.

Each suite is golden or differential (`SUITE_CLASSES` in `baselines.py`). A
differential suite (`daemon-ops`, `actions`, `entrypoints`, `mcp`, ...) compares
base and head only. A golden suite (`journeys`, `tours`, `e2e`, `ax`, `upgrades`,
...) also has reviewed baselines in `tests/oracle/baselines/`.

## Diff base and head

This is the default check for a refactor that has no runtime fallback.

```bash
# cmux-tui products, on the Linux VM: both SHAs are built and run in the runnerd queue
python3 -I scripts/cmux-next/oracle/oracle.py diff --base BASE_SHA --head HEAD_SHA --suite daemon-ops \
  --out /tmp/oracle/runs/diff-my-change

# the app, on the fleet: both sides of each shard run on one worker
python3 -I scripts/cmux-next/oracle/oracle.py diff --product app --base BASE_SHA --head HEAD_SHA --suite journeys
python3 -I scripts/cmux-next/oracle/fleet.py diff --base BASE_SHA --head HEAD_SHA --shards 4 --suite journeys
```

- Give pushed commits. The base is the merge base or the parent of the change.
- `oracle.py diff --product app` sends its arguments to `fleet.py diff`. Use
  `fleet.py diff` directly for `--shards`, `--config` or `--out`. A second
  `fleet.py diff --out SAME_DIR` attaches to the same steps. It does not submit them again.
- Outside the VM, give prebuilt binaries: `--base-binary BIN --head-binary BIN`.
- To compare runs that you already have, use `--base-run DIR --head-run DIR`.
  Give each option one time per shard. Add `--partial` when the shard set is not complete.
- A diff retries 3 times by default: first the full shard with the same config,
  then the case alone. A case that passes on a retry is Flaky. Flaky cases are
  never compared.
- The two sides must come from the same host and OS. If not, the diff is refused with `host-mismatch`.

Exit codes of `oracle.py diff`:

| Exit | Meaning |
| --- | --- |
| 0 | no unexplained divergence |
| 1 | divergences (or, with `--order-parity`, a LeakedState on one side only) |
| 2 | bad arguments or bad input |
| 3 | refused: `host-mismatch`, a shard set with a gap or a mismatch, nothing compared, `arm-not-exercised` |
| 4 | an internal error in diff.py. This is not a divergence. |

## Read divergence.json

The diff writes two files in its `--out` directory (for `fleet.py`, in `OUT/compare/`):

- `comparison.json` (`cmux-oracle-comparison-v2`) gives the verdict:
  - `ok`: true only when nothing was refused and there is no divergence.
  - `refused`: the reason when the diff did not compare the sides.
  - `compared`: the number of cases compared.
  - `divergent` and `explained`: the case ids.
  - `flaky_excluded`: the Flaky cases.
  - `not_comparable`: golden cases that have no baseline.
  - `partial`: true for a diff of only some shards.
  - `leaked_state`: the LeakedState cases of each side.
- `divergence.json` (`oracle-divergence-v1`, written by `diff.py`) gives the details:

| Key | Contents |
| --- | --- |
| `verdict` | `pass`, `divergent`, `host-mismatch`, `bad-input` or `internal-error` |
| `a`, `b` | the base (or old arm) and the head (or new arm): `path`, `runId`, `sha`, `host` |
| `summary` | case counts: `equal`, `explained`, `divergent`, `flaky`, `skipped` |
| `divergences[]` | one finding per divergent case, in the ui-workflow-corpus review-finding shape |
| `cases[]` | each case: `status`, `result.a`/`result.b`, `first`, `explained[]` (`pointer`, `reason`, `count`, `samples`) |

A finding in `divergences[]` holds only the **first** divergence of its case:

- `journey`: the case id.
- `step` and `checkpoint` (`before`, `turn` or `settled`): where the divergence is.
- `pointer`: the first JSON pointer in sorted-key order where the snapshots differ.
- `a` and `b`: the values at that pointer.
- `count`: how many other checkpoints and pointers differ after it. Later differences
  usually follow from the first one, so fix the first one, then run the diff again.
- `evidence`: `checkpoints.jsonl#LINE` of each side.
- `replay`: the `diff.py` command that compares this case again.
- `confidence`: always `unconfirmed`. See [Confirm](#confirm-a-divergence).

Some pointers tell you more:

- `/step` means that the two sides ran different step scripts.
- `/result` means that the results differ (for example, `Pass` against `Crash`)
  and the snapshots do not.
- `kind: missing-in-a` or `missing-in-b` means that one side has no value there.

## Confirm a divergence

A divergence goes to an owner only after it is confirmed (rule 10 of the spec).

`oracle.py confirm` (`confirm.py`, task p15-oracle-184) is not on the branch yet.
It will run the divergent shards on the head again with the same config and
seed, then run the cases alone on the base. Only "the head fails two times and
the base passes" goes to an owner.

Until it is on the branch, do the same steps with `diff`:

```bash
python3 -I scripts/cmux-next/oracle/oracle.py diff --base BASE_SHA --head HEAD_SHA \
  --filter 'daemon-ops/the-case' --seed SEED_FROM_THE_RECEIPT --repeat 3 --fresh
```

- `--fresh` makes the runnerd queue run both sides again. Without it, the queue uses the receipt it has.
- If the case is still in `divergent`, the divergence is confirmed. Fix it, or
  explain it (see [Register a switch](#register-a-refactor-switch) and the expectations below).
- If the case moves to `flaky_excluded`, it is a flake. Add
  `--flaky-out /tmp/oracle/flaky.jsonl` to send it to the MQ-flakes queue. Do not report it as a divergence.
- If the base also fails alone, the base is red. That is not the change's divergence.

`oracle.py route` (task p15-oracle-118) is not on the branch yet. Until it is,
the owner is the most specific row of
[OWNERS.md](../../../plans/cmux-next/OWNERS.md) for the files of the first divergence.

## Register a refactor switch

Use a switch only when the refactor must have a runtime fallback (an old and a
new arm in one build). Otherwise, use `diff --base --head`.

1. Add `plans/cmux-next/refactor-switches/<name>.json`. The fields are in
   [its README](../../../plans/cmux-next/refactor-switches/README.md) and the schema
   `schemas/oracle/refactor-switches.schema.json`: `name`, `owner`, `plan`,
   `read`, `app_key` or `daemon_key`, `default`, `planned`, `created`,
   `expires` (not more than 14 days after `created`), `suites` and
   `expected_differences`.
2. Check it with `oracle.py switches --check`. The check refuses an app key that
   `RefactorSwitches.swift` (task p15-oracle-180) does not declare, and a daemon
   key that `switches.rs` (task p15-oracle-020) does not parse. Until those catalogs
   are on the branch, the check refuses every key and names the missing catalog.
3. Land the new path with `default` set to `old`. Then diff the arms of one build on both platforms:

   ```bash
   python3 -I scripts/cmux-next/oracle/oracle.py diff --switch NAME --ref SHA --suite SUITES_OF_THE_SWITCH_FILE
   python3 -I scripts/cmux-next/oracle/oracle.py diff --product app --switch NAME --ref SHA --suite SUITES_OF_THE_SWITCH_FILE
   ```

   Give `--suite` the switch file's `suites`. A side with 0 hits on its arm is
   refused with `arm-not-exercised` (exit 3): no guarded site ran, so the run
   proves nothing. `oracle.py switches --hits FILE --switch NAME --arm old|new`
   reads one `debug.oracle.switch_hits` reply.
4. Fix each divergence, or add it to `expected_differences` with a `pointer`
   and a `reason`. The diff labels an expected difference `explained` and does not count it.
5. Set `default` to the planned arm, then run every switch at its planned arm against all defaults:
   `oracle.py diff --arm launch-config --ref SHA --suite all`.
6. Remove the old path and the switch file before `expires`.
   `oracle.py switches --expired` prints one deletion line for each expired switch.

## Rebaseline a golden suite

Golden suites compare the head with reviewed baselines:

```bash
python3 -I scripts/cmux-next/oracle/oracle.py diff --head-run RUN_DIR --baselines
```

When the head is correct and a baseline is old, write the head's values as the new baselines:

```bash
python3 -I scripts/cmux-next/oracle/oracle.py rebaseline --run RUN_DIR [--only-changed] [--platform [DIR]] [--no-optimize]
```

- `RUN_DIR` is one shard or a `merge` of shards. A dry run has nothing to rebaseline.
- Only golden cases are written. Differential cases are left out, because their base run is the oracle.
- A case is written only when each of its checkpoints is certain: the result is
  Pass or Failure, the case is not Flaky, and each attempt gives the same hashes.
  Timeout, Crash, uncertain checkpoints and not-comparable facets refuse the whole case.
- `--platform` writes `platform/<p>/` baselines (bare: the run's platform, or a
  dir such as `mac-app`). Without it, the generic baseline (or the run's arm baseline) is written. If a more
  specific baseline is on the run's fallback path, the case is refused.
- The touched cases are optimized after the write. `--no-optimize` stops this.
- `--root DIR` writes into another baseline tree (default `tests/oracle/baselines`).
- Review the changed `.expected.jsonl` files in the commit, as you review code.

To see which baseline a configuration reads, use
`python3 -I scripts/cmux-next/oracle/baselines.py lookup SUITE CASE --platform P --product X [--arm A]`.

## Known wrong values and the corpus

- A value that you know is wrong gets a pointer-scoped line in
  `tests/oracle/Expectations.d/<suite>.txt`, with a task id or an issue. Do not
  add a whole-case `Failure`. The syntax is in [tests/oracle/README.md](../../../tests/oracle/README.md).
- `oracle.py check` lints the corpus and the expectation files.
  `oracle.py gen --suite S` runs the suite generators.
- `oracle.py selftest` runs the self tests of the oracle tooling (stdlib, no network).

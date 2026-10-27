# Refactor switches

One `<name>.json` per refactor switch: an old and a new arm of one refactor in one build, so the
oracle can diff the arms and the new path can ship with a kill switch
([p15-oracle.md](../chrome-parity/p15-oracle.md), sections 3F.2 and 3I). Use a switch only when a
refactor wants a runtime fallback; the default gate is `oracle.py diff --base --head`, which needs none.

Schema: [`schemas/oracle/refactor-switches.schema.json`](../../../schemas/oracle/refactor-switches.schema.json).
Check: `python3 -I scripts/cmux-next/oracle/switches.py --check` (`oracle.py switches --check`).

| Field | Meaning |
| --- | --- |
| `name` | The file stem; the name a case lists in `switches` and `--switch` takes. |
| `owner` | Who deletes the old path before `expires`. |
| `plan` | The plan that owns the refactor (a repo path, optional `#anchor`). |
| `read` | `launch`: read once at the owner start and passed as a value. `live`: sites ask the injected frozen value at each use. Never a per-call closure. |
| `app_key`, `daemon_key` or `config_key` | Exactly one. App: a `RefactorSwitchKey` raw value (`--cmux-switches=KEY=ARM`, `CMUX_NEXT_SWITCHES`). Daemon: a `SWITCH_NAMES` entry of `switches.rs` (`CMUX_TUI_SWITCHES=KEY=ARM`). Config: a settings row key the driver writes into the launch config (a virtual suite: the same cases with one setting changed). |
| `config_arms`, `readback` | Config arms only. `config_arms`: the key's value on `old` and `new`. `readback`: the snapshot pointer where the product reports the applied value (its schema `x-oracle-source` `from` is the key); each case's `switch_readback` comes from it. |
| `default` | The arm when nothing sets the switch. |
| `planned` | The arm the refactor lands at; `--arm launch-config` runs every switch at it. |
| `created`, `expires` | Days (UTC). `expires` is at most `created` + 14 days. A config arm has no `expires`: it is a standing run mode, not a refactor, and never enters the `--expired` queue. |
| `suites` | The minimum suites a both-arm run covers. |
| `expected_differences` | The arms' intended differences (diff.py `--expected`); `[]` when the arms must be equal. |
| `old_paths` | Optional: the old arm's files, deleted with the switch. |

`switches.py --check` refuses a file that does not match the schema (a missing `planned`, `owner` or
`plan`, an unknown field), an expiry more than 14 days after `created`, an app key that
`RefactorSwitchKey` does not declare, a daemon key that `switches.rs` does not parse, a config key that
is no settings row (or whose `readback` does not read that key back), and a switch that a
Swift site reads through a closure literal (`isEnabled: { switches.isNew(.key) }`), which is how one
process ran both arms of `browserResize.layerHosting`.

Lifecycle: register the file, land the new path at default `old`, run `oracle.py run --switch NAME
--arm old|new` and `diff` on both platforms, explain or fix each divergence, flip `default`, run
`--arm launch-config`, then delete the old path and this file before `expires`.
`switches.py --expired` prints one deletion line per switch whose day has come.

Each guarded app site counts the arm it ran: it holds a `RefactorSwitchHit` from
`AppServices.switchHits.hit(_:site:)` (`CmuxNextCompat/SwitchHits.swift`) next to its frozen arm and
calls `ran(new:)` on the branch it took. `debug.oracle.switch_hits` (DEBUG and bench builds) reports the counts per switch and site, and
`switches.py --hits FILE --switch NAME --arm ARM` calls a run with 0 hits on the requested arm
`arm-not-exercised` and one with hits on both arms `arm-mixed` (task p15-oracle-027). The reply is
`SwitchHits.Report` keyed by `app_key`; both sides test the golden
`schemas/oracle/fixtures/switch-hits-reply.json`. Only app sites count, so `--hits` refuses a daemon
switch. The first counted site (`CEFLayerHosting`, `.layerHosting`) is wired once p15-oracle-180
freezes that arm.

`motion-on.json` is a config arm (task p15-oracle-233): every case runs with `ui.animations.enabled=false`
(the `old` arm), and the `new` arm reruns the actions and journeys suites with animation on; settled
snapshots must be equal. `scripts/cmux-next/oracle/action_fixtures/__init__.py` (`MotionArm`) turns the arm
into the launch config, reads it back from `/facets/settings/motion/enabled` and counts its hits per case
(`debug.motion` spans). A config arm's `planned` is its `default`: `oracle.py diff --arm launch-config` leaves it
out (and with it its `expected_differences`); `oracle.py diff --switch motion-on` runs both arms.

The first switches (F6 holder, F1 layer hosting, F2 CEF helper, F7 intents, F8 discard) are task
p15-oracle-232; they need the catalogs of p15-oracle-020 (`switches.rs`) and p15-oracle-180
(`RefactorSwitches.swift`).

"""Shared setup of the oracle's action cases and the every-action sweep (p15-oracle-046, phase 1: no move).

Spec: plans/cmux-next/chrome-parity/p15-oracle.md, section 3 and task 046. Stdlib only, Python 3.9+, safe under
`python3 -I` (the caller puts scripts/cmux-next/oracle on sys.path).

The sweep's setup and teardown stay where they are (scripts/cmux-next/every-action-sweep.py: Sweep.build, plan,
ready, restore, close_new, App, TagTeardown). `load_sweep` imports that script as a module, so the oracle drives
the same code; `every-action-sweep.py --plan-only` shows a later move changed no action's plan.

Per-family fixtures are the sibling modules action_fixtures/<family>.py (`*_test.py` are their tests). Each one
defines:

  FAMILY      the family slug (case-v1 `fixtures.setup`).
  CONTEXTS    {registry context key: factory}. The sweep calls each factory once per session run (and again after
              a relaunch) and keeps the ContextFixture it returns. `enter(driver)` runs where show() makes the base
              pane show a tab kind, before action.describe: it returns None, or why the context did not hold (the
              row's `gap` skip). It may run again before `leave` (the key-not-routed retry's second ready()) and
              then keeps what the first call made. `leave(driver)` runs in the restore before close_new whenever
              `enter` ran, also when the sample stopped after it; it returns None or a drift note. What `enter`
              made is an extra close_new closes like PREPARE's.
  PREPARE     {action id: [(action, args)]}, merged into the sweep's PREPARE (an id both name is refused).
  ARGUMENTS   {(action id, argument name): factory}: values for arguments the sweep's ARGUMENT_FIXTURES does not
              fill, required or not. The sweep calls each distinct factory once per session run (and again after a
              relaunch), so keys that share a factory share one ArgumentFixture. `value(driver, base)` runs in the
              row's plan, before ready(): it returns (value, None), or (None, why) for the row's `gap` skip.
  UNDO        {action id: (action, args)}: the inverse the restore runs after each sample of an action whose effect
              no restore sees (a default it set), merged into the sweep's UNDO (an id both name is refused).
  CLOSES      the gap ids this fixture closes. load_families refuses a CLOSES that is not exactly the `closed` ids of
              tests/oracle/gap-closures/<family>.json (the record p15-oracle-087 applies to every-action-gaps.json),
              and that file without a CLOSES.

`driver` is the sweep's narrow seam (SweepDriver): `run(action, args)` is action.run with focus (the PREPARE path),
`rpc(method, params)` one socket call, `wait(predicate, seconds)` its bounded poll. Replies are the raw socket
reply dicts.

Motion arm (p15-oracle-233)
---------------------------
Every case runs with `ui.animations.enabled=false` (the `old` arm of the `motion-on` virtual suite,
plans/cmux-next/refactor-switches/motion-on.json). CLAUDE.md requires that code works with no
animation: a change applies in one frame and completions still run. The `new` arm runs the same
cases with animation on; its settled snapshots must equal the motion-off ones, and the registry's
expected_differences list the only values allowed to differ.

The arm is a launch config value, not a RefactorSwitches key, so the driver writes it into the
bench config (`launch_config`) and removes it from `--cmux-switches` (`split_switches`). The arm is
read back from the settings facet (`readback` in the registry, /facets/settings/motion/enabled):
the applied settings, not the file the driver wrote. With Reduce Motion on, motion is not on, so
that readback is no arm. `case_report` gives the driver protocol's `switch_readback` and
`switch_hits` for one case. A hit is evidence that the arm ran: on the new arm, each animation span
`debug.motion` recorded (MotionTrace: first frame to rest); on the old arm, a case that read back
motion off and recorded no span. A span on the old arm counts as a new-arm hit: something animated
with the master switch off, and `oracle.py diff --switch` refuses that run as arm-not-applied.

Chrome reference (chromium/src): chrome/test/interaction/interactive_browser_test.h and
ui/base/interaction/interactive_test.h keep one setup and verb set that every interactive UI test composes, instead
of each suite owning its fixture; web_tests/VirtualTestSuites reruns one test list with extra setup, as the
families add contexts to one sweep, and the motion-on arm is that rerun with animation on, compared with the
motion-off expectations. ui/compositor/scoped_animation_duration_scale_mode.h: browser and views tests run with
ZERO_DURATION, so an animation completes in the frame it starts and the test reads the end state (the old arm).
"""

from __future__ import annotations

import copy
import importlib.util
import json
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path
from types import ModuleType
from typing import Any, Callable, Dict, Iterable, List, Mapping, Optional, Protocol, Sequence, Tuple

HERE = Path(__file__).resolve().parent
SCRIPTS = HERE.parents[1]  # scripts/cmux-next
SWEEP = SCRIPTS / "every-action-sweep.py"
SWEEP_MODULE = "every_action_sweep"
GAP_CLOSURES = HERE.parents[3] / "tests" / "oracle" / "gap-closures"  # <family>.json per gap fixture family
FAMILY_SLUG = re.compile(r"^[a-z][A-Za-z0-9_]*$")


class FixtureError(ValueError):
    """A family module that does not load or does not hold the interface, or two families (or a family and the
    sweep) that claim the same context, PREPARE, ARGUMENTS or UNDO entry."""


class Driver(Protocol):
    def run(self, action: str, args: Optional[dict] = None, target: Optional[str] = None) -> Any: ...

    def rpc(self, method: str, params: Optional[dict] = None) -> Any: ...

    def wait(self, predicate: Callable[[], Any], seconds: float) -> Any: ...


class ContextFixture(Protocol):
    def enter(self, driver: Driver) -> Optional[str]: ...

    def leave(self, driver: Driver) -> Optional[str]: ...


class ArgumentFixture(Protocol):
    def value(self, driver: Driver, base: Mapping[str, Any]) -> Tuple[Any, Optional[str]]: ...


Step = Tuple[str, Dict[str, Any]]
ArgumentKey = Tuple[str, str]  # (action id, argument name), as the sweep's ARGUMENT_FIXTURES keys


@dataclass(frozen=True)
class Family:
    """One action_fixtures/<family>.py, checked."""

    name: str
    path: Path
    contexts: Mapping[str, Callable[[], ContextFixture]]
    prepare: Mapping[str, Tuple[Step, ...]]
    closes: Tuple[str, ...]
    module: ModuleType = field(compare=False, repr=False)  # the loaded module, for what a family adds beyond these
    arguments: Mapping[ArgumentKey, Callable[[], ArgumentFixture]] = field(default_factory=dict)
    undo: Mapping[str, Step] = field(default_factory=dict)


@dataclass(frozen=True)
class Families:
    """Every family of a directory, merged: no context key, PREPARE or UNDO id or ARGUMENTS key is claimed twice."""

    families: Tuple[Family, ...] = ()
    owners: Mapping[str, str] = field(default_factory=dict)  # context key -> family name

    @property
    def contexts(self) -> frozenset:
        return frozenset(self.owners)

    def new_contexts(self) -> Dict[str, ContextFixture]:
        """One fresh ContextFixture per context key: a session run's (or a relaunch's) own state."""
        return {key: family.contexts[key]() for family in self.families for key in family.contexts}

    def merged_prepare(self, base: Mapping[str, Sequence[Step]]) -> Dict[str, List[Step]]:
        """`base` (the sweep's PREPARE) with every family's steps added. An id `base` already prepares is refused:
        two setups for one action would run in an order nobody chose."""
        merged = {ident: list(steps) for ident, steps in base.items()}
        for family in self.families:
            for ident, steps in family.prepare.items():
                if ident in merged:
                    raise FixtureError(f"{family.path.name}: PREPARE {ident!r} is already prepared by the sweep "
                                       f"or another family")
                merged[ident] = [(step, dict(args)) for step, args in steps]
        return merged

    def new_arguments(self, driver: Driver, gap: Callable[[str], Any]) -> Dict[ArgumentKey, Callable[[Any], Any]]:
        """The families' ARGUMENTS as ARGUMENT_FIXTURES entries (`base -> (value, None)` or `(None, gap(why))`), over
        one fresh ArgumentFixture per distinct factory: a session run's (or a relaunch's) own state."""
        made: Dict[int, ArgumentFixture] = {}
        fixtures: Dict[ArgumentKey, Callable[[Any], Any]] = {}
        for family in self.families:
            for key, factory in family.arguments.items():
                fixture = made.get(id(factory))
                if fixture is None:
                    fixture = made[id(factory)] = factory()
                fixtures[key] = lambda base, fixture=fixture: _answer(fixture.value(driver, base), gap)
        return fixtures

    def argument_keys(self) -> Tuple[ArgumentKey, ...]:
        """Every (action id, argument name) a family fills, for --plan-only's placeholders."""
        return tuple(key for family in self.families for key in family.arguments)

    def check_arguments(self, sweep_arguments: Iterable[ArgumentKey]) -> None:
        """Refuses a family argument the sweep already fills (ARGUMENT_FIXTURES): one argument, one value."""
        taken = sorted(set(sweep_arguments) & set(self.argument_keys()))
        if taken:
            raise FixtureError(f"argument {', '.join('.'.join(k) for k in taken)} is filled by the sweep "
                               f"(ARGUMENT_FIXTURES) and by a family")

    def merged_undo(self, base: Mapping[str, Step]) -> Dict[str, Step]:
        """`base` (the sweep's UNDO) with every family's inverses added; an id `base` already undoes is refused."""
        merged = dict(base)
        for family in self.families:
            for ident, step in family.undo.items():
                if ident in merged:
                    raise FixtureError(f"{family.path.name}: UNDO {ident!r} is already undone by the sweep or another "
                                       f"family")
                merged[ident] = step
        return merged

    def check_contexts(self, sweep_contexts: Iterable[str]) -> None:
        """Refuses a family context the sweep already sets up (CONTEXT_SETUP): one key, one setup."""
        taken = sorted(set(sweep_contexts) & set(self.owners))
        if taken:
            raise FixtureError(f"context {', '.join(taken)} is set up by the sweep (CONTEXT_SETUP) and by family "
                               f"{', '.join(sorted({self.owners[k] for k in taken}))}")


def _answer(answer: Any, gap: Callable[[str], Any]) -> Tuple[Any, Any]:
    """An ArgumentFixture's (value, why) as the sweep's (value, None) or (None, gap(why))."""
    value, why = answer
    if value is None:
        return None, gap(why or "the argument fixture gave no value")
    return value, None


def family_paths(directory: Path = HERE) -> List[Path]:
    """The family modules of `directory`, sorted: every *.py but __init__.py and *_test.py."""
    return sorted(p for p in directory.glob("*.py") if p.name != "__init__.py" and not p.stem.endswith("_test"))


def _steps(where: str, ident: Any, steps: Any, table: str = "PREPARE") -> Tuple[Step, ...]:
    if not isinstance(ident, str) or not ident:
        raise FixtureError(f"{where}: a {table} key is an action id, not {ident!r}")
    if not isinstance(steps, (list, tuple)):
        raise FixtureError(f"{where}: {table} {ident!r} is a list of (action, args)")
    out = []
    for step in steps:
        if not (isinstance(step, (list, tuple)) and len(step) == 2 and isinstance(step[0], str) and step[0]
                and isinstance(step[1], dict)):
            raise FixtureError(f"{where}: {table} {ident!r} step {step!r} is not (action id, args dict)")
        out.append((step[0], dict(step[1])))
    return tuple(out)


def check_family(module: Any, path: Path) -> Family:
    """The Family `module` (loaded from `path`) defines, or FixtureError naming what it lacks."""
    where = path.name
    name = getattr(module, "FAMILY", None)
    if not isinstance(name, str) or not FAMILY_SLUG.match(name):
        raise FixtureError(f"{where}: FAMILY must be a family slug, not {name!r}")
    contexts = getattr(module, "CONTEXTS", {})
    if not isinstance(contexts, dict) or not all(isinstance(k, str) and k and callable(v) for k, v in contexts.items()):
        raise FixtureError(f"{where}: CONTEXTS maps a registry context key to a ContextFixture factory")
    prepare = getattr(module, "PREPARE", {})
    if not isinstance(prepare, dict):
        raise FixtureError(f"{where}: PREPARE maps an action id to its (action, args) steps")
    closes = getattr(module, "CLOSES", ())
    if not isinstance(closes, (list, tuple)) or not all(isinstance(i, str) and i for i in closes):
        raise FixtureError(f"{where}: CLOSES is a list of action ids")
    if len(set(closes)) != len(closes):
        raise FixtureError(f"{where}: CLOSES names an id twice")
    arguments = getattr(module, "ARGUMENTS", {})
    if not isinstance(arguments, dict) or not all(
            isinstance(k, tuple) and len(k) == 2 and all(isinstance(p, str) and p for p in k) and callable(v)
            for k, v in arguments.items()):
        raise FixtureError(f"{where}: ARGUMENTS maps (action id, argument name) to an ArgumentFixture factory")
    undo = getattr(module, "UNDO", {})
    if not isinstance(undo, dict):
        raise FixtureError(f"{where}: UNDO maps an action id to its (action, args) inverse")
    if not (contexts or prepare or arguments or undo):
        raise FixtureError(f"{where}: a family gives CONTEXTS, PREPARE, ARGUMENTS or UNDO")
    return Family(name, path, dict(contexts), {i: _steps(where, i, s) for i, s in prepare.items()}, tuple(closes),
                  module, dict(arguments), {i: _steps(where, i, [s], "UNDO")[0] for i, s in undo.items()})


def load_module(path: Path, name: str) -> ModuleType:
    """`path` imported as module `name`, registered in sys.modules before it runs (importlib's recipe): dataclasses,
    typing.get_type_hints and pickle look a class's module up there."""
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise FixtureError(f"{path}: not a Python module")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    try:
        spec.loader.exec_module(module)
    except BaseException:
        sys.modules.pop(name, None)
        raise
    return module


def check_closures(family: Family, closures: Path) -> None:
    """Refuses a family whose CLOSES is not the `closed` ids of its gap-closures record (`closures`/<family>.json),
    or a record without a CLOSES: the two name one set of ids."""
    record = closures / f"{family.name}.json"
    where = family.path.name
    if not record.exists():
        if family.closes:
            raise FixtureError(f"{where}: CLOSES names ids but {record.name} is not in {closures}")
        return
    try:
        closed = json.loads(record.read_text())["closed"]
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise FixtureError(f"{record}: not a gap-closures record with `closed`: {error!r}") from error
    if not isinstance(closed, dict):
        raise FixtureError(f"{record}: `closed` maps an action id to why it is closed")
    only_closes, only_record = sorted(set(family.closes) - set(closed)), sorted(set(closed) - set(family.closes))
    if only_closes or only_record:
        raise FixtureError(f"{where}: CLOSES and {record.name} differ: only in CLOSES {only_closes}, "
                           f"only in {record.name} {only_record}")


def load_families(directory: Path = HERE, closures: Path = GAP_CLOSURES) -> Families:
    """Every family of `directory`, checked (CLOSES against `closures`) and merged. A module that fails to import is a
    FixtureError: a fixture that silently drops out would turn its closed gaps back into gaps with no word why."""
    families: List[Family] = []
    owners: Dict[str, str] = {}
    names: Dict[str, Path] = {}
    for path in family_paths(directory):
        try:
            module = load_module(path, f"action_fixtures.{path.stem}")
        except FixtureError:
            raise
        except Exception as error:  # the family's own import error, named with its file
            raise FixtureError(f"{path.name}: does not import: {error!r}") from error
        family = check_family(module, path)
        if family.name in names:
            raise FixtureError(f"{path.name}: FAMILY {family.name!r} is also {names[family.name].name}'s")
        names[family.name] = path
        for key in family.contexts:
            if key in owners:
                raise FixtureError(f"{path.name}: context {key!r} is also family {owners[key]!r}'s")
            owners[key] = family.name
        families.append(family)
    prepared: Dict[str, str] = {}
    filled: Dict[ArgumentKey, str] = {}
    undone: Dict[str, str] = {}
    for family in families:
        for ident in family.prepare:
            if ident in prepared:
                raise FixtureError(f"{family.path.name}: PREPARE {ident!r} is also family {prepared[ident]!r}'s")
            prepared[ident] = family.name
        for key in family.arguments:
            if key in filled:
                raise FixtureError(f"{family.path.name}: ARGUMENTS {'.'.join(key)!r} is also family {filled[key]!r}'s")
            filled[key] = family.name
        for ident in family.undo:
            if ident in undone:
                raise FixtureError(f"{family.path.name}: UNDO {ident!r} is also family {undone[ident]!r}'s")
            undone[ident] = family.name
        check_closures(family, closures)
    return Families(tuple(families), owners)


def load_sweep(argv: Sequence[str]) -> ModuleType:
    """every-action-sweep.py as a module, configured by `argv` (its command line without the program name, e.g.
    ["--app", APP, "--tag", TAG, "--out", DIR]): the oracle's way to its App, Sweep, plan(), restore() and
    TagTeardown where they are. The script reads its options at import (phase 1 moves nothing), so `argv` stands in
    for sys.argv during the import only. One configuration per process: a second call with the same `argv` returns
    the first module, another `argv` is a FixtureError (the module's options are fixed at import)."""
    argv = [str(a) for a in argv]
    loaded = sys.modules.get(SWEEP_MODULE)
    if loaded is not None:
        if getattr(loaded, "LOADED_ARGV", None) != argv:
            raise FixtureError(f"every-action-sweep.py is already loaded with {getattr(loaded, 'LOADED_ARGV', None)}, "
                               f"not {argv}")
        return loaded
    saved = sys.argv
    sys.argv = [str(SWEEP), *argv]
    try:
        module = load_module(SWEEP, SWEEP_MODULE)
    finally:
        sys.argv = saved
    module.LOADED_ARGV = argv
    return module


# ---------------------------------------------------------------------------------------
# Motion arm (p15-oracle-233)

MOTION_SWITCH = "motion-on"
MOTION_REGISTRY = HERE.parents[3] / "plans" / "cmux-next" / "refactor-switches" / f"{MOTION_SWITCH}.json"
ARMS = ("old", "new")
# Reduce Motion replaces movement (spec 3B; Motion.basePolicy), so a motion-on arm needs it off.
REDUCE_MOTION_POINTER = "/facets/settings/motion/reduceMotion"
# The per-case readback when the checkpoints disagree about the arm (a mid-case settings change).
INCONSISTENT = "inconsistent"
# The settings facet was readable but showed no arm: motion on with Reduce Motion on.
NO_ARM = "none"


class MotionArmError(ValueError):
    """A malformed motion-on registry file, an unknown arm, or a case config that sets the arm itself."""


def _unescape(token: str) -> str:
    return token.replace("~1", "/").replace("~0", "~")


_MISSING = object()


def resolve(document: Any, pointer: str) -> Any:
    """The value at a JSON pointer (RFC 6901), or _MISSING."""
    if pointer == "":
        return document
    node = document
    for token in pointer[1:].split("/"):
        token = _unescape(token)
        if isinstance(node, dict) and token in node:
            node = node[token]
        elif isinstance(node, list) and token.isdigit() and int(token) < len(node):
            node = node[int(token)]
        else:
            return _MISSING
    return node


@dataclass(frozen=True)
class MotionArm:
    """The motion-on registry entry: which config key sets the arm and where the product reads it back."""

    name: str
    config_path: Tuple[str, ...]
    values: Mapping[str, bool]
    readback: str
    default: str

    @classmethod
    def load(cls, path: Path = MOTION_REGISTRY) -> "MotionArm":
        try:
            entry = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            raise MotionArmError(f"{path}: {error}") from None
        return cls.from_entry(entry, str(path))

    @classmethod
    def from_entry(cls, entry: Any, where: str = MOTION_SWITCH) -> "MotionArm":
        if not isinstance(entry, dict):
            raise MotionArmError(f"{where}: a switch file is a JSON object")
        key, arms, readback, default = (entry.get(k) for k in ("config_key", "config_arms", "readback", "default"))
        if not isinstance(key, str) or not key or any(not part for part in key.split(".")):
            raise MotionArmError(f"{where}: config_key must be a dotted settings path")
        if not (isinstance(arms, dict) and set(arms) == set(ARMS) and all(isinstance(v, bool) for v in arms.values())
                and arms["old"] != arms["new"]):
            raise MotionArmError(f"{where}: config_arms must map old and new to two different booleans")
        if not isinstance(readback, str) or not readback.startswith("/"):
            raise MotionArmError(f"{where}: readback must be a JSON pointer into the snapshot")
        if default not in ARMS:
            raise MotionArmError(f"{where}: default must be old or new")
        return cls(str(entry.get("name") or MOTION_SWITCH), tuple(key.split(".")), dict(arms), readback, default)

    def check_arm(self, arm: str) -> str:
        if arm not in ARMS:
            raise MotionArmError(f"{self.name}: unknown arm {arm!r} (old or new)")
        return arm

    def split_switches(self, switches: Mapping[str, str]) -> Tuple[Dict[str, str], str]:
        """The switches for `--cmux-switches` without this one, and this one's arm (the default when unset)."""
        rest = {name: arm for name, arm in switches.items() if name != self.name}
        return rest, self.check_arm(switches.get(self.name, self.default))

    def launch_config(self, config: Mapping[str, Any], arm: str) -> Dict[str, Any]:
        """`config` (the bench's cmux-next.json) with the arm's value at config_key.

        A config that already sets the key to another value is refused: a case does not pick the
        arm (case-v1 `environment`), the run does.
        """
        value = self.values[self.check_arm(arm)]
        out = copy.deepcopy(dict(config))
        node = out
        for part in self.config_path[:-1]:
            child = node.setdefault(part, {})
            if not isinstance(child, dict):
                raise MotionArmError(f"{self.name}: the config sets {'.'.join(self.config_path)} parent {part!r} "
                                     f"to {child!r}, not an object")
            node = child
        leaf = self.config_path[-1]
        if leaf in node and node[leaf] != value:
            raise MotionArmError(f"{self.name}: the config sets {'.'.join(self.config_path)}={node[leaf]!r}, "
                                 f"but the {arm} arm needs {value!r}")
        node[leaf] = value
        return out

    def read_arm(self, snapshot: Mapping[str, Any]) -> Optional[str]:
        """The arm a snapshot reads back: old, new, NO_ARM, or None when its settings facet has no value."""
        value = resolve(snapshot, self.readback)
        if not isinstance(value, bool):
            return None
        arm = next(a for a in ARMS if self.values[a] is value)
        if arm == "new" and resolve(snapshot, REDUCE_MOTION_POINTER) is True:
            return NO_ARM
        return arm

    def case_readback(self, snapshots: Iterable[Mapping[str, Any]]) -> Optional[str]:
        """One case's readback over its checkpoints: their common arm, INCONSISTENT, or None (none readable)."""
        seen = {arm for arm in (self.read_arm(s) for s in snapshots) if arm is not None}
        if not seen:
            return None
        return seen.pop() if len(seen) == 1 else INCONSISTENT

    def case_report(self, arm: str, snapshots: Sequence[Mapping[str, Any]], motion: Mapping[str, Any]) -> Dict[str, Any]:
        """The driver result's switch_readback and switch_hits of one case on `arm`.

        `motion` is the `debug.motion {"action": "stop"}` reply of the recording `start_calls`
        began (its `spans` list).
        """
        self.check_arm(arm)
        spans = motion.get("spans")
        if not isinstance(spans, list):
            raise MotionArmError(f"{self.name}: debug.motion reply has no spans list: {json.dumps(motion)[:200]}")
        readback = self.case_readback(snapshots)
        hits: Dict[str, int] = {}
        if spans:
            hits["new"] = len(spans)
        elif arm == "old" and readback == "old":
            hits["old"] = 1
        return {
            "switch_readback": {} if readback is None else {self.name: readback},
            "switch_hits": {self.name: hits} if hits else {},
        }

    def start_calls(self, arm: str) -> List[Tuple[str, Dict[str, Any]]]:
        """Socket calls before a case: start span recording; on the new arm, Reduce Motion off in memory."""
        calls: List[Tuple[str, Dict[str, Any]]] = [("debug.motion", {"action": "start"})]
        if self.check_arm(arm) == "new":
            calls.append(("debug.motion", {"action": "reduce", "value": False}))
        return calls

    @staticmethod
    def stop_call() -> Tuple[str, Dict[str, Any]]:
        """The socket call after a case's last checkpoint; its reply is case_report's `motion`."""
        return ("debug.motion", {"action": "stop"})

    def arm_pointers(self) -> List[str]:
        """The snapshot values that read the arm back, for diff.py --arm-pointer (p15-oracle-111)."""
        return [self.readback]

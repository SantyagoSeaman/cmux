#!/usr/bin/env python3
"""The oracle's headless driver: CmuxNextHeadless (p15-oracle-160) driven over its control socket.

Spec: plans/cmux-next/chrome-parity/p15-oracle.md, section 3F.4 (product parity) and task 161. Stdlib plus the
sibling oracle modules and scripts/perf-lab/bench_env.py, Python 3.9+, safe under `python3 -I`. It implements the
driver protocol of oracle.py (`open_session`, `session.run_case`, `session.close`) for `--product headless`; a
session is one host process that runs up to --batch-limit cases (oracle.py DEFAULT_BATCH_LIMIT), and the VM runs
200 of them at once (`--jobs 200`, the VM default).

The host is the app's model without its views: the HeadlessServices composition root over Daemon, Control,
Actions, reducers, Settings and the Oracle facets. It speaks the app's control protocol, so the case runner is
driver_app.py's (CaseRun: action.run, cli, settle, snapshot, waitFor, turn checkpoints) on a different launch,
and a headless run and an app run of one case are comparable checkpoint by checkpoint (`oracle.py parity`).

Host contract (what task 160's executable serves; this driver is its only launcher):
    CmuxNextHeadless --socket CONTROL --daemon-socket DAEMON --cmux-tui CMUX_TUI --ready-fd N
        starts `CMUX_TUI --headless --socket DAEMON` as its daemon, serves the app's newline-JSON control
        protocol on CONTROL, then writes one JSON line
        {"ready": {"socket": CONTROL, "daemonSocket": DAEMON}} to fd N and closes it. SIGTERM ends the host
        and its daemon. Environment: the bench launch's fixed variables (bench_env.fixed_env), HOME and
        CMUX_NEXT_ORACLE_ROOT (normalize.app_environment), CMUX_NEXT_CONFIG_FILE (the app driver's oracle
        config over the bench config defaults), CMUX_NEXT_CHIEF_HOME and CMUX_NEXT_TEST_WINDOW_FRAME (the
        model window's frame, the same frame the app driver gives the app window).
    CmuxNextHeadless --list-actions
        writes {"actions": [...], "methods": [...], "facets": [...]} to stdout: the action ids HeadlessServices
        binds, the control methods it serves (what `cmux app call METHOD` can reach) and the snapshot facets it
        provides (the debug.oracle.snapshot {"list": true} entries with `provided`).
Required methods (REQUIRED_METHODS): action.run, debug.oracle.snapshot, debug.oracle.settle and
debug.oracle.wait_for (CmuxNextControl's OracleControl, OracleSettleControl and OracleWaitCondition): the case
runner settles at setup and after every settled checkpoint, and waits on every waitFor. A host that lists fewer is
refused before selection; one that refuses one of them anyway fails the case as a Crash that names this contract.
The turn recorder (debug.oracle.recorder and turns) is optional: without it a turn checkpoint is taken with every
facet not-comparable.
The ready line comes on an inherited pipe, so the launch waits on the host's own signal and never polls a
socket file (chrome --remote-debugging-pipe, content/browser/devtools/devtools_pipe_handler.cc, hands the
child inherited fds for the same reason). The host runs in its own process group, and close() ends the whole
group, so no daemon or terminal host outlives the session (invariant I6).

Selection. A headless case is one the host can run with no window system:
- every case of the product's capabilities (oracle.py PRODUCTS["headless"]: actions), and
- an action or cli case written for the app window (requires at most actions and gui) whose steps are only
  action.run, cli, settle and snapshot, with no overlay target and no web fixture, where every action the case
  runs is one the host binds (an action.run step, and a cli `action run ID`), every cli `app call METHOD` names a
  method the host serves, no cli step routes itself (--socket, --session, --machine, --app-socket,
  --all-sessions: the driver gives the CLI the session's sockets), and every waitFor reads a facet the host
  provides that is not GUI-only (GUI_FACETS: a wait on a facet the host does not have could only time out).
  `capable_cases(binary)` is that rule; oracle.py asks for it before it
  selects (chromium/src base/test/launcher/unit_test_launcher.cc asks the test binary for its tests with
  --gtest_list_tests the same way). Such a case runs on the VM although it says `platforms: [mac]`: that is where
  the app runs, and the host runs on Linux and macOS.
A GUI-only facet (GUI_FACETS) comes back not comparable from the host; `oracle.py parity` reports it as
missing-facet, never as a divergence.

Terminals run the app launch's shell (bench_env SHELL, zsh): a machine without it is refused, so the terminals
facet compares the products and not the hosts' shells.

`cli` steps run the cmux-tui of the run (`--cmux-tui`) as `cmux --app-socket CONTROL --socket DAEMON`: the app's
scopes on the host's control socket, the daemon's on the session's own daemon (a bare cmux-tui on Linux has no app
bundle to find it by, and would reach the runner's default session).
The host has no keys, no MCP and no restart, so key, mcp, kill and restart steps (and resource.call, the daemon
product's) refuse the case as a Crash before any launch. A turn checkpoint the host cannot record (its Control has
no recorder, or reads none of the step's facets in a perform turn) is taken with every facet not-comparable and
named in the result message: the step indexes stay those of the app run, and `oracle.py parity` reports each such
facet as missing-facet.

Chrome reference (chromium/src): headless/ (headless_shell: the browser without its UI, driven by the same
DevTools protocol as chrome) and third_party/blink/tools/run_wpt_tests.py (one suite, products chrome and
headless_shell, compared through per-product metadata).

Usage:
  driver_headless.py run-case --binary CmuxNextHeadless --cmux-tui CMUX_TUI --ref SHA CASE.json [--repeat 2]
  driver_headless.py list-actions --binary CmuxNextHeadless
  driver_headless.py --selftest       the offline tests (driver_headless_test.py)
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import re
import select
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Dict, FrozenSet, List, Mapping, Optional, Sequence, Tuple

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
BENCH_ENV = REPO / "scripts" / "perf-lab" / "bench_env.py"

if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import driver_app  # noqa: E402 - the case runner and the control protocol (p15-oracle-047)
import webfixture  # noqa: E402 - sibling loopback server (p15-oracle-222)
from driver_app import (  # noqa: E402
    RECORDER,
    REQUEST_S,
    SETTLE,
    SNAPSHOT,
    WAIT_FOR,
    AppGone,
    AppLaunchFailed,
    AppSession,
    Capture,
    CaseError,
    CaseRun,
    LaunchSpec,
    OraclePaths,
    control_request,
    decode_pointer,
    host_meta,
    not_comparable,
    read_daemon_facet,
)

PRODUCT = "headless"
HOST_NAME = "CmuxNextHeadless"
CLI_ARGV0 = "cmux"
# The step ops the host runs: its control surface and the CLI on it.
HEADLESS_OPS = frozenset({"action.run", "cli", "settle", "snapshot"})
# What the other products run, named in the refusal of a step the host cannot.
OTHER_PRODUCTS = {
    "key": "the app product (keys need a window; p15-oracle-047)",
    "tui.key": "the tui product (p15-oracle-234)",
    "mcp": "the app or mcp product (p15-oracle-209)",
    "kill": "the daemon or app product (p15-oracle-023)",
    "restart": "the daemon or app product (p15-oracle-023)",
    "resource.call": "the daemon product (p15-oracle-023)",
    "advance_clock": "the daemon product (p15-oracle-185)",
}
# A case written for the app window runs here when it needs nothing more than these and its steps are HEADLESS_OPS.
WIDENED_REQUIRES = frozenset({"actions", "gui"})
# The control methods the case runner calls on every case (Required methods in the module doc).
REQUIRED_METHODS = frozenset({"action.run", SNAPSHOT, SETTLE, WAIT_FOR})
# Facets of the window system: the host reports them not-comparable, so a wait on one never comes true.
GUI_FACETS = frozenset({"layout", "windows", "overlays", "webviews", "ax"})
# The CLI global flags (cmux-tui src/cli.rs GlobalArgs) a cli step may carry; the routing ones it may not.
CLI_FLAGS = frozenset({"--json", "--jsonl", "--quiet"})
CLI_VALUE_FLAGS = frozenset({"--idempotency-key"})
LAUNCH_S = 120.0
LIST_S = 30.0
QUIT_S = 10.0
CONFIG_NAME = "cmux-next.json"
CONTROL_NAME, DAEMON_NAME = "c.sock", "d.sock"
SHA = re.compile(r"^[0-9a-f]{40}$")


class HostRefused(ValueError):
    """The binary is not a CmuxNextHeadless the driver may launch, it gave no capability list, or this machine
    lacks what the app launch has (its shell)."""


# ---------------------------------------------------------------------------------------------------------
# Selection


@dataclass(frozen=True)
class HostCapabilities:
    """What `CmuxNextHeadless --list-actions` reports: bound action ids, served control methods, provided facets."""

    actions: FrozenSet[str]
    methods: FrozenSet[str]
    facets: FrozenSet[str]

    def digest(self) -> str:
        text = json.dumps({"actions": sorted(self.actions), "methods": sorted(self.methods),
                           "facets": sorted(self.facets)}, sort_keys=True)
        return hashlib.sha256(text.encode()).hexdigest()


def name_list(listing: Mapping[str, Any], key: str, binary: str) -> FrozenSet[str]:
    values = listing.get(key)
    if not isinstance(values, list) or not all(isinstance(v, str) for v in values):
        raise HostRefused(f"{binary} --list-actions: `{key}` is not a list of names")
    return frozenset(values)


def list_actions(binary: str, run: Callable[..., subprocess.CompletedProcess] = subprocess.run) -> HostCapabilities:
    """The host's actions, methods and facets (`CmuxNextHeadless --list-actions`); refuses a host that lacks one
    of REQUIRED_METHODS."""
    try:
        done = run([binary, "--list-actions"], capture_output=True, text=True, timeout=LIST_S,
                   stdin=subprocess.DEVNULL)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise HostRefused(f"{binary} --list-actions: {error}") from None
    if done.returncode != 0:
        raise HostRefused(f"{binary} --list-actions exited {done.returncode}: {done.stderr.strip()[-300:]}")
    try:
        listing = json.loads(done.stdout)
    except ValueError:
        listing = None
    if not isinstance(listing, dict):
        raise HostRefused(f"{binary} --list-actions printed no {{\"actions\", \"methods\", \"facets\"}} object")
    host = HostCapabilities(*(name_list(listing, key, binary) for key in ("actions", "methods", "facets")))
    lacking = sorted(REQUIRED_METHODS - host.methods)
    if lacking:
        raise HostRefused(f"{binary} serves no {', '.join(lacking)}: the case runner calls them on every case "
                          "(driver_headless.py, Required methods)")
    return host


def cli_command(argv: Any) -> Optional[List[str]]:
    """A cli step's argv after its global flags; None when it routes itself or is malformed."""
    if not isinstance(argv, list) or not all(isinstance(a, str) for a in argv):
        return None
    rest = list(argv)
    while rest and rest[0].startswith("-"):
        flag, inline = rest[0].split("=", 1)[0], "=" in rest[0]
        if flag in CLI_FLAGS and not inline:
            rest = rest[1:]
        elif flag in CLI_VALUE_FLAGS:
            rest = rest[1:] if inline else rest[2:]
        else:
            return None
    return rest


def cli_capable(argv: Any, host: HostCapabilities) -> bool:
    """A cli step the host serves: `action run ID` of a bound action, `app call METHOD` of a served method."""
    command = cli_command(argv)
    if not command:
        return False
    if command[:2] == ["action", "run"]:
        return len(command) > 2 and command[2] in host.actions
    if command[:2] == ["app", "call"]:
        return len(command) > 2 and command[2] in host.methods
    return True


def wait_capable(wait: Any, host: HostCapabilities) -> bool:
    """No waitFor, or one on a facet the host provides that is not GUI-only."""
    if not wait:
        return True
    if not isinstance(wait, Mapping) or not isinstance(wait.get("pointer"), str):
        return False
    try:
        parts = decode_pointer(wait["pointer"])
    except ValueError:
        return False
    return bool(parts) and parts[0] not in GUI_FACETS and parts[0] in host.facets


def headless_capable(case: Mapping[str, Any], host: HostCapabilities) -> bool:
    """Whether a case written for the app window runs on the host (see Selection in the module doc)."""
    if not set(case.get("requires") or ()) <= WIDENED_REQUIRES:
        return False
    if (case.get("fixture") or {}).get("web"):
        return False
    steps = case.get("steps") or ()
    if not steps:
        return False
    for step in steps:
        if not isinstance(step, Mapping) or step.get("op") not in HEADLESS_OPS:
            return False
        if "overlay" in (step.get("target") or {}):
            return False
        args = step.get("args") or {}
        if step["op"] == "action.run" and args.get("action") not in host.actions:
            return False
        if step["op"] == "cli" and not cli_capable(args.get("argv"), host):
            return False
        if not wait_capable(step.get("waitFor"), host):
            return False
    return True


def capable_cases(binary: str, run: Callable[..., subprocess.CompletedProcess] = subprocess.run,
                  shell_exists: Callable[[str], bool] = os.path.exists
                  ) -> Tuple[Callable[[Mapping[str, Any]], bool], Dict[str, Any]]:
    """oracle.py's selection hook for --product headless: the rule over the host's capabilities, and what the
    receipt records about them. A machine without the app launch's shell is refused before any selection."""
    check_shell(load_bench_env(), shell_exists)
    host = list_actions(binary, run)
    note = {"source": f"{HOST_NAME} --list-actions", "actions": len(host.actions), "methods": len(host.methods),
            "facets": sorted(host.facets), "digest": host.digest()}
    return (lambda case: headless_capable(case, host)), note


def check_steps(case: Mapping[str, Any]) -> None:
    """Refuses, before any launch, a step the host cannot run."""
    for step in case.get("steps") or ():
        op = step.get("op") if isinstance(step, Mapping) else None
        if op not in HEADLESS_OPS:
            where = OTHER_PRODUCTS.get(str(op), "no product")
            raise CaseError("Crash", f"op {op} does not run on the headless product; it runs on {where}",
                            step.get("id") if isinstance(step, Mapping) else None)


def headless_launch_spec(case: Mapping[str, Any]) -> LaunchSpec:
    check_steps(case)
    return driver_app.launch_spec(case)


# ---------------------------------------------------------------------------------------------------------
# The host, behind one seam (tests pass a fake)


def daemon_path(paths: OraclePaths) -> str:
    """The session's own daemon socket: the host's --daemon-socket and the CLI's --socket."""
    return str(paths.root / DAEMON_NAME)


def load_bench_env() -> Any:
    """scripts/perf-lab/bench_env.py, loaded by path (`python3 -I` puts no script folder on sys.path)."""
    if "bench_env" in sys.modules:
        return sys.modules["bench_env"]
    spec = importlib.util.spec_from_file_location("bench_env", str(BENCH_ENV))
    if spec is None or spec.loader is None:
        raise ImportError(f"no module at {BENCH_ENV}")
    module = importlib.util.module_from_spec(spec)
    sys.modules["bench_env"] = module
    spec.loader.exec_module(module)
    return module


def check_shell(bench: Any, shell_exists: Callable[[str], bool] = os.path.exists) -> str:
    """The app launch's shell (bench_env SHELL); refuses a machine without it: the terminals facet holds the
    shell's rows, and another shell would be a difference of the hosts, not of the products."""
    shell = str(bench.fixed_env().get("SHELL") or "")
    if not shell or not shell_exists(shell):
        raise HostRefused(f"this machine has no {shell or 'SHELL'}, the shell of the app launch (bench_env.py): "
                          "install it so both products' terminals run the same shell")
    return shell


def host_environment(paths: OraclePaths, config_file: Path, window: Tuple[int, int, int, int],
                     bench_env: Any = None) -> Dict[str, str]:
    """The host's environment: the app launch's fixed variables (its shell too) and hermetic prefixes, nothing
    inherited but PATH, USER and TMPDIR (no CMUX variable of the runner's own terminal reaches it)."""
    import normalize

    bench = bench_env or load_bench_env()
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "USER": os.environ.get("USER", ""),
           "TMPDIR": os.environ.get("TMPDIR", "/tmp"), **bench.fixed_env()}
    env.update(normalize.app_environment(normalize.Context(root=paths.roots, home=(str(paths.home),))))
    env.update({"CMUX_NEXT_CONFIG_FILE": str(config_file), "CMUX_NEXT_CHIEF_HOME": str(paths.root / "chief"),
                "CMUX_NEXT_TEST_WINDOW_FRAME": ",".join(str(v) for v in window)})
    return env


def write_config(path: Path, bench_env: Any = None) -> None:
    """The app driver's oracle config over the bench config defaults, as the app launch writes it."""
    bench = bench_env or load_bench_env()
    path.write_text(json.dumps(bench.with_config_defaults(dict(driver_app.ORACLE_CONFIG)), sort_keys=True),
                    encoding="utf-8")


def read_ready(fd: int, deadline: float, alive: Callable[[], bool]) -> Dict[str, Any]:
    """Blocks on the ready pipe until the host's ready line, its close, or `deadline` (time.monotonic)."""
    buf = b""
    while b"\n" not in buf:
        left = deadline - time.monotonic()
        if left <= 0:
            raise AppLaunchFailed("ready-timeout", f"no ready line in {LAUNCH_S:.0f} s")
        readable, _, _ = select.select([fd], [], [], left)
        if not readable:
            continue
        chunk = os.read(fd, 4096)
        if not chunk:
            state = "exited" if not alive() else "closed the ready pipe"
            raise AppLaunchFailed("no-ready", f"the host {state} before its ready line")
        buf += chunk
    try:
        ready = json.loads(buf.split(b"\n", 1)[0])["ready"]
    except (ValueError, KeyError, TypeError):
        raise AppLaunchFailed("bad-ready", f"the ready line is not {{\"ready\": {{...}}}}: {buf[:200]!r}") from None
    if not isinstance(ready, dict) or not isinstance(ready.get("socket"), str):
        raise AppLaunchFailed("bad-ready", f"the ready line names no control socket: {ready!r}")
    return ready


class HeadlessPort:
    """The host as driver_app.CaseRun uses an app (BenchAppPort's interface): launch, request, alive, the CLI and
    its environment, the daemon socket, stop. The host has no restart and no daemon kill (no such capability)."""

    def __init__(self, binary: str, cmux_tui: str, root: OraclePaths, out: Path,
                 environment: Callable[[Tuple[int, int, int, int]], Dict[str, str]],
                 popen: Callable[..., subprocess.Popen] = subprocess.Popen):
        self.binary = binary
        self.cmux_tui = cmux_tui
        self.root = root
        self.out = out
        self.environment = environment
        self.popen = popen
        self.process: Optional[subprocess.Popen] = None
        self.env: Dict[str, str] = {}
        self.control = str(root.root / CONTROL_NAME)
        self.daemon = daemon_path(root)

    def launch(self, spec: LaunchSpec) -> None:
        for path in (self.control, self.daemon):
            Path(path).unlink(missing_ok=True)
        self.env = self.environment(spec.window)
        read_end, write_end = os.pipe()
        argv = [self.binary, "--socket", self.control, "--daemon-socket", self.daemon, "--cmux-tui", self.cmux_tui,
                "--ready-fd", str(write_end)]
        try:
            with (self.out / "host.log").open("ab") as log:
                self.process = self.popen(argv, env=self.env, cwd=str(self.root.home), stdin=subprocess.DEVNULL,
                                          stdout=log, stderr=subprocess.STDOUT, pass_fds=(write_end,),
                                          start_new_session=True)
        except OSError as error:
            os.close(read_end)
            raise AppLaunchFailed("spawn", f"{self.binary}: {error}") from None
        finally:
            os.close(write_end)
        try:
            ready = read_ready(read_end, time.monotonic() + LAUNCH_S, self.alive)
        except AppLaunchFailed:
            self.stop()
            raise
        finally:
            os.close(read_end)
        if ready["socket"] != self.control or ready.get("daemonSocket", self.daemon) != self.daemon:
            self.stop()
            raise AppLaunchFailed("bad-ready", f"the host serves {ready}, not {self.control} and {self.daemon}")

    def request(self, method: str, params: Mapping[str, Any], timeout: float) -> Dict[str, Any]:
        return control_request(self.control, method, params, timeout)

    def alive(self) -> bool:
        return self.process is not None and self.process.poll() is None

    @property
    def socket_path(self) -> str:
        return self.control

    @property
    def cli_path(self) -> str:
        return self.cmux_tui

    def cli_env(self) -> Dict[str, str]:
        """The host's HOME, config file and PATH for the CLI, the session's daemon as CMUX_TUI_SOCKET, and no other
        CMUX variable (a terminal's CMUX_SOCKET_PATH would send the CLI elsewhere)."""
        env = {k: v for k, v in self.env.items() if not k.startswith("CMUX")}
        env["CMUX_NEXT_CONFIG_FILE"] = self.env["CMUX_NEXT_CONFIG_FILE"]
        env["CMUX_TUI_SOCKET"] = self.daemon
        return env

    def daemon_socket(self) -> Optional[str]:
        return self.daemon if self.alive() else None

    def signal_daemon(self, signum: int) -> Optional[int]:
        return None

    def relaunch(self) -> None:
        raise AppLaunchFailed("no-restart", "the headless product has no restart (oracle.py PRODUCTS)")

    def stop(self) -> None:
        """SIGTERM to the host's process group (the host, its daemon and terminal hosts), SIGKILL after QUIT_S."""
        process, self.process = self.process, None
        if process is None:
            return
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=QUIT_S)
        except subprocess.TimeoutExpired:
            pass
        # The group outlives its leader when a daemon or terminal host ignored SIGTERM: none may survive (I6).
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


# ---------------------------------------------------------------------------------------------------------
# One case


class HeadlessCaseRun(CaseRun):
    """driver_app's CaseRun on the host: its own ops only, the CLI as `cmux` on the session's two sockets, product
    `headless` in the meta, a turn checkpoint the host cannot record taken with every facet not-comparable (named in
    the message), and a required method the host refuses reported as a break of its contract."""

    def __init__(self, session: "HeadlessSession", case: Mapping[str, Any], ctx: Mapping[str, Any]):
        super().__init__(session, case, ctx)
        # The steps whose turn checkpoint was taken not comparable, and the running step's unarmed turn (the
        # captures index it goes to and the step's facets).
        self.turnless: List[str] = []
        self.unarmed: Optional[Tuple[int, Tuple[str, ...]]] = None

    def run(self) -> Dict[str, Any]:
        result = super().run()
        if self.turnless:
            steps = ", ".join(self.turnless)
            note = f"turn checkpoint not comparable (the host records no turn of these facets): {steps}"
            result["message"] = "; ".join(filter(None, [result.get("message"), note]))
        return result

    def call(self, method: str, params: Mapping[str, Any], step: str, timeout: Optional[float] = None
             ) -> Dict[str, Any]:
        reply = super().call(method, params, step, timeout)
        if method in REQUIRED_METHODS and (reply.get("error") or {}).get("code") == "method_not_found":
            raise CaseError("Crash", f"the host serves no {method}, which the CmuxNextHeadless contract requires "
                                     "(driver_headless.py, Required methods; p15-oracle-160)", step)
        return reply

    def arm(self, facets: Sequence[str], step: str) -> Optional[Tuple[int, int]]:  # type: ignore[override]
        if not any(f in self.session.armable for f in facets):
            self.unarmed = (len(self.state.captures), tuple(facets))
            return None
        return super().arm(facets, step)

    def run_step(self, index: int, step: Mapping[str, Any]) -> None:
        """The step, then the turn checkpoint the host could not record, put after the step's `before`."""
        self.unarmed = None
        super().run_step(index, step)
        if self.unarmed is not None:
            at, facets = self.unarmed
            values = {name: not_comparable("the headless host records no perform turn") for name in facets}
            self.state.captures.insert(at, Capture(index, step["id"], "turn", self.snapshot(index, "turn", values)))
            self.turnless.append(step["id"])

    def run_cli(self, argv: List[str], stdin: Optional[str], step: str) -> subprocess.CompletedProcess:
        """The run's cmux-tui as `cmux --app-socket CONTROL --socket DAEMON`, bounded like the app driver's CLI."""
        bound = driver_app.CLI_S * self.scale
        feed: Dict[str, Any] = {"input": stdin} if stdin is not None else {"stdin": subprocess.DEVNULL}
        try:
            return self.session.run([CLI_ARGV0, "--app-socket", self.app.socket_path,
                                     "--socket", daemon_path(self.session.paths), *argv],
                                    executable=self.app.cli_path, env=self.app.cli_env(), capture_output=True,
                                    text=True, timeout=bound, **feed)
        except subprocess.TimeoutExpired:
            raise CaseError("Timeout", f"cmux {' '.join(argv)[:200]} gave no answer in {bound:.0f} s", step) from None
        except OSError as error:
            raise CaseError("Crash", f"cmux {' '.join(argv)[:200]} did not start: {error}", step) from None

    def snapshot(self, index: int, checkpoint: str, facets: Dict[str, Any]) -> Dict[str, Any]:
        out = super().snapshot(index, checkpoint, facets)
        out["meta"]["product"] = PRODUCT
        return out


# ---------------------------------------------------------------------------------------------------------
# The session


HostFactory = Callable[[str, str, OraclePaths, Path, Callable[[Tuple[int, int, int, int]], Dict[str, str]]], Any]


def executable(path: Any, what: str) -> str:
    if not path:
        raise HostRefused(f"the headless driver needs {what}")
    real = os.path.realpath(str(path))
    if real.rstrip("/").endswith(".app") or not os.path.isfile(real) or not os.access(real, os.X_OK):
        raise HostRefused(f"{path} is not an executable file ({what})")
    return real


class HeadlessSession(AppSession):
    """One host process for up to --batch-limit cases. It reuses AppSession's launch-agnostic half (ensure_launched,
    request, daemon_facet, canonical, close) over a HeadlessPort; its own setup replaces the app bundle's identity
    checks, which a host executable has none of."""

    case_class = HeadlessCaseRun

    def __init__(self, session_ctx: Mapping[str, Any], *, make_host: Optional[HostFactory] = None,
                 canonicalizer: Optional[Tuple[Any, Dict[str, Any]]] = None,
                 run: Callable[..., subprocess.CompletedProcess] = subprocess.run,
                 read_daemon: Callable[[str, Mapping[str, Any]], Dict[str, Any]] = read_daemon_facet,
                 host: Callable[[], Dict[str, str]] = host_meta, root_base: Path = Path("/tmp/o"),
                 web_fixture: Callable[[], Any] = webfixture.WebFixture, bench_env: Any = None,
                 shell_exists: Callable[[str], bool] = os.path.exists):
        if session_ctx.get("switches") or session_ctx.get("arm") not in (None, "default"):
            raise HostRefused("switch arms reach the host with p15-oracle-049; this driver runs the default arm")
        if Path(str(session_ctx.get("binary") or "")).name != HOST_NAME:
            raise HostRefused(f"--binary {session_ctx.get('binary')}: the headless product runs {HOST_NAME} "
                              "(p15-oracle-160)")
        binary = executable(session_ctx.get("binary"), f"--binary PATH/{HOST_NAME}")
        cmux_tui = executable(session_ctx.get("cmux_tui"), "--cmux-tui PATH (its daemon and the cmux of cli steps)")
        ref = session_ctx.get("ref")
        if not (isinstance(ref, str) and SHA.match(ref)):
            raise HostRefused("the headless product needs --ref FULL_SHA: a host executable records no build sha")
        check_shell(bench_env or load_bench_env(), shell_exists)
        self.sha = ref
        self.build = str(session_ctx.get("build") or "debug")
        self.canonicalize, self.schema_doc = canonicalizer or driver_app.normalize_canonicalizer(
            session_ctx.get("snapshot_schema"))
        self.app_path = binary
        self.tag = None
        self.run = run
        self.read_daemon = read_daemon
        self.web_fixture = web_fixture
        self.host = host()
        root_base.mkdir(parents=True, exist_ok=True)
        self.paths = OraclePaths(Path(tempfile.mkdtemp(prefix="h", dir=str(root_base))))
        self.paths.home.mkdir()
        self.config_file = self.paths.root / CONFIG_NAME
        write_config(self.config_file, bench_env)
        out = Path(session_ctx.get("out_dir") or root_base / "logs") / PRODUCT / self.paths.root.name
        out.mkdir(parents=True, exist_ok=True)
        self.out = out

        def environment(window: Tuple[int, int, int, int]) -> Dict[str, str]:
            return host_environment(self.paths, self.config_file, window, bench_env)

        self.app = (make_host or HeadlessPort)(binary, cmux_tui, self.paths, out, environment)
        self.launched: Optional[LaunchSpec] = None
        self.armable: Tuple[str, ...] = ()

    def probe_armable(self) -> Tuple[str, ...]:
        """AppSession's probe, except that a host whose Control has no recorder arms nothing (no turn checkpoints)."""
        catalog = self.request(SNAPSHOT, {"list": True})
        provided = [e["facet"] for e in catalog if isinstance(e, dict) and e.get("provided")]
        armable = []
        for name in provided:
            reply = self.app.request(RECORDER, {"arm": True, "facets": [name]}, REQUEST_S)
            if (reply.get("error") or {}).get("code") == "method_not_found":
                return ()
            if reply.get("ok") is True:
                armable.append(name)
        self.request(RECORDER, {"arm": False, "clear": True})
        return tuple(armable)

    def run_case(self, case: Dict[str, Any], case_ctx: Dict[str, Any]) -> Dict[str, Any]:
        try:
            spec = headless_launch_spec(case)
            self.ensure_launched(spec)
        except CaseError as error:
            refused = {"status": error.status, "message": error.message, "checkpoints": []}
            if error.step:
                refused["step"] = error.step
            return refused
        except (AppLaunchFailed, AppGone, OSError) as error:
            return {"status": "Crash", "message": f"host launch: {error} (log {self.out / 'host.log'})",
                    "checkpoints": [], "session_dirty": True}
        return self.case_class(self, case, case_ctx).run()



def open_session(session_ctx: Dict[str, Any]) -> Any:
    return HeadlessSession(session_ctx)


# ---------------------------------------------------------------------------------------------------------
# CLI


def cmd_run_case(args: argparse.Namespace) -> int:
    case = json.loads(Path(args.case).read_text(encoding="utf-8"))
    ctx = {"run_id": "run-case", "product": PRODUCT, "build": "debug", "binary": args.binary,
           "cmux_tui": args.cmux_tui, "ref": args.ref, "out_dir": args.out}
    return driver_app.run_repeats(ctx, case, args.repeat, open_session=HeadlessSession,
                                  emit=lambda line: print(line, flush=True))


def cmd_list_actions(args: argparse.Namespace) -> int:
    host = list_actions(args.binary)
    print(json.dumps({"actions": sorted(host.actions), "methods": sorted(host.methods),
                      "facets": sorted(host.facets)}, indent=1))
    return 0


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--selftest", action="store_true", help="run the offline tests (driver_headless_test.py)")
    sub = parser.add_subparsers(dest="command")
    one = sub.add_parser("run-case", help="one case --repeat times, each in a fresh host")
    one.add_argument("case")
    one.add_argument("--binary", required=True, help=f"the {HOST_NAME} executable")
    one.add_argument("--cmux-tui", required=True, help="the cmux-tui of the same SHA (daemon and cli steps)")
    one.add_argument("--ref", required=True, help="the full SHA both were built from")
    one.add_argument("--repeat", type=int, default=2)
    one.add_argument("--out", help="log folder (default: /tmp/o/logs, kept after the run)")
    listing = sub.add_parser("list-actions", help="the actions the host binds")
    listing.add_argument("--binary", required=True)
    args = parser.parse_args(argv)
    if args.selftest:
        import unittest

        import driver_headless_test

        suite = unittest.defaultTestLoader.loadTestsFromModule(driver_headless_test)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    try:
        if args.command == "run-case":
            return cmd_run_case(args)
        if args.command == "list-actions":
            return cmd_list_actions(args)
    except HostRefused as error:
        print(f"driver_headless: {error}", file=sys.stderr)
        return 2
    parser.print_usage(sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())

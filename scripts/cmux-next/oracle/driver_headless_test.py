#!/usr/bin/env python3
"""Offline tests of driver_headless.py and `oracle.py parity` (p15-oracle-161).

A fake host (driver_app_test.FakeApp behind the HeadlessPort interface) runs the case runner; a stub
`CmuxNextHeadless` script exercises the real launch: the ready pipe, the control socket, --list-actions and
the process-group teardown. Product parity runs on run directories written by oracle.perform_run with two
fake products. The VM test of the task (headless at --jobs 200 against an app run of the same SHA, the full
run in under 10 minutes) needs the CmuxNextHeadless executable of p15-oracle-160.

    python3 -I scripts/cmux-next/oracle/driver_headless_test.py
"""

from __future__ import annotations

import io
import json
import os
import select
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from typing import Any, Dict, List

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import driver_app  # noqa: E402
import driver_app_test  # noqa: E402
import driver_headless  # noqa: E402
import oracle  # noqa: E402
from driver_app_test import FakeApp, Recorder, case, identity_canonicalizer, step  # noqa: E402

SHA = "b" * 40
ACTIONS = frozenset({"workspace.new", "workspace.rename", "splitRight"})
METHODS = frozenset({"action.run", "snapshot.get", "debug.oracle.snapshot", "debug.oracle.settle",
                     "debug.oracle.wait_for", "debug.focus"})
FACETS = frozenset({"topology", "daemon", "focus", "settings", "terminals", "tabs"})
HOST = driver_headless.HostCapabilities(ACTIONS, METHODS, FACETS)


def listing(**overrides: Any) -> str:
    return json.dumps({"actions": sorted(ACTIONS), "methods": sorted(METHODS), "facets": sorted(FACETS),
                       **overrides})


def executable(path: Path, body: str = "#!/bin/sh\nexit 0\n") -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body)
    path.chmod(0o755)
    return path


class SelectionTests(unittest.TestCase):
    def test_window_cases_of_host_actions_and_cli_run_headless(self) -> None:
        runs = case(step("new", "action.run", args={"action": "workspace.new"}), step("s", "snapshot"),
                    requires=["actions", "gui"])
        self.assertTrue(driver_headless.headless_capable(runs, HOST))
        tour = case(step("c", "cli", args={"argv": ["tree", "--json"]}), step("s", "settle"), requires=["gui"])
        self.assertTrue(driver_headless.headless_capable(tour, HOST))

    def test_keys_chromium_web_overlays_and_unbound_actions_do_not(self) -> None:
        def capable(*steps, **extra) -> bool:
            return driver_headless.headless_capable(case(*steps, **extra), HOST)

        new = step("new", "action.run", args={"action": "workspace.new"})
        self.assertFalse(capable(new, step("k", "key", args={"keys": ["cmd+n"]}), requires=["actions", "gui", "keys"]))
        self.assertFalse(capable(new, step("k", "key", args={"keys": ["cmd+n"]}), requires=["actions", "gui"]))
        self.assertFalse(capable(new, requires=["actions", "gui", "chromium"]))
        self.assertFalse(capable(step("x", "action.run", args={"action": "browser.reload"}), requires=["actions"]))
        self.assertFalse(capable(new, requires=["actions"], fixture={"seed": "s", "web": {"pages": []}}))
        self.assertFalse(capable(step("p", "snapshot", target={"overlay": "palette"}), requires=["gui"]))
        self.assertFalse(capable(step("r", "resource.call", args={}), requires=["resource_api"]))
        self.assertFalse(capable(requires=["gui"]))

    def test_cli_steps_need_a_bound_action_a_served_method_and_no_routing(self) -> None:
        """The step shapes of tours/core.json and tours/home.json."""
        def cli(*argv: str) -> bool:
            return driver_headless.headless_capable(case(step("c", "cli", args={"argv": list(argv)}),
                                                         requires=["gui"]), HOST)

        self.assertTrue(cli("action", "run", "splitRight"))
        self.assertTrue(cli("--json", "action", "run", "splitRight"))
        self.assertFalse(cli("action", "run", "appStore.show", "--tab", "features", "--focus"))
        self.assertFalse(cli("--json", "action", "run", "commandPalette"))
        self.assertFalse(cli("action", "run"))
        self.assertTrue(cli("--json", "app", "call", "debug.focus"))
        self.assertFalse(cli("app", "call", "debug.home.sidebar_fixture", '{"search":true}'))
        self.assertFalse(cli("app", "call", "debug.appearance", '{"mode":"dark"}'))
        self.assertTrue(cli("settings", "set", "appearance.theme", "Soft Blue"))
        self.assertTrue(cli("--idempotency-key", "k1", "workspace", "new", "--name", "Docs"))
        for routed in (["--socket", "/tmp/x", "tree"], ["--session=main", "tree"], ["--app-socket", "/x", "tree"],
                       ["--machine", "m", "tree"], ["--all-sessions", "workspace", "list"], ["--json"]):
            self.assertFalse(cli(*routed), routed)

    def test_a_wait_on_a_facet_the_host_lacks_is_not_selected(self) -> None:
        def waits(pointer: str) -> bool:
            return driver_headless.headless_capable(case(step("c", "cli", args={"argv": ["tree"]},
                                                              waitFor={"pointer": pointer, "predicate": "exists",
                                                                       "maxMs": 1000}), requires=["gui"]), HOST)

        self.assertTrue(waits("/topology/workspaces"))
        self.assertTrue(waits("/terminals/0/rows"))
        for gui in ("/layout/screens/0/workspace", "/webviews/pages", "/windows/0", "/overlays/palette", "/ax"):
            self.assertFalse(waits(gui), gui)
        self.assertFalse(waits("/history/0"), "a facet the host does not list")
        self.assertFalse(waits("topology"), "a malformed pointer")

    def test_capable_cases_asks_the_host_for_its_capabilities(self) -> None:
        listed = Recorder(stdout=listing())
        predicate, note = driver_headless.capable_cases("/x/CmuxNextHeadless", run=listed,
                                                        shell_exists=lambda path: True)
        self.assertEqual(listed.calls[0]["argv"], ["/x/CmuxNextHeadless", "--list-actions"])
        self.assertEqual((note["actions"], note["methods"], note["facets"]), (3, 6, sorted(FACETS)))
        self.assertTrue(predicate(case(step("n", "action.run", args={"action": "workspace.new"}))))
        bad_outputs = ("not json", '{"actions": [1], "methods": [], "facets": []}', listing(methods=None),
                       listing(facets="topology"))
        for bad in (*(Recorder(stdout=o) for o in bad_outputs), Recorder(returncode=3)):
            with self.assertRaises(driver_headless.HostRefused):
                driver_headless.capable_cases("/x/CmuxNextHeadless", run=bad, shell_exists=lambda path: True)

    def test_a_host_that_lists_no_settle_or_wait_for_is_refused(self) -> None:
        for method in ("debug.oracle.settle", "debug.oracle.wait_for"):
            run = Recorder(stdout=listing(methods=sorted(METHODS - {method})))
            with self.assertRaises(driver_headless.HostRefused) as caught:
                driver_headless.capable_cases("/x/CmuxNextHeadless", run=run, shell_exists=lambda path: True)
            self.assertIn(method, str(caught.exception))

    def test_a_machine_without_the_app_shell_is_refused(self) -> None:
        with self.assertRaises(driver_headless.HostRefused) as caught:
            driver_headless.capable_cases("/x/CmuxNextHeadless", run=Recorder(stdout=listing()),
                                          shell_exists=lambda path: False)
        self.assertIn("/bin/zsh", str(caught.exception))

    def test_oracle_selection_widens_through_the_hook_and_records_it(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            corpus = tmp / "corpus"
            cases = {
                "actions/new": case(step("new", "action.run", args={"action": "workspace.new"}),
                                    requires=["actions", "gui"]),
                "actions/key": case(step("k", "key", args={"keys": ["cmd+n"]}), requires=["actions", "gui", "keys"]),
                "daemon-ops/r": case(step("r", "resource.call", args={}), requires=["resource_api"]),
                "mcp/one": case(step("m", "mcp", args={"tool": "x"}), requires=["mcp"]),
                "own/mac-only": case(step("n", "action.run", args={"action": "workspace.new"}), requires=["actions"]),
            }
            for cid, data in cases.items():
                (corpus / cid).parent.mkdir(parents=True, exist_ok=True)
                # case() cases are mac-only, as the app window cases are.
                (corpus / f"{cid}.json").write_text(json.dumps({**data, "id": cid.split("/")[1]}))
            loaded, errors = oracle.load_corpus(corpus)
            self.assertEqual(errors, [])
            predicate = lambda data: driver_headless.headless_capable(data, HOST)  # noqa: E731
            selected, _ = oracle.select_cases(loaded, suites=["all"], filters=[], product="headless", platform="linux",
                                              capable=predicate)
            self.assertEqual([c.id for c in selected], ["actions/new"],
                             "a widened app case runs on the VM; the product's own mac-only case does not")
            selected, _ = oracle.select_cases(loaded, suites=["all"], filters=[], product="headless", platform="mac",
                                              capable=predicate)
            self.assertEqual([c.id for c in selected], ["actions/new", "own/mac-only"])
            selected, _ = oracle.select_cases(loaded, suites=["all"], filters=[], product="headless", platform="linux")
            self.assertEqual(selected, [], "without the host's list only capability cases run (none here)")
            options = driver_app_test_options(tmp, product="headless", dry_run=True)
            receipt, _ = quiet(oracle.perform_run, options, loaded, None, None, {}, capable=(predicate, {"actions": 2}))
            self.assertEqual(receipt["capable"], {"actions": 2, "widened": 1})
            self.assertEqual(receipt["order"], ["actions/new"])

    def test_headless_defaults_are_the_vm_jobs_and_a_batch_of_ten(self) -> None:
        args = oracle.build_parser().parse_args(["run", "--product", "headless", "--cmux-tui", "/x/cmux-tui"])
        options = oracle.options_from_args(args, retry_default=None, out=Path("/tmp/x"), run_id="r", switch=None,
                                           arm=None, switches={}, ref=None, binary="/x/CmuxNextHeadless")
        self.assertEqual((options.jobs, options.batch_limit, options.cmux_tui), (200, 10, "/x/cmux-tui"))
        self.assertNotIn("resource_api", oracle.PRODUCTS["headless"], "resource.call cases are the daemon's")


def driver_app_test_options(tmp: Path, **overrides: Any) -> oracle.RunOptions:
    base: Dict[str, Any] = dict(
        product="headless", suites=("all",), filters=(), shard=(0, 1), jobs=4, batch_limit=1, repeat=1, retry_limit=0,
        seed=None, platform="linux", build="debug", switch=None, arm=None, switches={}, ref=SHA, tag=None,
        binary=None, corpus=tmp / "corpus", out=tmp / "out", state_base=Path("/tmp/o"), run_id="r",
        expectations_root=tmp / "expectations", isolation="none")
    base.update(overrides)
    return oracle.RunOptions(**base)


def quiet(fn, *args, **kwargs):
    with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
        return fn(*args, **kwargs)


class NoRecorderHost(FakeApp):
    """A host whose Control has no turn recorder (the debug.oracle recorder methods are not bound)."""

    m_debug_oracle_recorder = None  # type: ignore[assignment]
    m_debug_oracle_turns = None  # type: ignore[assignment]


class NoSettleHost(FakeApp):
    """A host built short of the contract: no debug.oracle.settle."""

    m_debug_oracle_settle = None  # type: ignore[assignment]


class SessionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        self.binary = executable(root / "bin" / "CmuxNextHeadless")
        self.tui = executable(root / "bin" / "cmux-tui")
        self.hosts: List[FakeApp] = []
        self.envs: List[Dict[str, str]] = []
        self.run_proc = Recorder(stdout='{"workspaces": 2}')

    def session(self, host_class=FakeApp, **ctx) -> driver_headless.HeadlessSession:
        def make(binary, cmux_tui, paths, out, environment):
            env = environment((40, 40, 1440, 900))
            self.envs.append(env)
            fake = host_class(binary, out, env)
            fake.cli_path = cmux_tui
            self.hosts.append(fake)
            return fake

        base = {"run_id": "r", "product": "headless", "binary": str(self.binary), "cmux_tui": str(self.tui),
                "ref": SHA, "out_dir": self.tmp.name}
        return driver_headless.HeadlessSession({**base, **ctx}, make_host=make, canonicalizer=identity_canonicalizer(),
                                               run=self.run_proc, host=lambda: {"os": "Linux", "arch": "x86_64"},
                                               root_base=Path(self.tmp.name) / "o", shell_exists=lambda path: True)

    def test_action_case_records_checkpoints_as_the_headless_product(self) -> None:
        session = self.session()
        self.addCleanup(session.close)
        result = session.run_case(case(driver_app_test.NEW_WORKSPACE), {"case_id": "actions/c"})
        self.assertEqual(result["status"], "Pass", result["message"])
        self.assertEqual([(c["step"], c["checkpoint"]) for c in result["checkpoints"]],
                         [("new", "before"), ("new", "turn"), ("new", "settled")])
        self.assertEqual({c["snapshot"]["meta"]["product"] for c in result["checkpoints"]}, {"headless"})
        self.assertEqual(result["checkpoints"][-1]["snapshot"]["facets"]["topology"], {"workspaces": 2})

    def test_host_environment_is_hermetic_and_the_app_launch_s(self) -> None:
        session = self.session()
        self.addCleanup(session.close)
        session.run_case(case(step("s", "snapshot")), {"case_id": "actions/c"})
        env = self.envs[0]
        self.assertEqual(env["HOME"], str(session.paths.home))
        self.assertEqual(env["CMUX_NEXT_SOCKET_MODE"], "automation")
        self.assertEqual(env["CMUX_NEXT_TEST_WINDOW_FRAME"], "40,40,1440,900")
        config = json.loads(Path(env["CMUX_NEXT_CONFIG_FILE"]).read_text())
        self.assertIs(config["ui"]["animations"]["enabled"], False)
        self.assertIs(config["updates"]["checkAutomatically"], False)
        self.assertFalse([k for k in env if k.startswith("CMUX") and k not in (
            "CMUX_NEXT_NO_ACTIVATE", "CMUX_NEXT_SOCKET_MODE", "CMUX_NEXT_AGENT_PANE_MOCK", "CMUX_NEXT_CHIEF_ISOLATED",
            "CMUX_NEXT_ORACLE_ROOT", "CMUX_NEXT_CONFIG_FILE", "CMUX_NEXT_CHIEF_HOME", "CMUX_NEXT_TEST_WINDOW_FRAME")])

    def test_steps_of_other_products_refuse_before_any_launch(self) -> None:
        session = self.session()
        self.addCleanup(session.close)
        for op in ("key", "mcp", "kill", "restart", "resource.call", "tui.key"):
            result = session.run_case(case(step("x", op, args={})), {"case_id": "actions/c"})
            self.assertEqual((result["status"], result.get("step")), ("Crash", "x"), op)
            self.assertIn("does not run on the headless product", result["message"])
        self.assertEqual(self.hosts[0].launches, [])

    def test_cli_runs_the_run_cmux_tui_as_cmux_on_the_host_socket(self) -> None:
        session = self.session()
        self.addCleanup(session.close)
        result = session.run_case(case(step("c", "cli", args={"argv": ["tree", "--json"]})), {"case_id": "tours/c"})
        self.assertEqual(result["status"], "Pass", result["message"])
        call = self.run_proc.calls[0]
        daemon = str(session.paths.root / "d.sock")
        self.assertEqual(call["argv"], ["cmux", "--app-socket", FakeApp.socket_path, "--socket", daemon, "tree",
                                        "--json"])
        self.assertEqual(call["executable"], str(self.tui))
        self.assertEqual(call["stdin"], subprocess.DEVNULL)

    def test_a_host_without_a_recorder_takes_turn_checkpoints_not_comparable(self) -> None:
        session = self.session(host_class=NoRecorderHost)
        self.addCleanup(session.close)
        only_turn = step("again", "action.run", args={"action": "workspace.new"}, checkpoint=["turn"])
        result = session.run_case(case(driver_app_test.NEW_WORKSPACE, only_turn, step("s", "snapshot")),
                                  {"case_id": "actions/c"})
        self.assertEqual(result["status"], "Pass", result["message"])
        self.assertEqual([(c["step"], c["checkpoint"]) for c in result["checkpoints"]],
                         [("new", "before"), ("new", "turn"), ("new", "settled"), ("again", "turn"), ("s", "settled")],
                         "a turn-only step keeps its place, so the step indexes match the app run's")
        for checkpoint in result["checkpoints"]:
            if checkpoint["checkpoint"] == "turn":
                self.assertEqual({v["status"] for v in checkpoint["snapshot"]["facets"].values()}, {"not-comparable"})
        self.assertIn("turn checkpoint not comparable", result["message"])
        self.assertIn("new, again", result["message"])

    def test_a_host_without_settle_fails_the_case_naming_the_contract(self) -> None:
        session = self.session(host_class=NoSettleHost)
        self.addCleanup(session.close)
        result = session.run_case(case(driver_app_test.NEW_WORKSPACE), {"case_id": "actions/c"})
        self.assertEqual((result["status"], result.get("step")), ("Crash", "setup"))
        self.assertIn("serves no debug.oracle.settle", result["message"])
        self.assertIn("CmuxNextHeadless contract", result["message"])

    def test_session_refuses_a_wrong_binary_a_missing_cmux_tui_and_no_sha(self) -> None:
        other = executable(Path(self.tmp.name) / "bin" / "cmux-tui-other")
        for ctx in ({"binary": str(other)}, {"cmux_tui": None}, {"cmux_tui": str(Path(self.tmp.name) / "none")},
                    {"ref": "main"}, {"switches": {"x": "new"}}):
            with self.assertRaises(driver_headless.HostRefused, msg=ctx):
                self.session(**ctx)

    def test_a_machine_without_the_app_shell_refuses_the_session(self) -> None:
        with self.assertRaises(driver_headless.HostRefused):
            driver_headless.HeadlessSession({"run_id": "r", "product": "headless", "binary": str(self.binary),
                                             "cmux_tui": str(self.tui), "ref": SHA, "out_dir": self.tmp.name},
                                            make_host=lambda *a: FakeApp(a[0], a[3], {}), run=self.run_proc,
                                            canonicalizer=identity_canonicalizer(), host=lambda: {},
                                            root_base=Path(self.tmp.name) / "o", shell_exists=lambda path: False)

    def test_daemon_facet_is_not_comparable_when_the_host_daemon_is_gone(self) -> None:
        session = self.session()
        self.addCleanup(session.close)
        self.assertEqual(session.daemon_facet("s")["status"], "not-comparable")


# A stand-in for CmuxNextHeadless (p15-oracle-160): its argv, ready line, control socket, --list-actions and a
# child (its daemon) in its process group that ignores SIGTERM.
STUB_HOST = textwrap.dedent('''\
    #!{python} -I
    import json, os, signal, socket, subprocess, sys
    argv = sys.argv[1:]
    if argv == ["--list-actions"]:
        print(json.dumps({{"actions": ["workspace.new"], "facets": ["topology"],
                          "methods": ["action.run", "debug.oracle.settle", "debug.oracle.snapshot",
                                      "debug.oracle.wait_for"]}}))
        sys.exit(0)
    opts = dict(zip(argv[::2], argv[1::2]))
    if os.environ.get("STUB_NO_READY"):
        sys.exit(4)
    child = subprocess.Popen([sys.executable, "-I", "-c",
                              "import signal, sys\\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\\n"
                              "print('up', flush=True)\\nsignal.pause()"], stdout=subprocess.PIPE)
    child.stdout.readline()  # its SIGTERM is ignored from here on
    with open(opts["--daemon-socket"] + ".child", "w") as f:
        f.write(str(child.pid))
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(opts["--socket"])
    server.listen(4)
    os.write(int(opts["--ready-fd"]), (json.dumps({{"ready": {{"socket": opts["--socket"],
             "daemonSocket": opts["--daemon-socket"]}}}}) + "\\n").encode())
    os.close(int(opts["--ready-fd"]))
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    while True:
        conn, _ = server.accept()
        request = json.loads(conn.makefile().readline())
        result = [] if request["params"].get("list") else {{"cwd": os.getcwd(), "home": os.environ["HOME"]}}
        conn.sendall((json.dumps({{"id": 1, "ok": True, "result": result}}) + "\\n").encode())
        conn.close()
''')


class PortTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.mkdtemp(prefix="hp", dir="/tmp")
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", self.tmp], check=False))
        self.binary = executable(Path(self.tmp) / "CmuxNextHeadless", STUB_HOST.format(python=sys.executable))
        self.paths = driver_app.OraclePaths(Path(self.tmp) / "r")
        self.paths.home.mkdir(parents=True)

    def port(self, env: Dict[str, str]) -> driver_headless.HeadlessPort:
        port = driver_headless.HeadlessPort(str(self.binary), "/bin/true", self.paths, Path(self.tmp),
                                            lambda window: {"PATH": os.environ["PATH"], "HOME": str(self.paths.home),
                                                            "CMUX_NEXT_CONFIG_FILE": "/x", **env})
        self.addCleanup(port.stop)
        return port

    @unittest.skipUnless(hasattr(os, "pidfd_open"), "the exit of an orphan is read through a Linux pidfd")
    def test_launch_waits_on_the_ready_line_and_stop_ends_the_whole_group(self) -> None:
        port = self.port({})
        port.launch(driver_app.LaunchSpec(window=(40, 40, 1440, 900)))
        self.assertTrue(port.alive())
        reply = port.request("debug.oracle.snapshot", {"facets": []}, 10)
        self.assertEqual(reply["result"], {"cwd": os.path.realpath(self.paths.home), "home": str(self.paths.home)})
        self.assertEqual(port.daemon_socket(), str(self.paths.root / "d.sock"))
        self.assertEqual(port.cli_env(), {"PATH": os.environ["PATH"], "HOME": str(self.paths.home),
                                          "CMUX_NEXT_CONFIG_FILE": "/x",
                                          "CMUX_TUI_SOCKET": str(self.paths.root / "d.sock")},
                         "the CLI reaches the session's own daemon, not the runner's default session")
        child = os.pidfd_open(int(Path(str(self.paths.root / "d.sock") + ".child").read_text()))
        self.addCleanup(os.close, child)
        started = time.monotonic()
        driver_headless.QUIT_S, saved = 0.5, driver_headless.QUIT_S
        try:
            port.stop()
        finally:
            driver_headless.QUIT_S = saved
        self.assertFalse(port.alive())
        self.assertLess(time.monotonic() - started, 5)
        # A pidfd reads ready once its process has exited (the SIGKILLed orphan's reaper is init).
        ended, _, _ = select.select([child], [], [], 5)
        self.assertEqual(ended, [child], "the host's SIGTERM-ignoring child outlived the session (I6)")

    def test_a_host_that_exits_before_ready_is_a_launch_failure(self) -> None:
        port = self.port({"STUB_NO_READY": "1"})
        with self.assertRaises(driver_app.AppLaunchFailed) as caught:
            port.launch(driver_app.LaunchSpec(window=(40, 40, 1440, 900)))
        self.assertEqual(caught.exception.reason, "no-ready")
        self.assertFalse(port.alive())

    def test_list_actions_reads_the_stub(self) -> None:
        host = driver_headless.list_actions(str(self.binary))
        self.assertEqual((host.actions, host.facets), (frozenset({"workspace.new"}), frozenset({"topology"})))


# ---------------------------------------------------------------------------------------------------------
# Product parity


class ProductDriver:
    """A fake product: `facets(case_id, checkpoint)` gives each checkpoint's facets, `status` each case's result."""

    def __init__(self, facets, status=None, checkpoints=("before", "turn", "settled"), stops=None):
        self.facets = facets
        self.status = status or {}
        self.checkpoints = checkpoints
        # case id -> (the step a non-Pass result stopped at, its [(step, checkpoint)] stream)
        self.stops = stops or {}

    def open_session(self, ctx):
        return self

    def run_case(self, data, ctx):
        cid = ctx["case_id"]
        stop, stream = self.stops.get(cid, (None, [("s1", c) for c in self.checkpoints]))
        result = {"status": self.status.get(cid, "Pass"), "message": "",
                  "checkpoints": [{"step": s, "checkpoint": c, "snapshot": {"meta": {"product": "x"},
                                                                            "facets": self.facets(cid, c)}}
                                  for s, c in stream]}
        if stop:
            result["step"] = stop
        return result

    def close(self):
        pass


def nc(reason: str) -> Dict[str, str]:
    return {"status": "not-comparable", "reason": reason}


class ParityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="parity"))
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", str(self.tmp)], check=False))
        corpus = self.tmp / "corpus"
        for name in ("same", "missing", "diverges", "fails", "flaky", "app-only"):
            path = corpus / "actions" / f"{name}.json"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps({"id": name, "steps": [{"id": "s1", "op": "snapshot"}]}))
        self.cases, errors = oracle.load_corpus(corpus)
        assert errors == [], errors

    def run_product(self, product: str, driver: ProductDriver, filters=(), **extra) -> Path:
        out = self.tmp / product
        options = driver_app_test_options(self.tmp, product=product, out=out, filters=tuple(filters), **extra)
        expectation = None
        quiet(oracle.perform_run, options, self.cases, driver.open_session, expectation, {},
              isolate=lambda mode: {"mode": mode, "kind": "none", "reason": "test"})
        return out

    def app_facets(self, cid, checkpoint):
        return {"topology": {"workspaces": 2}, "layout": {"frame": [0, 0, 1440, 900]}, "daemon": {"panes": 1},
                "focus": {"pane": "p1"}}

    def headless_facets(self, cid, checkpoint):
        facets = {"topology": {"workspaces": 2}, "layout": nc("no window system on the headless host"),
                  "daemon": {"panes": 1}, "focus": {"pane": "p1"}}
        if cid == "actions/same":
            facets["layout"] = {"frame": [0, 0, 1440, 900]}
        if cid == "actions/diverges" and checkpoint == "settled":
            facets["topology"] = {"workspaces": 3}
        return facets

    def parity(self, *argv: str):
        out = self.tmp / "parity"
        args = ["parity", "--a-run", str(self.tmp / "app"), "--b-run", str(self.tmp / "headless"), "--out", str(out),
                *argv]
        code = quiet(oracle.main, args)
        return code, json.loads((out / "parity.json").read_text())

    def write_runs(self, headless_checkpoints=("before", "turn", "settled")) -> None:
        self.run_product("app", ProductDriver(self.app_facets, status={"actions/fails": "Pass"}))
        filters = ["actions/same", "actions/missing", "actions/diverges", "actions/fails", "actions/flaky"]
        driver = ProductDriver(self.headless_facets, status={"actions/fails": "Failure"},
                               checkpoints=headless_checkpoints)
        flaky = {"n": 0}
        base_run = driver.run_case

        def run_case(data, ctx):
            result = base_run(data, ctx)
            if ctx["case_id"] == "actions/flaky":
                flaky["n"] += 1
                result["status"] = "Failure" if flaky["n"] == 1 else "Pass"
            return result

        driver.run_case = run_case
        self.run_product("headless", driver, filters=filters, retry_limit=1)

    def test_every_difference_is_missing_facet_or_divergence(self) -> None:
        self.write_runs()
        code, doc = self.parity()
        self.assertEqual(code, 1)
        self.assertEqual(doc["schema"], "cmux-oracle-parity-v1")
        cases = doc["cases"]
        self.assertEqual(cases["actions/same"]["status"], "equal")
        self.assertEqual(cases["actions/missing"]["status"], "missing-facet")
        self.assertEqual([(d["class"], d["facet"], d["missing_on"], d["count"])
                          for d in cases["actions/missing"]["differences"]],
                         [("missing-facet", "layout", "headless", 3)])
        diverged = cases["actions/diverges"]["differences"]
        self.assertEqual([(d["class"], d["facet"], d["checkpoint"], d["pointer"], d["a"], d["b"])
                          for d in diverged if d["class"] == "divergence"],
                         [("divergence", "topology", "settled", "/facets/topology/workspaces", 2, 3)])
        self.assertEqual(cases["actions/fails"]["differences"][0]["pointer"], "/result")
        self.assertEqual(doc["divergent"], ["actions/diverges", "actions/fails"])
        self.assertIn("actions/flaky", doc["excluded"])
        self.assertEqual(doc["only"]["app"], ["actions/app-only"])
        self.assertEqual(doc["facets"]["layout"], {"equal": 1, "missing-facet": 3, "divergence": 0})
        for record in cases.values():
            for item in record["differences"]:
                self.assertIn(item["class"], ("missing-facet", "divergence"))

    def test_a_turn_checkpoint_the_host_does_not_record_is_missing_facet(self) -> None:
        self.write_runs(headless_checkpoints=("before", "settled"))
        _, doc = self.parity()
        missing = [d for d in doc["cases"]["actions/same"]["differences"]]
        self.assertEqual([(d["class"], d["facet"], d["checkpoint"], d["missing_on"]) for d in missing],
                         [("missing-facet", "*", "turn", "headless")])
        self.assertEqual(doc["cases"]["actions/same"]["status"], "missing-facet")
        fails = doc["cases"]["actions/fails"]["differences"]
        self.assertEqual([d["pointer"] for d in fails if d["class"] == "divergence"], ["/result"])

    def test_a_settled_checkpoint_on_one_side_only_is_a_divergence(self) -> None:
        self.write_runs(headless_checkpoints=("before", "turn"))
        _, doc = self.parity()
        same = doc["cases"]["actions/same"]
        self.assertEqual(same["status"], "divergent")
        self.assertEqual([(d["class"], d["pointer"], d["checkpoint"], d["a"], d["b"]) for d in same["differences"]],
                         [("divergence", "/checkpoint", "settled", "s1", None)])

    def test_equal_failures_at_different_steps_are_a_divergence(self) -> None:
        app = ProductDriver(self.app_facets, status={"actions/fails": "Failure"},
                            stops={"actions/fails": ("s2", [("s1", "before"), ("s1", "settled"), ("s2", "before")])})
        self.run_product("app", app)
        headless = ProductDriver(self.headless_facets, status={"actions/fails": "Failure"},
                                 stops={"actions/fails": ("s1", [("s1", "before")])})
        self.run_product("headless", headless, filters=["actions/same", "actions/fails"])
        code, doc = self.parity()
        self.assertEqual(code, 1)
        fails = doc["cases"]["actions/fails"]
        self.assertEqual(fails["status"], "divergent")
        self.assertEqual(fails["results"], {"app": "Failure", "headless": "Failure"})
        self.assertEqual([(d["pointer"], d["a"], d["b"]) for d in fails["differences"] if d["class"] == "divergence"],
                         [("/result/step", "s2", "s1"), ("/checkpoint", "s1", None)])
        self.assertFalse([d for d in fails["differences"] if d["class"] == "missing-facet" and d["facet"] == "*"],
                         "checkpoints the failing side never reached are not a missing facet")
        self.assertEqual(doc["divergent"], ["actions/fails"])

    def test_expected_rules_explain_a_divergence(self) -> None:
        self.write_runs()
        rules = self.tmp / "rules.json"
        rules.write_text(json.dumps([{"pointer": "/facets/topology/workspaces", "case": "actions/diverges",
                                      "reason": "the host opens one more workspace", "task": "p15-oracle-161"}]))
        _, doc = self.parity("--expected", str(rules))
        self.assertEqual(doc["cases"]["actions/diverges"]["status"], "missing-facet")
        self.assertEqual(doc["cases"]["actions/diverges"]["explained"][0]["facet"], "topology")
        self.assertEqual(doc["divergent"], ["actions/fails"])

    def test_refusals(self) -> None:
        self.write_runs()
        out = self.tmp / "same"
        code = quiet(oracle.main, ["parity", "--a-run", str(self.tmp / "app"), "--b-run", str(self.tmp / "app"),
                                   "--out", str(out)])
        self.assertEqual(code, oracle.EXIT_REFUSED)
        self.assertIn("both runs are product app", json.loads((out / "parity.json").read_text())["refused"])
        self.run_product("headless", ProductDriver(self.headless_facets), ref="c" * 40)
        code, doc = self.parity()
        self.assertEqual(code, oracle.EXIT_REFUSED)
        self.assertIn("different sources", doc["refused"])


if __name__ == "__main__":
    unittest.main()

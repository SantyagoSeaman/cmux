#!/usr/bin/env python3
"""startup-lint.py: Q2 launch calls inside a startup.post, outside one and allowlisted, in Startup/*.swift and in
AppServices.init, with the pinned ast-grep.

Runs on any host (Linux included): no app, no fleet. The first run downloads the pinned ast-grep once (the
pre-push gate's copy); without it the tests skip.

    python3 scripts/cmux-next/tests/test_startup_lint.py
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("startup_lint", HERE / "startup-lint.py")
sl = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = sl  # dataclasses resolve the module by name
SPEC.loader.exec_module(sl)

try:
    AST_GREP = sl.pinned_ast_grep()
except (SystemExit, OSError) as error:  # no network and no cached copy
    AST_GREP = None
    SKIP_REASON = f"pinned ast-grep unavailable: {error}"
else:
    SKIP_REASON = ""

STARTUP = textwrap.dedent("""\
    import AppKit

    // services.comment.start() is a comment, and "x.start()" a string: neither is a call.
    extension AppDelegate {
        func registerCore(services: AppServices) {
            startup.post(.afterStartup, "Home.start", on: .main) { [weak services] in services?.home.start() }
            order.startup.post(.critical, "Daemon.start", on: .main, { services.daemon.start() })
            self.order.startup.post(.afterFirstFrame, "Feeds.follow", on: .lane(.cpu, .idle)) {
                feeds.follow(
                    settings
                )
                services.palette.prepare().load()
            }
            services.debugSettings.start()
            startup.post(.afterStartup, "Late", on: .main) { Task { services.late.load() } }
            services.windows.onFirstWindow = { _ in services.openPanel.warmWhenIdle() }
            queue.post(.afterStartup, "NotTheScheduler", on: .main) { services.queued.install() }
            services.startEnergy()
            WindowActivation.activateApp()
            services.registry.registerHandlers()
            WindowActivation.activate()
            let note = "services.string.start()"
            start()
        }
    }
    """)

APP_SERVICES = textwrap.dedent("""\
    final class AppServices {
        init(environment: AppEnvironment) {
            startup.post(.afterStartup, "Updater.wire", on: .main) { [weak self] in self?.updater.wire() }
            notifications.wire(self)
        }

        func later() { bookmarks.attach(self) }
    }

    final class Other {
        init() { other.start() }
    }
    """)


def write_repo(root: Path, startup: str | None = STARTUP, app_services: str | None = APP_SERVICES) -> Path:
    if startup is not None:
        (root / sl.STARTUP_DIR).mkdir(parents=True)
        (root / sl.STARTUP_DIR / "Startup+Core.swift").write_text(startup, encoding="utf-8")
        (root / sl.STARTUP_DIR / "Notes.md").write_text("x.start()\n", encoding="utf-8")
    if app_services is not None:
        (root / sl.APP_SERVICES).parent.mkdir(parents=True, exist_ok=True)
        (root / sl.APP_SERVICES).write_text(app_services, encoding="utf-8")
    return root


@unittest.skipIf(AST_GREP is None, SKIP_REASON)
class StartupLintTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def states(self, allowlist=None, **repo):
        write_repo(self.root, **repo)
        rows = sl.calls(self.root, AST_GREP, {} if allowlist is None else allowlist)
        return {(Path(c.path).name, c.callee): c.state for c in rows}, rows

    def test_inside_a_post_is_posted(self):
        states, _ = self.states()
        core = "Startup+Core.swift"
        self.assertEqual(states[(core, "services?.home.start")], "posted")  # trailing closure
        self.assertEqual(states[(core, "services.daemon.start")], "posted")  # closure argument
        self.assertEqual(states[(core, "feeds.follow")], "posted")  # multi-line call in a multi-line post
        self.assertEqual(states[(core, "services.palette.prepare().load")], "posted")  # chained on a call
        self.assertEqual(states[("AppServices.swift", "self?.updater.wire")], "posted")

    def test_outside_a_post_is_a_hit(self):
        states, _ = self.states()
        core = "Startup+Core.swift"
        for callee in ("services.debugSettings.start",  # inline
                       "services.late.load",  # the Task, not the post, is its closest call
                       "services.openPanel.warmWhenIdle",  # a callback closure
                       "services.queued.install",  # a post that is not the startup scheduler's
                       "services.registry.registerHandlers",  # register[A-Za-z]*
                       "WindowActivation.activate"):
            self.assertEqual(states[(core, callee)], "hit", callee)
        self.assertEqual(states[("AppServices.swift", "notifications.wire")], "hit")

    def test_allowlisted_callee_is_not_a_hit(self):
        allow = {"services.debugSettings.start": "fixture reason"}
        states, _ = self.states(allowlist=allow)
        self.assertEqual(states[("Startup+Core.swift", "services.debugSettings.start")], "allowlisted")
        self.assertEqual(states[("Startup+Core.swift", "WindowActivation.activate")], "hit")

    def test_only_q2_calls_in_scope_are_listed(self):
        states, rows = self.states()
        callees = {callee for _, callee in states}
        # Not Q2 names, no receiver, comments and strings, a Markdown file, other AppServices members, other inits.
        for absent in ("services.startEnergy", "WindowActivation.activateApp", "start", "services.comment.start",
                       "services.string.start", "x.start","bookmarks.attach", "other.start", "startup.post", "order.startup.post"):
            self.assertNotIn(absent, callees)
        self.assertEqual(len(rows), 12)
        self.assertEqual(sum(c.state == "hit" for c in rows), 7)

    def test_line_is_the_call_line(self):
        _, rows = self.states()
        by_callee = {c.callee: c.line for c in rows}
        self.assertEqual(by_callee["feeds.follow"], STARTUP.splitlines().index("            feeds.follow(") + 1)
        self.assertEqual(by_callee["notifications.wire"], 4)

    def test_no_startup_folder_reads_app_services_only(self):
        _, rows = self.states(startup=None)
        self.assertEqual({c.callee for c in rows}, {"self?.updater.wire", "notifications.wire"})

    def run_main(self, *args):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = sl.main(["--repo", str(self.root), "--ast-grep", str(AST_GREP), *args])
        return code, out.getvalue(), err.getvalue()

    def test_main_prints_hits_and_exits_one(self):
        write_repo(self.root)
        code, out, _ = self.run_main()
        self.assertEqual(code, 1)
        lines = out.splitlines()
        self.assertEqual(lines[-1], "7 hits")
        self.assertTrue(all("runs outside a startup.post" in line for line in lines[:-1]))
        self.assertEqual(len(lines), 8)

    def test_main_zero_hits_exits_zero(self):
        write_repo(self.root, startup=None, app_services=textwrap.dedent("""\
            final class AppServices {
                init() { startup.post(.afterStartup, "A.start", on: .main) { a.start() } }
            }
            """))
        code, out, _ = self.run_main()
        self.assertEqual((code, out.splitlines()[-1]), (0, "0 hits"))

    def test_main_json_all(self):
        write_repo(self.root)
        code, out, _ = self.run_main("--json", "--all")
        report = json.loads(out)
        self.assertEqual((code, report["hits"], len(report["calls"])), (1, 7, 12))
        self.assertEqual(set(report["calls"][0]), {"path", "line", "callee", "state"})


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""guardrails.py's refactor-switch warning (p15-oracle-172): a push that adds a RefactorSwitchKey case or a
SWITCH_NAMES entry with no file in plans/cmux-next/refactor-switches/ prints one warning line per name, and the
push is never refused for it (the exit code does not change).

The catalog readers run on text; the push tests make a throwaway git repository in a temporary folder and run
check() and main() on it as the pre-push hook does.

Not a CI job (Lawrence 2026-10-23: no new CI gates): run it yourself after a change to the warning:
  python3 -I scripts/cmux-next/tests/test_guardrails_switches.py
"""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True  # loading the tool below must not write __pycache__ beside it

SCRIPTS = Path(__file__).resolve().parents[1]
_spec = importlib.util.spec_from_file_location("cmux_next_guardrails_p15_172", SCRIPTS / "guardrails.py")
guardrails = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = guardrails
_spec.loader.exec_module(guardrails)

APP = guardrails.APP_SWITCH_CATALOG
DAEMON = guardrails.DAEMON_SWITCH_CATALOG
REGISTRY = guardrails.SWITCH_REGISTRY

SWIFT_CATALOG = """/// The refactor switches (`case holder` is not a case: a doc comment).
public enum RefactorSwitchKey: String, Sendable, CaseIterable {
    case layerHosting = "browser.layer-hosting"
%s}
"""
RUST_CATALOG = """// "commented" is not a name.
pub const SWITCH_NAMES: &[&str] = &[
    "holder",
%s];
"""


def entry(name: str, **key: str) -> str:
    return json.dumps({"name": name, "owner": "perf-lead", "plan": "plans/cmux-next/chrome-parity/p15-oracle.md",
                       "read": "launch", "default": "old", "planned": "new", "created": "2026-10-20",
                       "expires": "2026-11-01", "suites": ["daemon-ops"], "expected_differences": [], **key})


BASE = {
    APP: SWIFT_CATALOG % "",
    DAEMON: RUST_CATALOG % "",
    REGISTRY + "layer-hosting.json": entry("layer-hosting", app_key="browser.layer-hosting"),
    REGISTRY + "holder.json": entry("holder", daemon_key="holder"),
    REGISTRY + "README.md": "# Refactor switches\n",
}


class CatalogTests(unittest.TestCase):
    def test_app_cases_raw_values_and_lines(self):
        text = SWIFT_CATALOG % '    case cefHelper = "browser.cef-helper", intents\n    // case ghost = "x"\n'
        self.assertEqual(guardrails.app_switch_names(text),
                         {"browser.layer-hosting": 3, "browser.cef-helper": 4, "intents": 4})

    def test_app_catalog_without_the_enum_has_no_names(self):
        self.assertEqual(guardrails.app_switch_names("enum OtherKey: String { case a = \"a\" }\n"), {})

    def test_daemon_names_and_lines(self):
        text = RUST_CATALOG % '    "discard", // "not-a-name"\n'
        self.assertEqual(guardrails.daemon_switch_names(text), {"holder": 3, "discard": 4})


class PushTests(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.TemporaryDirectory(prefix="cmux-guardrails-switches-")
        self.repo = Path(self.root.name) / "repository"
        config = Path(self.root.name) / "gitconfig"
        config.write_text("[user]\n\tname = Fixture\n\temail = fixture@example.com\n[maintenance]\n\tauto = false\n"
                          "[gc]\n\tauto = 0\n")
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        self.env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=str(config), HOME=self.root.name)
        self.saved = {k: os.environ.pop(k) for k in list(os.environ) if k.startswith("GIT_")}
        self.repo.mkdir()
        self.git("init", "--quiet", "--initial-branch=feat-cmux-next")
        self.base = self.commit(BASE, "base")

    def tearDown(self):
        os.environ.update(self.saved)
        self.root.cleanup()

    def git(self, *args: str) -> str:
        return subprocess.run(["git", "-C", str(self.repo), *args], env=self.env, text=True, capture_output=True,
                              check=True).stdout.strip()

    def commit(self, files: dict[str, str], message: str) -> str:
        for relative, text in files.items():
            path = self.repo / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        self.git("add", "-A")
        self.git("commit", "--quiet", "--no-verify", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def switch_warnings(self, files: dict[str, str]) -> list[str]:
        head = self.commit(files, "change")
        refusals, warnings = guardrails.check(self.repo, self.base, head)
        self.assertEqual([w for w in warnings if w[0] == "guardrails"], [])
        self.assertEqual(refusals, [])
        return [text for kind, text in warnings if kind == "refactor-switch"]

    def test_an_unregistered_app_case_warns_once(self):
        found = self.switch_warnings({APP: SWIFT_CATALOG % '    case cefHelper = "browser.cef-helper"\n'})
        self.assertEqual(len(found), 1)
        self.assertTrue(found[0].startswith(f"{APP}:4: adds refactor switch 'browser.cef-helper'"), found[0])
        self.assertIn("app_key", found[0])

    def test_an_unregistered_daemon_name_warns_once(self):
        found = self.switch_warnings({DAEMON: RUST_CATALOG % '    "discard",\n'})
        self.assertEqual(len(found), 1)
        self.assertTrue(found[0].startswith(f"{DAEMON}:4: adds refactor switch 'discard' (CMUX_TUI_SWITCHES)"),
                        found[0])

    def test_a_name_registered_in_the_same_push_passes(self):
        self.assertEqual(self.switch_warnings({
            APP: SWIFT_CATALOG % '    case cefHelper = "browser.cef-helper"\n',
            DAEMON: RUST_CATALOG % '    "discard",\n',
            REGISTRY + "cef-helper.json": entry("cef-helper", app_key="browser.cef-helper"),
            REGISTRY + "discard.json": entry("discard", daemon_key="discard")}), [])

    def test_a_key_registered_on_the_other_side_still_warns(self):
        """A daemon_key "discard" does not register an app case "discard"."""
        found = self.switch_warnings({APP: SWIFT_CATALOG % "    case discard\n",
                                      REGISTRY + "discard.json": entry("discard", daemon_key="discard")})
        self.assertEqual(len(found), 1)
        self.assertIn("'discard'", found[0])

    def test_names_the_base_already_had_and_edits_around_them_pass(self):
        """The base's unregistered names are not this push's, and reformatting the catalog adds none."""
        self.base = self.commit({DAEMON: RUST_CATALOG % '    "legacy",\n'}, "legacy base")
        self.assertEqual(self.switch_warnings({
            DAEMON: '/// Names.\npub const SWITCH_NAMES: &[&str] = &["holder", "legacy"];\n',
            APP: SWIFT_CATALOG.replace("CaseIterable", "CaseIterable, Codable") % ""}), [])

    def test_a_push_off_the_catalogs_reads_no_registry(self):
        self.assertEqual(self.switch_warnings({REGISTRY + "README.md": "# Refactor switches (edited)\n"}), [])

    def test_the_hook_prints_one_line_and_exits_zero(self):
        """main() as the hook runs it: one `[refactor-switch]` line for one unregistered name, exit 0."""
        head = self.commit({APP: SWIFT_CATALOG % '    case cefHelper = "browser.cef-helper"\n'}, "unregistered")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = guardrails.main(["--repo", str(self.repo), "--base", self.base, "--head", head])
        lines = [line for line in out.getvalue().splitlines() if "[refactor-switch]" in line]
        self.assertEqual(code, 0, out.getvalue())
        self.assertEqual(len(lines), 1, out.getvalue())
        self.assertIn("browser.cef-helper", lines[0])


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Launch calls that run outside a StartupScheduler post (plans/cmux-next/chrome-parity/p08-startup.md, task 104).

Done check 8(4) of the startup program: every Q2 launch call (a start/follow/wire/load/install/... call, the
same names as the section 6 Q2 regex) in `CmuxNextApp/Startup/*.swift` and in `AppServices.init` runs as a
`startup.post(...)` task, so the scheduler orders, measures and defers it (Chrome's
`AfterStartupTaskUtils::PostTask`, chrome/browser/after_startup_task_utils.cc: launch work is posted, never run
inline). A call is a hit when its closest enclosing call is not `<...>startup.post(...)` and its callee is not in
ALLOWLIST. "Closest enclosing call" is literal: `startup.post(...) { Task { x.start() } }` is a hit, because the
Task, not the post, runs the call.

The pinned ast-grep (scripts/cmux-next/lint-tools.lock, the pre-push gate's copy) parses the Swift, so comments
and strings never match and multi-line calls count once. This is an agent tool, not a CI step or a gate check.

    python3 scripts/cmux-next/startup-lint.py            # hits, then "N hits"; exit 1 when N > 0
    python3 scripts/cmux-next/startup-lint.py --all      # every Q2 call with its state (posted, allowlisted, hit)
    python3 scripts/cmux-next/startup-lint.py --json     # the same rows as JSON
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import subprocess
import sys
from dataclasses import asdict, dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
LOCK = HERE / "lint-tools.lock"
APP = "Packages/macOS/CmuxNext/Sources/CmuxNextApp"
STARTUP_DIR = f"{APP}/Startup"
APP_SERVICES = f"{APP}/AppServices.swift"

# The method names of the Q2 regex (p08-startup.md, section 6), as one anchored alternation.
Q2_NAMES = (r"^(start|follow|wire|load|install|prewarm|register[A-Za-z]*|activate|attach|keepCurrent|startCloud"
            r"|startChromiumRuntime|observe[A-Za-z]*|warmWhenIdle|bindRegistryActions|startDaemonSync|watchScope"
            r"|writeManagedStatus|applyManagedFeaturesNow)$")

# A scheduler post: `startup.post(`, `order.startup.post(`, `self?.startup.post(`.
POST_RECEIVER = r"(^|\.)startup$"

# Callees (whitespace removed, as printed) that may run outside a post, each with the reason. A new entry needs
# its row in plans/cmux-next/startup-DESIGN.md saying why the launch needs it inline and what it costs.
ALLOWLIST: dict[str, str] = {}


@dataclass(frozen=True)
class Call:
    path: str
    line: int
    callee: str
    state: str  # "posted", "allowlisted" or "hit"


def _gate_module():
    spec = importlib.util.spec_from_file_location("cmux_next_prepush_gate", HERE / "prepush-gate.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def pinned_ast_grep() -> Path:
    """The lock's ast-grep, downloaded and sha256-checked once (shared with the pre-push gate)."""
    return _gate_module().ensure_tool(LOCK, "ast-grep")


def _q2_callee() -> dict:
    # The callee of a call (a navigation_expression is a call's callee when the call is its parent) whose last
    # member is a Q2 name and that has a receiver, like the regex's `[A-Za-z.?]+\.name\(`.
    return {
        "kind": "navigation_expression",
        "has": {"field": "suffix", "has": {"field": "suffix", "regex": Q2_NAMES}},
        "inside": {"kind": "call_expression"},
    }


def _closest_call_is_post() -> dict:
    # The nearest call_suffix above the callee (arguments or trailing closure of the closest enclosing call; a
    # call the callee only chains on, `a.b().start()`, has it in its callee, not its suffix) belongs to a post.
    post_callee = {
        "kind": "navigation_expression",
        "has": {"field": "suffix", "has": {"field": "suffix", "regex": r"^post$"}},
        "all": [{"has": {"field": "target", "regex": POST_RECEIVER}}],
    }
    return {
        "inside": {
            "kind": "call_suffix",
            "stopBy": {"kind": "call_suffix"},
            "inside": {"kind": "call_expression", "has": post_callee},
        }
    }


def _rule(rule_id: str, scope: dict | None, posted: bool) -> dict:
    rule = _q2_callee()
    conditions = [_closest_call_is_post() if posted else {"not": _closest_call_is_post()}]
    if scope is not None:
        conditions.append(scope)
    rule["all"] = conditions
    return {"id": rule_id, "language": "swift", "severity": "info", "message": rule_id, "rule": rule}


# AppServices.init: an init declared directly in `class AppServices`.
APP_SERVICES_INIT = {
    "inside": {
        "kind": "init_declaration",
        "stopBy": "end",
        "inside": {"kind": "class_body", "inside": {"kind": "class_declaration",
                                                     "has": {"field": "name", "regex": r"^AppServices$"}}},
    }
}


def _scan(binary: Path, repo: Path, files: list[str], scope: dict | None) -> list[tuple[str, int, str, bool]]:
    if not files:
        return []
    rules = "\n---\n".join(json.dumps(_rule(rule_id, scope, posted))
                           for rule_id, posted in (("posted", True), ("outside", False)))
    result = subprocess.run([str(binary), "scan", "--inline-rules", rules, "--json=compact", *files],
                            cwd=repo, capture_output=True, text=True, stdin=subprocess.DEVNULL)
    try:
        matches = json.loads(result.stdout or "[]")
    except json.JSONDecodeError as error:
        raise SystemExit(f"startup-lint: ast-grep failed ({error}): {result.stderr.strip()}") from error
    if result.returncode not in (0, 1) and not matches:
        raise SystemExit(f"startup-lint: ast-grep exited {result.returncode}: {result.stderr.strip()}")
    return [(m["file"], m["range"]["start"]["line"] + 1, "".join(m["text"].split()), m["ruleId"] == "posted")
            for m in matches]


def startup_files(repo: Path) -> list[str]:
    folder = repo / STARTUP_DIR
    return sorted(str(p.relative_to(repo)) for p in folder.glob("*.swift")) if folder.is_dir() else []


def calls(repo: Path, binary: Path, allowlist: dict[str, str] = ALLOWLIST) -> list[Call]:
    """Every Q2 call in the Startup files and AppServices.init, in file and line order."""
    found = _scan(binary, repo, startup_files(repo), None)
    if (repo / APP_SERVICES).is_file():
        found += _scan(binary, repo, [APP_SERVICES], APP_SERVICES_INIT)
    rows = []
    for path, line, callee, posted in found:
        state = "posted" if posted else "allowlisted" if callee in allowlist else "hit"
        rows.append(Call(path, line, callee, state))
    return sorted(set(rows), key=lambda c: (c.path, c.line, c.callee))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--repo", type=Path, default=REPO, help="checkout to read (default: this one)")
    parser.add_argument("--all", action="store_true", help="list posted and allowlisted calls too")
    parser.add_argument("--json", action="store_true", help="print the rows as JSON")
    parser.add_argument("--ast-grep", type=Path, help="ast-grep binary (default: the pinned one)")
    args = parser.parse_args(argv)

    repo = args.repo.resolve()
    rows = calls(repo, args.ast_grep or pinned_ast_grep())
    hits = [c for c in rows if c.state == "hit"]
    shown = rows if args.all else hits
    if args.json:
        print(json.dumps({"hits": len(hits), "calls": [asdict(c) for c in shown]}, indent=2))
    else:
        for c in shown:
            suffix = f" [{c.state}]" if args.all else ""
            print(f"{c.path}:{c.line}: {c.callee}(...) runs outside a startup.post{suffix}" if c.state == "hit"
                  else f"{c.path}:{c.line}: {c.callee}(...){suffix}")
        if not startup_files(repo):
            print(f"startup-lint: no {STARTUP_DIR}/*.swift yet (task 014); only AppServices.init was read",
                  file=sys.stderr)
        print(f"{len(hits)} hits")
    return 1 if hits else 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""App-crash survival of the app's children in the daemon (p01-process-239, feature appChildrenInDaemon).

With the feature on (the tag's Labs local choice, as every bench turns a default-off feature on), one launch starts a
60 s shell-mode command through the agent pane's shell (debug.app_children run_shell: a daemon job, not the app's
child) and, when VS Code is installed, the inline VS Code server (serve_web). Then the app is killed with SIGKILL and
launched again on the same tag, and the relaunched app:

  - reads the command by the id and the offset it had before the kill: the output continues where it stopped
    (the next line number follows the last one read, nothing lost or repeated) and the command is still running;
  - asks for the VS Code URL again: the same URL, connection token included (the server was adopted, not replaced).

Then a clean quit (quitKeepSessions: the daemon stays) and process-model-census.py: no process under the daemon
may still carry this run's marker or run serve-web, since a clean quit ends the app's children at once
(app-child-release-all), not after the daemon's 30 s grace.

  scripts/cmux-next/app-crash-survival.py --tag T [--app APP] [--out DIR] [--vscode "/Applications/Visual Studio Code.app"]
  scripts/cmux-next/app-crash-survival.py ... --story app-crash-survival   (perf-lab: result-v1 into --out)

Metrics (result-v1 with --story): app_crash_survival.shell_continued and app_crash_survival.vscode_same_url (ratio,
1 = survived), app_crash_survival.shell_resume (ms from the relaunch's first answer to the first new output line)
and app_crash_survival.quit_left_children (count). A build whose app does not report the feature on, or a daemon
without app-children-v1, is NOT-MEASURABLE: nothing is recorded and the exit is 2. Any failed check exits 1.
"""
import argparse, glob, json, os, re, signal, subprocess, sys, tempfile, time, uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))  # its siblings, also under `python3 -I`
import launch_journey_report as journey  # noqa: E402
from bench_launch import BenchApp  # noqa: E402
from bench_story import story_problem  # noqa: E402

FEATURE = "appChildrenInDaemon"
STORY_ENGINE = "none"
CENSUS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "process-model-census.py")
DEFAULT_VSCODE = "/Applications/Visual Studio Code.app"
# The command runs this long (the task's acceptance: a 60 s shell-mode command), one numbered line per second.
COMMAND_SECONDS = 60
LINE = re.compile(r"^(?P<mark>acs-[0-9a-f]+) (?P<n>\d+)$")


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--tag", required=True)
    parser.add_argument("--app", help="tagged .app (default: found in DerivedData)")
    parser.add_argument("--out", help="the app log, the census and result-v1 (default a scratch folder)")
    parser.add_argument("--story", help="write result-v1 under this perf-lab story")
    parser.add_argument("--session", choices=["light"], default="light", help="result-v1 session")
    parser.add_argument("--vscode", default=DEFAULT_VSCODE, help="the VS Code app serve_web uses; missing: not measured")
    parser.add_argument("--launch-timeout", type=float, default=120.0)
    args = parser.parse_args(argv)
    if args.story:
        problem = story_problem(args.story, STORY_ENGINE)
        if problem:
            parser.error(problem)
    return args


def tagged_app(args):
    app = args.app or next(iter(sorted(glob.glob(os.path.expanduser(
        f"~/Library/Developer/Xcode/DerivedData/*/Build/Products/*/cmux DEV {args.tag}.app")))), None)
    if not app:
        sys.exit(f"no tagged app for {args.tag}; pass --app")
    return app


class NotMeasurable(Exception):
    pass


class Run:
    def __init__(self, bench, vscode):
        self.bench = bench
        self.vscode = vscode if os.path.isdir(vscode) else None
        self.mark = f"acs-{uuid.uuid4().hex[:12]}"
        self.failures = []
        self.metrics = {}

    def call(self, op, **params):
        reply = self.bench.rpc("debug.app_children", dict(params, op=op), timeout=90)
        if not isinstance(reply, dict) or reply.get("error"):
            raise RuntimeError(f"debug.app_children {op}: {json.dumps(reply)[:300]}")
        return reply

    def check(self, ok, what):
        print(("PASS " if ok else "FAIL ") + what, flush=True)
        if not ok:
            self.failures.append(what)
        return ok

    def require_feature(self):
        state = ((self.bench.features or ({}, None, None))[0].get(FEATURE) or {}).get("state")
        listed = self.call("list")
        if state != "on" or not listed.get("enabled"):
            raise NotMeasurable(f"{FEATURE} is {state or 'undeclared'} in this build's features.list")
        if not listed.get("served"):
            raise NotMeasurable("the daemon does not serve app-children-v1")

    def lines(self, job, after, until, want_line=None):
        """Reads `job` from `after` until `until` (monotonic) or until line `want_line` came: (numbers, next, exit)."""
        numbers, text = [], ""
        exit_ = None
        while time.monotonic() < until:
            chunk = self.call("read_shell", id=job, after=after)
            text += chunk.get("output") or ""
            after = int(chunk.get("next") or after)
            exit_ = chunk.get("exit")
            *complete, text = text.split("\n")
            for line in complete:
                match = LINE.match(line.strip())
                if match and match["mark"] == self.mark:
                    numbers.append(int(match["n"]))
            if exit_ or (want_line is not None and numbers and numbers[-1] >= want_line):
                break
        return numbers, after, exit_

    def before_the_kill(self):
        script = f'i=1; while [ $i -le {COMMAND_SECONDS} ]; do echo "{self.mark} $i"; i=$((i+1)); sleep 1; done'
        job = self.call("run_shell", command=script, cwd=tempfile.gettempdir())["id"]
        numbers, after, _ = self.lines(job, 0, time.monotonic() + 20, want_line=3)
        self.check(bool(numbers) and numbers == list(range(1, len(numbers) + 1)), f"the command streams its lines {numbers}")
        url = self.call("serve_web", vscode=self.vscode)["url"] if self.vscode else None
        return job, after, (numbers[-1] if numbers else 0), url

    def after_the_relaunch(self, job, after, last, url, answered_at):
        numbers, _, exit_ = self.lines(job, after, time.monotonic() + 30, want_line=last + 2)
        resumed = time.monotonic()
        continued = bool(numbers) and numbers[0] == last + 1 and numbers == list(range(last + 1, last + 1 + len(numbers)))
        self.check(continued and exit_ is None,
                   f"after the kill the command still runs and its output continues by offset (last {last}, then {numbers[:5]})")
        self.metrics["shell_continued"] = 1.0 if continued and exit_ is None else 0.0
        if continued:
            self.metrics["shell_resume"] = round((resumed - answered_at) * 1000, 1)
        if url:
            again = self.call("serve_web", vscode=self.vscode)["url"]
            same = again == url
            self.check(same, "VS Code keeps its URL across the kill" + ("" if same else f" ({url} became {again})"))
            self.metrics["vscode_same_url"] = 1.0 if same else 0.0
        else:
            print(f"SKIP VS Code: no VS Code at {self.vscode or DEFAULT_VSCODE}", flush=True)

    def after_a_clean_quit(self, out):
        quit_ = self.bench.quit_keep_sessions()
        self.check(quit_["quit"], f"the clean quit ended the app {quit_['error']}")
        census_path = os.path.join(out, "census-after-quit.json")
        subprocess.run([sys.executable, "-I", CENSUS, "--tag", self.bench.tag, "--no-regions", "--json",
                        "--out", census_path], stdout=subprocess.DEVNULL, check=False, timeout=120)
        try:
            with open(census_path) as handle:
                census = json.load(handle)
        except (OSError, ValueError) as error:
            self.check(False, f"process-model-census.py wrote no census: {error}")
            return
        left = [row for row in (census.get("children") or {}).get("under_daemon") or []
                if self.mark in row["args"] or "serve-web" in row["args"]]
        self.metrics["quit_left_children"] = float(len(left))
        self.check(not left, f"a clean quit leaves no app child under the daemon {left}")


def record(bench, story, metrics):
    units = {"shell_continued": ("ratio", "higher"), "vscode_same_url": ("ratio", "higher"),
             "shell_resume": ("ms", "lower"), "quit_left_children": ("count", "lower")}
    for name, value in metrics.items():
        unit, better = units[name]
        path = bench.record(story, f"app_crash_survival.{name}", unit, [value], better=better, engine=STORY_ENGINE)
        print(f"RESULT app_crash_survival.{name} {value} -> {path}", flush=True)


def main(argv=None, bench_app=BenchApp):
    args = parse_args(argv)
    app = tagged_app(args)
    out = args.out or tempfile.mkdtemp(prefix=f"app-crash-survival-{args.tag}-")
    os.makedirs(out, exist_ok=True)
    bench = bench_app(app, out=out, config={}, log_name="app.log", perf_session=args.session,
                      local_features={FEATURE: True})
    run = Run(bench, args.vscode)
    measured = False
    try:
        bench.launch(timeout=args.launch_timeout)
        journey.remember(bench)
        run.require_feature()
        job, after, last, url = run.before_the_kill()
        bench.process.send_signal(signal.SIGKILL)
        bench.process.wait(20)
        print(f"killed the app (SIGKILL) after line {last}", flush=True)
        journey.relaunch(bench, timeout=args.launch_timeout, probe_first_answer=True)
        run.after_the_relaunch(job, after, last, url, time.monotonic())
        run.after_a_clean_quit(out)
        measured = True
    except NotMeasurable as reason:
        print(f"NOT-MEASURABLE {reason}", flush=True)
        return 2
    finally:
        bench.stop()
    if args.story and measured:
        record(bench, args.story, run.metrics)
    if run.failures:
        print(f"FAIL {len(run.failures)}: {'; '.join(run.failures)}", flush=True)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

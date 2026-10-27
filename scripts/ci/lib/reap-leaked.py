#!/usr/bin/env python3
"""Stop the processes a test run left behind and report them (cx-leo45).

Usage: reap-leaked.py --marker NAME=VALUE --when TEXT [--root DIR]... [--json FILE]

A process is the run's when it is of this user and its environment holds the
marker (set only on the run's cargo commands), or its working directory or
executable is inside a --root (and, on a fleet step, the step's own TMPDIR), or
its parent is such a process. This script and its ancestors are never stopped;
no name is matched. It prints `gate-notice: LEAKED: N processes, M PTYs
reclaimed WHEN` and the leaked command lines, or `leaked: none WHEN`; --json
writes the same counts for a receipt. Used by scripts/ci/cmux-tui-rust-check.sh
(the Rust gate, with its checkout as the root) and
scripts/cmux-next/linux/rust-tests.sh (the marker alone: its checkout is an
agent's worktree that other processes use).
"""
import argparse, hashlib, json, os, re, signal, subprocess, sys, time

parser = argparse.ArgumentParser(description="stop the leftovers of a test run")
parser.add_argument("--marker", required=True, help="NAME=VALUE in the environment of the run's processes")
parser.add_argument("--when", required=True, help="the point of the run, for the report")
parser.add_argument("--root", action="append", default=[], help="a directory only this run uses")
parser.add_argument("--json", help="write the counts to this file")
args = parser.parse_args()
marker, when = args.marker, args.when
if "=" not in marker or marker.startswith("="):
    parser.error("--marker must be NAME=VALUE")
def summary(found, freed, left, groups):
    if not args.json:
        return
    with open(args.json, "w", encoding="utf-8") as handle:
        json.dump({"when": when, "processes": found, "ptys_reclaimed": freed, "still_running": left,
                   "by_command": groups}, handle, indent=2, sort_keys=True)
        handle.write("\n")
uid = os.geteuid()
darwin = sys.platform == "darwin"
def ptys():
    d = "/dev" if darwin else "/dev/pts"
    try:
        return sum(1 for n in os.listdir(d) if ((n.startswith("ttys") and n[4:].isdigit()) if darwin else n.isdigit()))
    except OSError:
        return -1
def run(cmd):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=60).stdout
    except (OSError, subprocess.SubprocessError):
        return ""
roots = list(args.root)
# A fleet step's TMPDIR is its own when the worker made it for this step:
# inside CI_JOB_DIR, or the worker's short path /tmp/cbw-<sha256(CI_JOB_DIR)[:12]>.
job, tmp = os.environ.get("CI_JOB_DIR", ""), os.environ.get("TMPDIR", "").rstrip("/")
if job.startswith("/") and tmp.startswith("/"):
    short = "/tmp/cbw-" + hashlib.sha256(job.encode()).hexdigest()[:12]
    if tmp == short or tmp.startswith(short + "/") or tmp.startswith(job.rstrip("/") + "/"):
        roots.append(tmp)
prefixes = set()
for r in roots:
    for p in (r, os.path.realpath(r)):
        p = os.path.normpath(p)
        if p not in ("/", "."):
            prefixes.add(p)
def inside(path):
    return any(path == p or path.startswith(p + "/") for p in prefixes)
procs = {}  # pid -> [ppid, command, state, seed]
if darwin:
    for line in run(["/bin/ps", "-axww", "-o", "pid=,ppid=,uid=,state=,command="]).splitlines():
        f = line.split(None, 4)
        if len(f) == 5 and f[0].isdigit() and f[2].lstrip("-").isdigit() and int(f[2]) % 2**32 == uid:
            procs[int(f[0])] = [int(f[1]), f[4], f[3], ""]
    for line in run(["/bin/ps", "-axww", "-E", "-o", "pid=,command="]).splitlines():
        f = line.split(None, 1)
        if len(f) == 2 and f[0].isdigit() and int(f[0]) in procs and re.search(r"(^| )" + re.escape(marker) + r"( |$)", f[1]):
            procs[int(f[0])][3] = "env"
    pid = 0
    for line in run(["/usr/sbin/lsof", "-nP", "-w", "-a", "-u", str(uid), "-d", "cwd,txt", "-F", "pn"]).splitlines():
        if line[:1] == "p":
            pid = int(line[1:]) if line[1:].isdigit() else 0
        elif line[:1] == "n" and pid in procs and not procs[pid][3] and inside(line[1:]):
            procs[pid][3] = line[1:]
else:
    for n in os.listdir("/proc"):
        if not n.isdigit():
            continue
        pid = int(n)
        try:
            if os.stat(f"/proc/{n}").st_uid != uid:
                continue
            stat = open(f"/proc/{n}/stat").read()
            rest = stat[stat.rindex(")") + 2:].split()
            cmd = open(f"/proc/{n}/cmdline", "rb").read().replace(b"\0", b" ").decode(errors="replace").strip()
        except (OSError, ValueError):
            continue
        procs[pid] = [int(rest[1]), cmd or stat[stat.index("(") + 1:stat.rindex(")")], rest[0], ""]
        try:
            if marker.encode() in open(f"/proc/{n}/environ", "rb").read().split(b"\0"):
                procs[pid][3] = "env"
        except OSError:
            pass
        for link in ("cwd", "exe"):
            if procs[pid][3]:
                break
            try:
                target = os.readlink(f"/proc/{n}/{link}")
            except OSError:
                continue
            if inside(target.removesuffix(" (deleted)")):
                procs[pid][3] = target
# never this script, its ancestors (the step runner) or pid 1
keep, p = {1, os.getpid()}, os.getppid()
while p > 1 and p not in keep:
    keep.add(p)
    p = procs[p][0] if p in procs else 0
found = {pid for pid, v in procs.items() if v[3] and pid not in keep and v[2][:1] != "Z"}
grew = True
while grew:  # what a leftover started is a leftover too
    grew = False
    for pid, v in procs.items():
        if pid not in found and pid not in keep and v[0] in found and v[2][:1] != "Z":
            found.add(pid)
            grew = True
if not found:
    print(f"leaked: none {when}")
    summary(0, 0, 0, {})
    sys.exit(0)
before = ptys()
for pid in sorted(found):
    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        pass
def living(pids):  # the pids that still run (not gone, not zombies)
    out = set()
    if darwin:
        if pids:
            for line in run(["/bin/ps", "-o", "pid=,state=", "-p", ",".join(map(str, sorted(pids)))]).splitlines():
                f = line.split()
                if len(f) == 2 and f[0].isdigit() and f[1][:1] != "Z":
                    out.add(int(f[0]))
        return out & pids
    for pid in pids:
        try:
            st = open(f"/proc/{pid}/stat").read()
            if st[st.rindex(")") + 2] != "Z":
                out.add(pid)
        except (OSError, ValueError):
            pass
    return out
deadline = time.monotonic() + 15  # a bounded wait for the kills to finish
left = living(found)
while left and time.monotonic() < deadline:
    time.sleep(0.2)
    left = living(left)
after = ptys()
freed = before - after if before >= 0 and after >= 0 else "?"
print(f"gate-notice: LEAKED: {len(found)} processes, {freed} PTYs reclaimed {when} (fixtures must end their own daemons: cx-leo45)")
if left:
    print(f"leaked: {len(left)} still running after SIGKILL: {' '.join(map(str, sorted(left)))}")
groups = {}
for pid in found:
    key = re.sub(r"\d+", "N", procs[pid][1])[:200]
    groups[key] = groups.get(key, 0) + 1
summary(len(found), freed if isinstance(freed, int) else None, len(left), groups)
print(f"leaked {when}, by command (digits as N):")
for key, n in sorted(groups.items(), key=lambda kv: -kv[1])[:40]:
    print(f"  {n:5d}  {key}")
print(f"leaked {when}, each process (pid ppid matched-by command):")
for pid in sorted(found)[:600]:
    v = procs[pid]
    print(f"  {pid} {v[0]} {'env' if v[3] == 'env' else 'child' if not v[3] else 'path'} {v[1][:300]}")

#!/usr/bin/env python3
"""Live queue of the daemon's readers of VT parser state (p01-process Q15, task 208).

Phase B of the process model (plans/cmux-next/chrome-parity/p01-process.md)
moves Ghostty's VT parser out of the daemon into sandboxed `cmux-vt-worker`
processes. Every place the daemon touches parser state today then has to
read the worker's shared screen page, ask the worker asynchronously, move
into the worker, or be shown to read no parser state. Chromium shares state
between processes the same two ways: a one-writer seqlock over read-only
shared memory for values a reader needs at once
(device/base/synchronization/one_writer_seqlock.h, the gamepad and sensor
shared buffers), and an asynchronous mojo call for everything else.

The scope is the Q15 query

    rg -n 'ghostty_vt::' cmux-tui/crates/cmux-tui-core/src \\
        --glob '!**/tests/**' --glob '!*_tests.rs'

plus every parser lock in the same files: the lock itself (`.term.lock(`,
`.term.try_lock(`, `.term.slot.lock(`, `.term.slot.try_lock(`), every read
of a terminal's input modes (`.input_modes(`, `.input_modes.load(`,
`.key_encoder(`: the word the key, paste, focus and mouse encoders read,
task 209) and every call of a parser accessor. The accessors are `with_terminal`,
`try_with_terminal` and `try_with_hot_terminal`, every function in the
scanned files whose signature hands a closure the parser (`Fn(&Terminal)`,
`FnMut(&mut Terminal)`, `FnOnce(&mut Terminal)`), and the item of every
table row whose path is `accessor`. A reader such as
`surface.try_with_terminal(|t| t.viewport_text())` names no `ghostty_vt::`
path, and the lock is where its read starts.

Each match is an access site, keyed by (file, item, symbol):
  file    path under cmux-tui/crates/cmux-tui-core/src;
  item    the enclosing Rust items, outermost first, joined by " > "
          (for example "impl Surface > fn attach_stream"; "use" for an
          import, "(file)" for a match outside any item);
  symbol  the path after `ghostty_vt::` (for example "Error::InvalidValue"),
          "{A,B}" for an import group, or the lock ("term.lock()",
          "term.slot.lock()", "try_with_terminal()"), with "comment:" or "string:" in front of
          a match in a comment or a literal.
Line numbers are only printed; the key survives edits that move lines.

plans/cmux-next/vt-worker-state-access.json classifies them. A row has
`file`, `item`, `symbols` (the symbols of that item it covers), `class`,
`reason`; a page row names the `field` (or fields) of the screen page it
reads, from `page_fields`; an async row names its `path` (request,
background or accessor; p01-process risk 8 counts the request rows). The
classes:
  page       reads a value the worker publishes in the seqlock screen page;
             no hop, safe on the key path (tasks 133, 209);
  async      reads parser state too large or too rare for the page
             (replays, snapshots, history, Kitty images); with process
             workers an asynchronous request to the shard owner, never on a
             key path (task 135);
  in-worker  runs where the parser runs (construction, byte feed, parser
             callbacks, frame builds, viewport and resize changes); it moves
             into the worker and the daemon sends it a command;
  dead       the daemon reads no parser state here: test code, a doc
             comment, a compile-time constant, a type, or formatting of a
             value another row read.
A row's class is what its enclosing item does with parser state, not what
the matched token is: an error value returned by a snapshot function is
`async`. A row over several uses (an import, an item with two reads) takes
the most constrained one (in-worker, then async, then page, then dead) and
its reason names the others.

Usage: scripts/cmux-next/vt-state-access.py [--repo ROOT] [--check] [--list]
                                            [--table PATH] [--json]
  (default)  print the count per class and every unclassified site;
  --check    exit 1 when a site has no row, a row matches no site, or a row
             is malformed (unknown class, page row without a known field,
             async row without a path, empty reason, duplicate key);
  --list     print every site with its line numbers and class;
  --json     print the sites with their classes as JSON;
  --table    use this table instead of the repo's.
Portable: Python 3.9+, no ripgrep and no build. Tested by
scripts/cmux-next/tests/vt-state-access.test.sh.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from collections import Counter
from dataclasses import dataclass, field

CORE = "cmux-tui/crates/cmux-tui-core/src"
TABLE = "plans/cmux-next/vt-worker-state-access.json"
# The Q15 needle and the parser lock; accessor calls are added per scan.
VT_NEEDLE = r"(?P<vt>ghostty_vt::)"
TERM_LOCK = r"\.term\.(?:slot\.)?(?:lock|try_lock)\("
# Reads of a terminal's input modes (task 209): the parser's own
# (`term.input_modes()`) and the published word the key, paste, focus and
# mouse encoders read with no lock (`input_modes.load()`, `Surface::
# input_modes()`, `Surface::key_encoder()`). A page row names that word.
MODES_READ = r"\.(?:input_modes\.load|input_modes|key_encoder)\("
# The daemon's parser accessors; scan() adds every other function that hands
# a closure the parser and the item of every `accessor` row.
ACCESSORS = frozenset({"with_terminal", "try_with_terminal", "try_with_hot_terminal"})
# A function that hands a closure the parser: `fn name<..>(.. impl
# FnOnce(&mut Terminal) ..)` or the same bound in a where clause.
ACCESSOR_FN = re.compile(r"\bfn\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\b(?P<sig>[^{;]*)")
TERMINAL_CLOSURE = re.compile(r"\bFn(?:Once|Mut)?\s*\(\s*&\s*(?:'\w+\s+)?(?:mut\s+)?(?:ghostty_vt::)?Terminal\b")
CLASSES = ("page", "async", "in-worker", "dead")
# Where an async read is asked for: a socket, CLI, resource or attach request
# (p01-process risk 8 counts these), background daemon work, or a generic
# accessor whose callers are rows of their own.
ASYNC_PATHS = ("request", "background", "accessor")
# p01-process risk 8: above this many request-path async rows, phase B stays
# off in DEV and only phase A ships.
RISK8_REQUEST_ROWS = 20

ITEM_KEYWORDS = {
    "fn",
    "struct",
    "enum",
    "union",
    "trait",
    "impl",
    "mod",
    "const",
    "static",
    "type",
    "use",
    "macro_rules!",
}
MODIFIERS = {"pub", "unsafe", "async", "extern", "default", "const", "crate"}
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
RAW_STRING = re.compile(r'b?r(#*)"')
PATH = re.compile(r"[A-Za-z0-9_]+(?:::[A-Za-z0-9_]+)*")
NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z0-9_]+)*")
CHAR_LITERAL = re.compile(r"'(?:\\(?:u\{[0-9A-Fa-f]+\}|x[0-9A-Fa-f]{2}|.)|[^\\'])'")


class TableError(Exception):
    """The classification table cannot be read."""


# --- Rust lexing -----------------------------------------------------------


def mask_rust(text: str) -> tuple[str, list[str]]:
    """Returns the text with comments and literal contents blanked, and the
    kind ("code", "comment", "string") of every character.

    Blanking keeps newlines and offsets, so braces and item keywords can be
    read from the result without a comment or a string fooling them.
    """
    out = list(text)
    kind = ["code"] * len(text)
    i, n = 0, len(text)

    def blank(start: int, end: int, what: str) -> None:
        for k in range(start, end):
            kind[k] = what
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        c = text[i]
        if text.startswith("//", i):
            end = text.find("\n", i)
            end = n if end < 0 else end
            blank(i, end, "comment")
            i = end
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            blank(i, j, "comment")
            i = j
        elif (m := RAW_STRING.match(text, i)) and (
            i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")
        ):
            close = '"' + m.group(1)
            end = text.find(close, m.end())
            end = n if end < 0 else end + len(close)
            blank(m.end(), end - len(close), "string")
            i = end
        elif c == '"' or (c == "b" and text.startswith('b"', i)):
            j = i + (2 if c == "b" else 1)
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            blank(i + (2 if c == "b" else 1), j, "string")
            i = j + 1
        elif c == "'":
            # A char literal ('a', '\n', '\u{1F600}') or a lifetime ('a).
            m = CHAR_LITERAL.match(text, i)
            if m:
                blank(i + 1, m.end() - 1, "string")
                i = m.end()
            else:
                i += 1
        else:
            i += 1
    return "".join(out), kind


TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*!?|::|->|=>|\S")


@dataclass
class Frame:
    name: str | None
    start: int


def item_spans(code: str) -> list[tuple[int, int, str]]:
    """Returns (start, end, name) of every named item in masked code.

    An item runs from its keyword to its closing brace, or to its `;` when it
    has no body. Anonymous blocks (closures, `if`, `match`) are not items.
    """
    spans: list[tuple[int, int, str]] = []
    stack: list[Frame] = []
    pending: tuple[str, int] | None = None
    pending_depth = 0
    prev: str | None = None
    after_vis = False
    tokens = list(TOKEN.finditer(code))
    k = 0
    while k < len(tokens):
        tok = tokens[k].group(0)
        pos = tokens[k].start()
        item_position = prev in (None, "{", "}", ";", "]") or after_vis or prev in MODIFIERS
        if tok == "pub":
            after_vis = True
            prev = tok
            k += 1
            if k < len(tokens) and tokens[k].group(0) == "(":
                while k < len(tokens) and tokens[k].group(0) != ")":
                    k += 1
                k += 1
            continue
        if pending is None and item_position and (tok in ITEM_KEYWORDS):
            nxt = tokens[k + 1].group(0) if k + 1 < len(tokens) else ""
            if tok == "macro_rules!":
                pending = (f"macro_rules! {nxt}", pos)
            elif tok == "const" and nxt in ("fn", "unsafe", "async", "extern"):
                pass  # a `const fn`: the `fn` names it
            elif tok == "impl":
                j, depth, words = k + 1, 0, []
                while j < len(tokens):
                    t = tokens[j].group(0)
                    if depth == 0 and t in ("{", ";"):
                        break
                    if t in ("<", "("):
                        depth += 1
                    elif t in (">", ")"):
                        depth = max(0, depth - 1)
                    words.append(t)
                    j += 1
                pending = ("impl " + render_path(words), pos)
            elif tok == "use":
                pending = ("use", pos)
            elif IDENT.fullmatch(nxt) and nxt not in ("fn",):
                pending = (f"{tok} {nxt}", pos)
            pending_depth = 0
        elif tok in ("(", "["):
            pending_depth += 1
        elif tok in (")", "]"):
            pending_depth = max(0, pending_depth - 1)
        if tok == "{":
            if pending is not None and pending_depth == 0:
                stack.append(Frame(pending[0], pending[1]))
                pending = None
            else:
                stack.append(Frame(None, pos))
        elif tok == "}":
            if stack:
                frame = stack.pop()
                if frame.name is not None:
                    spans.append((frame.start, tokens[k].end(), frame.name))
        elif tok == ";" and pending is not None and pending_depth == 0:
            spans.append((pending[1], tokens[k].end(), pending[0]))
            pending = None
        if tok != "pub":
            after_vis = after_vis and tok in MODIFIERS
        prev = tok
        k += 1
    return spans


def render_path(words: list[str]) -> str:
    out = ""
    for w in words:
        if out and (w in ("::", "<", ">", ",", ")", "(", "&") or out.endswith(("::", "<", "(", "&"))):
            out += w if w != "," else ", "
        else:
            out += (" " if out else "") + w
    return out


# --- Sites -----------------------------------------------------------------


@dataclass
class Site:
    file: str
    item: str
    symbol: str
    q15: bool = True
    lines: list[int] = field(default_factory=list)
    matches: int = 0

    @property
    def key(self) -> tuple[str, str, str]:
        return (self.file, self.item, self.symbol)


def symbol_at(masked: str, raw: str, start: int, in_code: bool) -> str:
    """The path after `ghostty_vt::` at `start`, or "{A,B}" for a group."""
    src = masked if in_code else raw
    m = PATH.match(src, start)
    if m:
        return m.group(0)
    if start < len(src) and src[start] == "{":
        depth, j = 0, start
        while j < len(src):
            if src[j] == "{":
                depth += 1
            elif src[j] == "}":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        names = sorted(set(NAME.findall(src, start + 1, j)))
        return "{" + ",".join(names) + "}"
    return "?"


def accessor_names(masked: str) -> set[str]:
    """Functions in masked code whose signature hands a closure the parser."""
    return {
        m.group("name")
        for m in ACCESSOR_FN.finditer(masked)
        if TERMINAL_CLOSURE.search(m.group("sig"))
    }


def site_pattern(accessors: set[str]) -> re.Pattern[str]:
    lock = f"{TERM_LOCK}|{MODES_READ}"
    if accessors:
        names = "|".join(sorted(map(re.escape, accessors), key=lambda n: (-len(n), n)))
        lock += rf"|(?<![A-Za-z0-9_])(?:{names})\("
    return re.compile(f"{VT_NEEDLE}|(?P<lock>{lock})")


def scan_file(rel: str, text: str, lexed: tuple[str, list[str]], pattern: re.Pattern[str]) -> list[Site]:
    masked, kinds = lexed
    spans = item_spans(masked)
    line_starts = [0] + [i + 1 for i, ch in enumerate(text) if ch == "\n"]
    sites: dict[tuple[str, str], Site] = {}
    for m in pattern.finditer(text):
        pos = m.start()
        if not m.group("vt") and re.search(r"\bfn\s+$", text[max(0, pos - 8):pos]):
            continue  # the accessor's own definition, not a call
        kind = kinds[pos]
        enclosing = sorted((s for s in spans if s[0] <= pos < s[1]), key=lambda s: (s[0], -s[1]))
        item = " > ".join(s[2] for s in enclosing) or "(file)"
        if m.group("vt"):
            symbol = symbol_at(masked, text, m.end(), kind == "code")
        else:
            symbol = m.group("lock").lstrip(".") + ")"
        if kind != "code":
            symbol = f"{kind}:{symbol}"
        line = _line_of(line_starts, pos)
        site = sites.setdefault((item, symbol), Site(rel, item, symbol, q15=bool(m.group("vt"))))
        site.matches += 1
        if line not in site.lines:
            site.lines.append(line)
    return list(sites.values())


def _line_of(starts: list[int], pos: int) -> int:
    lo, hi = 0, len(starts) - 1
    while lo < hi:
        mid = (lo + hi + 1) // 2
        if starts[mid] <= pos:
            lo = mid
        else:
            hi = mid - 1
    return lo + 1


def in_scope(rel: str) -> bool:
    """The Q15 globs: `.rs`, no `tests` directory, not `*_tests.rs`."""
    parts = rel.split("/")
    return rel.endswith(".rs") and "tests" not in parts[:-1] and not parts[-1].endswith("_tests.rs")


def table_accessors(table: dict) -> set[str]:
    """The function names of the table's `accessor` rows."""
    names = set()
    for row in table.get("sites") or []:
        if row.get("path") == "accessor" and isinstance(row.get("item"), str):
            last = row["item"].split(" > ")[-1]
            if last.startswith("fn "):
                names.add(last[3:])
    return names


def scan(repo: str, table: dict) -> tuple[list[Site], set[str]]:
    """The sites of the tree and the accessor names whose calls are sites."""
    root = os.path.join(repo, CORE)
    if not os.path.isdir(root):
        raise TableError(f"{CORE} not found under {repo}")
    files: list[tuple[str, str, tuple[str, list[str]]]] = []
    accessors = set(ACCESSORS) | table_accessors(table)
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in sorted(filenames):
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, root).replace(os.sep, "/")
            if not in_scope(rel):
                continue
            with open(path, encoding="utf-8", errors="replace") as f:
                text = f.read()
            lexed = mask_rust(text)
            files.append((rel, text, lexed))
            accessors |= accessor_names(lexed[0])
    pattern = site_pattern(accessors)
    sites: list[Site] = []
    for rel, text, lexed in files:
        if pattern.search(text):
            sites.extend(scan_file(rel, text, lexed, pattern))
    return sites, accessors


# --- Table -----------------------------------------------------------------


def load_table(path: str) -> dict:
    try:
        with open(path, encoding="utf-8") as f:
            table = json.load(f)
    except (OSError, ValueError) as error:
        raise TableError(f"cannot read {path}: {error}") from error
    if not isinstance(table, dict) or not isinstance(table.get("sites"), list):
        raise TableError(f"{path}: expected an object with a `sites` list")
    return table


def validate_rows(table: dict) -> list[str]:
    problems: list[str] = []
    fields = table.get("page_fields") or {}
    seen: set[tuple[str, str, str]] = set()
    for index, row in enumerate(table["sites"]):
        where = f"row {index} ({row.get('file')} / {row.get('item')})"
        for key in ("file", "item", "class", "reason"):
            if not isinstance(row.get(key), str) or not row[key].strip():
                problems.append(f"{where}: missing `{key}`")
        symbols = row.get("symbols")
        if not isinstance(symbols, list) or not symbols or not all(isinstance(s, str) for s in symbols):
            problems.append(f"{where}: `symbols` must be a non-empty list of strings")
            symbols = []
        if row.get("class") not in CLASSES:
            problems.append(f"{where}: class {row.get('class')!r} is not one of {', '.join(CLASSES)}")
        if row.get("class") == "page":
            named = row.get("field")
            named = named if isinstance(named, list) else [named]
            if not named or any(not isinstance(name, str) or name not in fields for name in named):
                problems.append(f"{where}: a page row names its field; {row.get('field')!r} is not in page_fields")
        elif "field" in row:
            problems.append(f"{where}: only page rows name a field")
        if row.get("class") == "async":
            if row.get("path") not in ASYNC_PATHS:
                problems.append(f"{where}: an async row names its path, one of {', '.join(ASYNC_PATHS)}")
        elif "path" in row:
            problems.append(f"{where}: only async rows name a path")
        for symbol in symbols:
            key = (row.get("file"), row.get("item"), symbol)
            if key in seen:
                problems.append(f"{where}: symbol {symbol!r} is classified twice")
            seen.add(key)
    return problems


def classify(sites: list[Site], table: dict) -> tuple[dict, list[Site], list[str]]:
    by_key = {}
    for row in table["sites"]:
        for symbol in row.get("symbols") or []:
            by_key[(row.get("file"), row.get("item"), symbol)] = row
    classified: dict[tuple[str, str, str], dict] = {}
    unclassified: list[Site] = []
    for site in sites:
        row = by_key.get(site.key)
        if row is None:
            unclassified.append(site)
        else:
            classified[site.key] = row
    live = {site.key for site in sites}
    stale = [
        f"{file} / {item} / {symbol}"
        for (file, item, symbol) in by_key
        if (file, item, symbol) not in live
    ]
    return classified, unclassified, stale


def describe(row: dict) -> str:
    field = row.get("field")
    if isinstance(field, list):
        field = ",".join(field)
    extra = field or row.get("path")
    return row["class"] + (f" {extra}" if extra else "")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--repo", default=os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..")))
    parser.add_argument("--table")
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    table_path = args.table or os.path.join(args.repo, TABLE)
    try:
        table = load_table(table_path)
        sites, accessors = scan(args.repo, table)
    except TableError as error:
        print(f"vt-state-access: {error}", file=sys.stderr)
        return 2
    problems = validate_rows(table)
    classified, unclassified, stale = classify(sites, table)
    matches = sum(s.matches for s in sites)
    lines = len({(s.file, line) for s in sites for line in s.lines})

    if args.json:
        print(json.dumps([
            {
                "file": s.file,
                "item": s.item,
                "symbol": s.symbol,
                "lines": s.lines,
                "class": classified.get(s.key, {}).get("class"),
                "field": classified.get(s.key, {}).get("field"),
                "path": classified.get(s.key, {}).get("path"),
            }
            for s in sites
        ], indent=1))
    elif args.list:
        for s in sites:
            row = classified.get(s.key)
            label = describe(row) if row else "UNCLASSIFIED"
            print(f"{s.file}:{','.join(map(str, s.lines))}\t{s.item}\t{s.symbol}\t{label}")

    counts = Counter(row["class"] for row in classified.values())
    rows = Counter(row.get("class") for row in table["sites"])
    files = len({s.file for s in sites})
    q15 = [s for s in sites if s.q15]
    summary = ", ".join(f"{c} {counts.get(c, 0)}/{rows.get(c, 0)}" for c in CLASSES)
    requests = sum(1 for row in table["sites"] if row.get("class") == "async" and row.get("path") == "request")
    print(
        f"vt-state-access: {len(sites)} sites ({matches} matches on {lines} lines in {files} files; "
        f"Q15 `ghostty_vt::` {sum(s.matches for s in q15)} matches in {len({s.file for s in q15})} files); "
        f"sites/rows: {summary}; async rows on request paths {requests} "
        f"({'over' if requests > RISK8_REQUEST_ROWS else 'within'} the risk 8 limit {RISK8_REQUEST_ROWS}); "
        f"unclassified {len(unclassified)}; stale rows {len(stale)}; "
        f"accessors {', '.join(sorted(accessors)) or 'none'}",
        file=sys.stderr,
    )
    for s in unclassified:
        print(f"  unclassified: {s.file}:{','.join(map(str, s.lines))}  item={s.item!r}  symbol={s.symbol!r}", file=sys.stderr)
    for entry in stale:
        print(f"  stale row (no such site): {entry}", file=sys.stderr)
    for problem in problems:
        print(f"  bad row: {problem}", file=sys.stderr)
    if args.check and (unclassified or stale or problems):
        print(f"vt-state-access: add or fix rows in {TABLE}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

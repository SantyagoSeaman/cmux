#!/usr/bin/env bash
# vt-state-access.py (p01-process-208): every daemon reader of VT parser state
# has a row; a new site, a stale row and a page row without its field fail
# --check; keys survive moved lines; comments and strings do not fool the item
# parser; the Q15 exclusions hold; the repo's own table passes.
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "$0")/../../.." && pwd)"
CHECK="$ROOT_DIR/scripts/cmux-next/vt-state-access.py"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/cmux-vt-state-access.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
CORE="$tmp/repo/cmux-tui/crates/cmux-tui-core/src"
TABLE="$tmp/table.json"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# Runs --check on the fixture repo and table; returns its exit code.
check() {
  local code=0
  python3 -I "$CHECK" --repo "$tmp/repo" --table "$TABLE" --check >"$tmp/out" 2>"$tmp/err" || code=$?
  return "$code"
}

reset_fixture() {
  rm -rf "$tmp/repo"
  mkdir -p "$CORE/surface" "$CORE/tests" "$CORE/server"
  cat >"$CORE/surface/input.rs" <<'RS'
use super::*;
use ghostty_vt::{KeyEncoder,
    Mods};

impl Surface {
    /// Reads `ghostty_vt::Terminal` modes; "{" in a string is not a block.
    pub(crate) fn write_paste(&self) -> ghostty_vt::Result<()> {
        let label = "fn fake() { ghostty_vt::Nope }";
        let bracketed = {
            let term = self.term.lock().unwrap();
            term.mode(2004, false)
        };
        let _ = (label, bracketed, '{');
        Ok(())
    }
}

pub(crate) const LIMIT: usize = ghostty_vt::MAX_KITTY_IMAGES as usize;
RS
  cat >"$CORE/server/read.rs" <<'RS'
pub(super) fn read_screen(surface: &Surface) -> anyhow::Result<String> {
    Ok(surface.try_with_terminal(|t| t.viewport_text())??)
}
RS
  # Out of scope: a tests/ directory and a *_tests.rs file.
  printf 'use ghostty_vt::Terminal;\n' >"$CORE/tests/fixture.rs"
  printf 'use ghostty_vt::Terminal;\n' >"$CORE/server/read_tests.rs"
}

write_table() {
  # $1: extra JSON rows (with a leading comma) or empty.
  local extra="${1:-}"
  cat >"$TABLE" <<JSON
{
 "page_fields": {"modes.bracketed_paste": "DECSET 2004", "screen.cells": "visible rows"},
 "sites": [
  {"file": "surface/input.rs", "item": "use", "symbols": ["{KeyEncoder,Mods}"], "class": "page", "field": "modes.bracketed_paste", "reason": "r"},
  {"file": "surface/input.rs", "item": "impl Surface", "symbols": ["comment:Terminal"], "class": "dead", "reason": "doc comment"},
  {"file": "surface/input.rs", "item": "impl Surface > fn write_paste", "symbols": ["Result", "string:Nope", "term.lock()"], "class": "page", "field": ["modes.bracketed_paste"], "reason": "r"},
  {"file": "surface/input.rs", "item": "const LIMIT", "symbols": ["MAX_KITTY_IMAGES"], "class": "dead", "reason": "constant"},
  {"file": "server/read.rs", "item": "fn read_screen", "symbols": ["try_with_terminal()"], "class": "async", "path": "request", "reason": "r"}$extra
 ]
}
JSON
}

# 1. The full table passes; the exclusions keep tests/ and *_tests.rs out.
reset_fixture
write_table
check || { cat "$tmp/err" >&2; fail "a complete table must pass"; }
grep -q '7 sites (7 matches on 7 lines in 2 files' "$tmp/err" || { cat "$tmp/err" >&2; fail "expected 7 sites (strings, comments and exclusions)"; }
grep -q 'async rows on request paths 1' "$tmp/err" || { cat "$tmp/err" >&2; fail "request-path async rows are counted"; }

# 2. Moving lines keeps every key.
python3 -I - "$CORE/surface/input.rs" <<'PY'
import sys
path = sys.argv[1]
text = open(path).read()
open(path, "w").write("\n\n// moved\n\n" + text.replace("impl Surface {", "impl Surface {\n\n\n"))
PY
check || { cat "$tmp/err" >&2; fail "moved lines must keep their keys"; }

# 3. A new site is unclassified.
reset_fixture
printf '\nfn new_reader(t: &ghostty_vt::Terminal) -> bool { t.mode(1004, false) }\n' >>"$CORE/server/read.rs"
write_table
if check; then fail "a new site must fail --check"; fi
grep -q "unclassified: server/read.rs:.*item='fn new_reader'.*symbol='Terminal'" "$tmp/err" \
  || { cat "$tmp/err" >&2; fail "the new site is named"; }

# 4. A row whose site is gone is stale.
reset_fixture
write_table ',
  {"file": "server/read.rs", "item": "fn gone", "symbols": ["Terminal"], "class": "dead", "reason": "r"}'
if check; then fail "a stale row must fail --check"; fi
grep -q 'stale row (no such site): server/read.rs / fn gone / Terminal' "$tmp/err" || { cat "$tmp/err" >&2; fail "the stale row is named"; }

# 5. A page row must name a known field; an async row its path.
reset_fixture
write_table
python3 -I - "$TABLE" <<'PY'
import json, sys
table = json.load(open(sys.argv[1]))
table["sites"][3] = dict(table["sites"][3], **{"class": "page"})
del table["sites"][4]["path"]
json.dump(table, open(sys.argv[1], "w"))
PY
if check; then fail "a page row without a field must fail --check"; fi
grep -q 'a page row names its field' "$tmp/err" || { cat "$tmp/err" >&2; fail "the missing field is named"; }
grep -q 'an async row names its path' "$tmp/err" || { cat "$tmp/err" >&2; fail "the missing path is named"; }

# 6. A site classified twice is refused.
reset_fixture
write_table ',
  {"file": "server/read.rs", "item": "fn read_screen", "symbols": ["try_with_terminal()"], "class": "dead", "reason": "again"}'
if check; then fail "a duplicate row must fail --check"; fi
grep -q 'classified twice' "$tmp/err" || { cat "$tmp/err" >&2; fail "the duplicate is named"; }

# 7. The cold-slot lock and every call of an accessor found in the code (its
#    signature hands a closure the Terminal) are sites; its definition is not.
reset_fixture
cat >"$CORE/surface/cold.rs" <<'RS'
impl Surface {
    pub(crate) fn try_with_hot_terminal<R>(
        &self,
        f: impl FnOnce(&mut Terminal) -> R,
    ) -> Option<R> {
        let mut slot = self.term.slot.lock().unwrap();
        Some(f(&mut slot))
    }

    pub(crate) fn freeze(&self) -> bool {
        self.term.slot.try_lock().is_ok()
    }

    fn with_parser_update<R>(&self, update: impl FnOnce(&mut Terminal) -> R) -> R
    where
        R: Send,
    {
        update(&mut self.term.lock().unwrap())
    }
}
RS
cat >>"$CORE/server/read.rs" <<'RS'

fn write_pass(surface: &Surface) -> Option<u16> {
    surface.try_with_hot_terminal(|t| t.cols())
}

fn resize(surface: &Surface) {
    surface.with_parser_update(|t| t.resize(80, 24));
}
RS
write_table
if check; then fail "a cold-slot lock and an accessor call must be sites"; fi
grep -q "unclassified: surface/cold.rs:.*item='impl Surface > fn try_with_hot_terminal'.*symbol='term.slot.lock()'" "$tmp/err" \
  || { cat "$tmp/err" >&2; fail "the .term.slot.lock() site is named"; }
grep -q "unclassified: surface/cold.rs:.*item='impl Surface > fn freeze'.*symbol='term.slot.try_lock()'" "$tmp/err" \
  || { cat "$tmp/err" >&2; fail "the .term.slot.try_lock() site is named"; }
grep -q "unclassified: server/read.rs:.*item='fn write_pass'.*symbol='try_with_hot_terminal()'" "$tmp/err" \
  || { cat "$tmp/err" >&2; fail "the try_with_hot_terminal( call is named"; }
grep -q "unclassified: server/read.rs:.*item='fn resize'.*symbol='with_parser_update()'" "$tmp/err" \
  || { cat "$tmp/err" >&2; fail "a call of an accessor found from its signature is named"; }
grep -q 'accessors try_with_hot_terminal, try_with_terminal, with_parser_update, with_terminal' "$tmp/err" \
  || { cat "$tmp/err" >&2; fail "the accessors are listed"; }
write_table ',
  {"file": "surface/cold.rs", "item": "impl Surface > fn try_with_hot_terminal", "symbols": ["term.slot.lock()"], "class": "async", "path": "accessor", "reason": "r"},
  {"file": "surface/cold.rs", "item": "impl Surface > fn freeze", "symbols": ["term.slot.try_lock()"], "class": "in-worker", "reason": "r"},
  {"file": "surface/cold.rs", "item": "impl Surface > fn with_parser_update", "symbols": ["term.lock()"], "class": "in-worker", "reason": "r"},
  {"file": "server/read.rs", "item": "fn write_pass", "symbols": ["try_with_hot_terminal()"], "class": "async", "path": "background", "reason": "r"},
  {"file": "server/read.rs", "item": "fn resize", "symbols": ["with_parser_update()"], "class": "in-worker", "reason": "r"}'
check || { cat "$tmp/err" >&2; fail "classified cold-slot and accessor sites must pass (the definition is no site)"; }

# 8. The item of an `accessor` row names an accessor even when its signature
#    hands out no closure; its calls are sites.
reset_fixture
cat >"$CORE/surface/guard.rs" <<'RS'
impl Surface {
    pub(crate) fn parser_guard(&self) -> Guard<'_> {
        self.term.lock().unwrap()
    }
}
RS
cat >>"$CORE/server/read.rs" <<'RS'

fn cursor(surface: &Surface) -> (u16, u16) {
    surface.parser_guard().cursor_position()
}
RS
write_table ',
  {"file": "surface/guard.rs", "item": "impl Surface > fn parser_guard", "symbols": ["term.lock()"], "class": "async", "path": "accessor", "reason": "r"}'
if check; then fail "a call of a table accessor must be a site"; fi
grep -q "unclassified: server/read.rs:.*item='fn cursor'.*symbol='parser_guard()'" "$tmp/err" \
  || { cat "$tmp/err" >&2; fail "the table accessor call is named"; }

# 9. Request-path async rows are compared with the risk 8 limit.
reset_fixture
write_table
check || { cat "$tmp/err" >&2; fail "a complete table must pass"; }
grep -q 'async rows on request paths 1 (within the risk 8 limit 20)' "$tmp/err" \
  || { cat "$tmp/err" >&2; fail "the risk 8 limit is reported"; }

# 10. The repo's table classifies every site of the tree.
python3 -I "$CHECK" --repo "$ROOT_DIR" --check >/dev/null 2>"$tmp/err" \
  || { cat "$tmp/err" >&2; fail "plans/cmux-next/vt-worker-state-access.json must classify every site"; }

echo "vt-state-access: all tests passed"

#!/usr/bin/env bash
# bundle-cmux-tui.sh places the sandboxed VT parser beside bin/cmux as
# bin/cmux-vt-worker (p01-process-211), as it places cmux-holder: from beside
# the cmux-tui source or CMUX_NEXT_VT_WORKER_BIN, recorded in
# cmux-tui.version; a build without one removes a stale copy (the daemon then
# parses with its VT pool threads).
# No network, no daemon: a fake cmux-tui and CONFIGURATION=Release skip the
# capability check.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
sha() { shasum -a 256 "$1" | awk '{print $1}'; }

mkdir -p "$TMP/build/one" "$TMP/build/two" "$TMP/app"
printf '#!/bin/sh\necho "cmux-tui 0.1.0 (%s)"\n' "$(printf 'a%.0s' {1..40})" > "$TMP/build/one/cmux-tui"
cp "$TMP/build/one/cmux-tui" "$TMP/build/two/cmux-tui"
printf 'worker one\n' > "$TMP/build/one/cmux-vt-worker"
printf 'worker override\n' > "$TMP/override-worker"
chmod 755 "$TMP/build/one/cmux-tui" "$TMP/build/two/cmux-tui"

bin="$TMP/app/Contents/Resources/bin"
bundle() { # <tui binary> <mode> [extra env...]
  local tui="$1" mode="$2"; shift 2
  env -u CMUX_NEXT_APP_HOST_BIN -u CMUX_NEXT_CLOUD_SERVER_BIN -u CMUX_NEXT_BROWSER_HOST_BIN \
    -u CMUX_NEXT_AGENT_SCREEN_DETECTION_BIN -u CMUX_NEXT_TERMINAL_HOST_BIN -u CMUX_NEXT_HOLDER_BIN \
    -u CMUX_NEXT_VT_WORKER_BIN -u CMUX_TUI_CLIENT_LOCAL \
    TARGET_BUILD_DIR="$TMP/app" UNLOCALIZED_RESOURCES_FOLDER_PATH=Contents/Resources \
    SRCROOT="$ROOT" CONFIGURATION=Release CMUX_NEXT_TUI_MODE="$mode" CMUX_NEXT_TUI_BIN="$tui" \
    "$@" bash "$ROOT/scripts/cmux-next/bundle-cmux-tui.sh" >"$TMP/out" 2>&1 \
    || { cat "$TMP/out" >&2; fail "bundle-cmux-tui.sh failed ($mode, $tui)"; }
}

# Tree mode: the worker beside the cmux-tui source is bundled and recorded.
bundle "$TMP/build/one/cmux-tui" tree
[[ -x "$bin/cmux-vt-worker" ]] || fail "tree mode did not bundle bin/cmux-vt-worker"
cmp -s "$bin/cmux-vt-worker" "$TMP/build/one/cmux-vt-worker" || fail "bundled worker is not the one beside cmux-tui"
grep -qx "vt_worker_sha256=$(sha "$TMP/build/one/cmux-vt-worker")" "$bin/cmux-tui.version" \
  || fail "cmux-tui.version does not record the worker sha256: $(cat "$bin/cmux-tui.version")"

# Its own override wins over the sibling.
bundle "$TMP/build/one/cmux-tui" tree CMUX_NEXT_VT_WORKER_BIN="$TMP/override-worker"
cmp -s "$bin/cmux-vt-worker" "$TMP/override-worker" || fail "CMUX_NEXT_VT_WORKER_BIN was not bundled"

# A build without a worker removes the stale copy: no worker of another build runs.
bundle "$TMP/build/two/cmux-tui" tree
[[ ! -e "$bin/cmux-vt-worker" ]] || fail "a build without a worker kept a stale bin/cmux-vt-worker"
grep -qx "vt_worker_sha256=" "$bin/cmux-tui.version" || fail "cmux-tui.version still records a worker"

# The fleet's client-local build (build-cmux-tui-client.sh) builds every
# companion a bundle script names, so it builds the worker.
grep -q 'cmux-vt-worker' "$ROOT/scripts/cmux-next/build-cmux-tui-client.sh" \
  || fail "build-cmux-tui-client.sh does not build cmux-vt-worker"
echo "bundle-cmux-tui-vt-worker: ok"

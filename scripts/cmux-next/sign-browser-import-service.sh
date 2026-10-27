#!/usr/bin/env bash
# The sandboxed browser import service (plans/cmux-next/rule-of-2.md, p10-stability-208):
#
#   Contents/XPCServices/cmux-browser-import.xpc   the Xcode target cmux-browser-import
#     (Packages/macOS/CmuxNext/Sources/CmuxNextBrowserImportService/Main/main.swift linking the
#     package product CmuxNextBrowserImportParsers, Bundle/Info.plist), embedded by the app
#     target's copy phase "Embed XPC Services".
#
# It is signed with Bundle/BrowserImportService.entitlements: the App Sandbox and read-only
# user-selected files, nothing else (no network, no other file access; it reads only the
# descriptors the app sends). The entitlements go on here, never through the target's
# CODE_SIGN_ENTITLEMENTS: reload.sh blanks that setting for every target of a Release build.
#
# Three modes:
#   sign-browser-import-service.sh                       Xcode phase "Sign browser import service":
#       signs the embedded service with the build's identity, or ad hoc (a DEV tag, or a build with
#       CODE_SIGNING_ALLOWED=NO, whose app reload.sh signs ad hoc after), so every build runs it sandboxed.
#   sign-browser-import-service.sh --sign <app> <identity>   Developer ID, hardened runtime and a
#       secure timestamp (CMUX_TIMESTAMP=none for a local signature); scripts/sign-cmux-bundle.sh runs
#       it before it signs the app (inside out).
#   sign-browser-import-service.sh --check <app>         fails unless the bundle carries the service,
#       signed with exactly the two entitlements above.
#
# Env: CMUX_CODESIGN_TOOL overrides codesign (tests).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ENTITLEMENTS="$ROOT/Packages/macOS/CmuxNext/Sources/CmuxNextBrowserImportService/Bundle/BrowserImportService.entitlements"
SERVICE="Contents/XPCServices/cmux-browser-import.xpc"
CODESIGN="${CMUX_CODESIGN_TOOL:-/usr/bin/codesign}"
EXPECTED_KEYS=$'com.apple.security.app-sandbox\ncom.apple.security.files.user-selected.read-only'

service_of() {
  local xpc="$1/$SERVICE"
  if [[ ! -d "$xpc" ]]; then
    echo "error: sign-browser-import-service: $1 has no $SERVICE" >&2
    return 1
  fi
  printf '%s\n' "$xpc"
}

# Signs $1 (the .xpc) with identity $2; "-" is ad hoc.
sign() {
  local xpc="$1" identity="$2" flags=(--force --entitlements "$ENTITLEMENTS" --sign "$identity")
  if [[ "$identity" != "-" ]]; then
    flags+=(--options runtime)
    if [[ "${CMUX_TIMESTAMP:-}" == "none" ]]; then flags+=(--timestamp=none); else flags+=(--timestamp); fi
  fi
  "$CODESIGN" "${flags[@]}" "$xpc"
  echo "sign-browser-import-service: signed $(basename "$xpc") ($identity)"
}

# The entitlement keys the service is signed with, one per line, sorted.
signed_keys() {
  "$CODESIGN" -d --entitlements - --xml "$1" 2>/dev/null |
    python3 -I -c 'import plistlib, sys; data = sys.stdin.buffer.read(); print("\n".join(sorted(plistlib.loads(data) if data.strip() else {})))'
}

check() {
  local xpc keys
  xpc="$(service_of "$1")"
  "$CODESIGN" --verify --strict "$xpc"
  keys="$(signed_keys "$xpc")"
  if [[ "$keys" != "$EXPECTED_KEYS" ]]; then
    printf 'error: sign-browser-import-service: %s is signed with entitlements\n%s\nwant exactly\n%s\n' "$SERVICE" "$keys" "$EXPECTED_KEYS" >&2
    return 1
  fi
  echo "sign-browser-import-service: $SERVICE carries only the sandbox and read-only selected files"
}

case "${1:-}" in
  --sign)
    sign "$(service_of "${2:?usage: --sign <app> <identity>}")" "${3:?usage: --sign <app> <identity>}"
    ;;
  --check)
    check "${2:?usage: --check <app>}"
    ;;
  "")
    app="${TARGET_BUILD_DIR:?}/${WRAPPER_NAME:?}"
    identity="-"
    if [[ "${CODE_SIGNING_ALLOWED:-YES}" != "NO" && -n "${EXPANDED_CODE_SIGN_IDENTITY:-}" ]]; then
      identity="$EXPANDED_CODE_SIGN_IDENTITY"
    fi
    CMUX_TIMESTAMP=none sign "$(service_of "$app")" "$identity"
    ;;
  *)
    echo "usage: $0 [--sign <app> <identity> | --check <app>]" >&2
    exit 2
    ;;
esac

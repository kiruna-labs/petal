#!/usr/bin/env bash
#
# Release guards:
# - issue #231/#88: published macOS app must contain both Apple Silicon and
#   Intel slices.
# - GitHub #102: updater endpoint/pubkey must match pinned production trust
#   anchors before publishing latest.json.
# - GitHub #915: the built app must carry the Apple Events automation
#   entitlement (com.apple.security.automation.apple-events), or the
#   shared-browser-window Open URL feature silently never works -- the
#   hardened runtime denies osascript's Apple Events with no prompt at all
#   when the entitlement is absent.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# The committed tauri.conf.json deliberately carries NO updater endpoint
# (open-source builds must not phone home). The official trust anchors live
# only in the release overlay, which `tauri build --config` layers on top.
TAURI_CONF="$ROOT/apps/desktop/src-tauri/tauri.release.conf.json"
TAURI_BASE_CONF="$ROOT/apps/desktop/src-tauri/tauri.conf.json"

# Trust anchors this gate requires the built app to carry. They default to
# Petal's own official values, so the official release recipe needs no extra
# env -- but a fork that correctly repoints its updater must be able to run
# this gate against ITS anchors instead of failing on ours.
EXPECTED_UPDATER_ENDPOINT="${PETAL_EXPECTED_UPDATER_ENDPOINT:-https://app.petal.live/api/updater}"
EXPECTED_UPDATER_PUBKEY="${PETAL_EXPECTED_UPDATER_PUBKEY:-dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDJGNEFGMDUyNUMwRjBDQkQKUldTOURBOWNVdkJLTDJVOE9aT1RDSGRVMWZrS25tU1dZVXlDbzBGMjJmQUs5ZGgvajBuYUZ4a2gK}"

usage() {
  echo "usage: $0 /path/to/Petal.app" >&2
  echo "       $0 --updater-config-only" >&2
  echo "       $0 --check-entitlements-only /path/to/Petal.app" >&2
}

verify_updater_config() {
  [ -f "$TAURI_CONF" ] || {
    echo "updater config gate: tauri config not found: $TAURI_CONF" >&2
    exit 66
  }
  command -v node >/dev/null 2>&1 || {
    echo "updater config gate: node is required to parse $TAURI_CONF" >&2
    exit 69
  }

  TAURI_CONF="$TAURI_CONF" \
    TAURI_BASE_CONF="$TAURI_BASE_CONF" \
    EXPECTED_UPDATER_ENDPOINT="$EXPECTED_UPDATER_ENDPOINT" \
    EXPECTED_UPDATER_PUBKEY="$EXPECTED_UPDATER_PUBKEY" \
    node <<'NODE'
const fs = require('node:fs');

const confPath = process.env.TAURI_CONF;
const baseConfPath = process.env.TAURI_BASE_CONF;
const expectedEndpoint = process.env.EXPECTED_UPDATER_ENDPOINT;
const expectedPubkey = process.env.EXPECTED_UPDATER_PUBKEY;

function fail(message) {
  console.error(`updater config gate: ${message}`);
  process.exit(65);
}

let config;
try {
  config = JSON.parse(fs.readFileSync(confPath, 'utf8'));
} catch (error) {
  fail(`could not parse ${confPath}: ${error.message}`);
}

// The committed base config must stay endpoint-free: an OSS build from a
// plain clone must never poll an update feed. Catch a regression that
// re-adds the production anchors to tauri.conf.json.
let baseConfig;
try {
  baseConfig = JSON.parse(fs.readFileSync(baseConfPath, 'utf8'));
} catch (error) {
  fail(`could not parse ${baseConfPath}: ${error.message}`);
}
const baseEndpoints = baseConfig?.plugins?.updater?.endpoints;
if (!Array.isArray(baseEndpoints) || baseEndpoints.length !== 0) {
  fail(
    `${baseConfPath} must ship plugins.updater.endpoints = [] (open-source builds must not ` +
      `phone home); got ${JSON.stringify(baseEndpoints)}`
  );
}

const updater = config?.plugins?.updater;
if (!updater || typeof updater !== 'object') {
  fail('plugins.updater is missing');
}

const endpoints = updater.endpoints;
if (!Array.isArray(endpoints)) {
  fail('plugins.updater.endpoints must be an array');
}
if (endpoints.length !== 1 || endpoints[0] !== expectedEndpoint) {
  fail(
    `expected endpoints exactly ${JSON.stringify([expectedEndpoint])}; ` +
      `got ${JSON.stringify(endpoints)}`
  );
}

if (updater.pubkey !== expectedPubkey) {
  fail('plugins.updater.pubkey does not match the pinned release key');
}

console.log(`updater config gate: OK (${expectedEndpoint})`);
NODE
}

# GitHub #915: verify the built app carries the Apple Events automation
# entitlement, set to true. `codesign -d --entitlements :-` prints the
# app's entitlements plist to stdout (":-" means "to stdout", not a file).
# Factored into its own function, callable standalone via
# `--check-entitlements-only`, so it can be exercised against a throwaway
# ad-hoc-signed bundle without needing a full universal Petal.app.
check_entitlements() {
  local app="$1"
  local entitlements
  if ! entitlements="$(codesign -d --entitlements :- "$app" 2>/dev/null)"; then
    echo "entitlements gate (#915): codesign could not read entitlements for $app" >&2
    exit 65
  fi
  if ! grep -q 'com\.apple\.security\.automation\.apple-events' <<<"$entitlements"; then
    echo "entitlements gate (#915): $app is missing com.apple.security.automation.apple-events" >&2
    echo "  -- the shared-browser-window Open URL feature (#915) needs Apple Events to read" >&2
    echo "     a shared browser window's URL; add it to Entitlements.plist and rebuild." >&2
    exit 65
  fi
  if ! grep -A1 'com\.apple\.security\.automation\.apple-events' <<<"$entitlements" | grep -q '<true/>'; then
    echo "entitlements gate (#915): $app carries com.apple.security.automation.apple-events" >&2
    echo "  but it is not set to true; add it to Entitlements.plist and rebuild." >&2
    exit 65
  fi
  echo "entitlements gate (#915): OK (com.apple.security.automation.apple-events = true)"
}

if [ "$#" -eq 1 ] && [ "$1" = "--updater-config-only" ]; then
  verify_updater_config
  exit 0
fi

if [ "$#" -eq 2 ] && [ "$1" = "--check-entitlements-only" ]; then
  check_entitlements "$2"
  exit 0
fi

if [ "$#" -ne 1 ]; then
  usage
  exit 64
fi

APP="$1"
BIN="$APP/Contents/MacOS/desktop"

verify_updater_config

if [ ! -x "$BIN" ]; then
  echo "universal gate: executable not found: $BIN" >&2
  exit 66
fi

ARCHS="$(lipo -archs "$BIN" 2>/dev/null || true)"
if [ -z "$ARCHS" ]; then
  echo "universal gate: lipo could not read architectures for $BIN" >&2
  exit 65
fi

has_x86_64=0
has_arm64=0
case " $ARCHS " in *" x86_64 "*) has_x86_64=1 ;; esac
case " $ARCHS " in *" arm64 "*) has_arm64=1 ;; esac

if [ "$has_x86_64" -eq 1 ] && [ "$has_arm64" -eq 1 ]; then
  echo "universal gate: OK ($ARCHS)"
else
  echo "universal gate: expected both x86_64 and arm64 in $BIN; got: $ARCHS" >&2
  exit 65
fi

check_entitlements "$APP"


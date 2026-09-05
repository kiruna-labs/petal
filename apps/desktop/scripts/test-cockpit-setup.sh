#!/usr/bin/env bash
# Non-GUI regression for the two fixed cockpit identities. It exercises the
# confirmed path in an isolated HOME and makes sure no arbitrary marker-path
# input is accepted.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
TMP="$(mktemp -d -t petal-cockpit-setup-test)"
trap 'rm -rf "$TMP"' EXIT

HOME="$TMP/home" PETAL_COCKPIT_SETUP_CONFIRMED=1 "$SCRIPT_DIR/cockpit-setup.sh" >"$TMP/output" 2>&1

ROOT="$TMP/home/Library/Application Support"
PRIMARY="$ROOT/com.petal.app/.cockpit-setup-complete"
PEER="$ROOT/com.petal.app.testpeer/.cockpit-setup-complete"
[[ -f "$PRIMARY" ]] || { echo "cockpit setup test: primary marker missing" >&2; exit 1; }
[[ -f "$PEER" ]] || { echo "cockpit setup test: test-peer marker missing" >&2; exit 1; }
grep -Fq "marker written for com.petal.app: $PRIMARY" "$TMP/output"
grep -Fq "marker written for com.petal.app.testpeer: $PEER" "$TMP/output"

# The helper has no generic marker/path override. Supplying a plausible-looking
# one must not create a path outside the two fixed application-support dirs.
INJECTED="$TMP/injected-marker"
HOME="$TMP/other-home" PETAL_COCKPIT_SETUP_CONFIRMED=1 PETAL_COCKPIT_MARKER_PATH="$INJECTED" \
  "$SCRIPT_DIR/cockpit-setup.sh" >/dev/null
[[ ! -e "$INJECTED" ]] || { echo "cockpit setup test: arbitrary marker path was accepted" >&2; exit 1; }
[[ -f "$TMP/other-home/Library/Application Support/com.petal.app/.cockpit-setup-complete" ]]
[[ -f "$TMP/other-home/Library/Application Support/com.petal.app.testpeer/.cockpit-setup-complete" ]]

# The canonical root helper performs the actual TCC checks before writing the
# same two fixed markers. Keep its final writer on the same allowlist without
# invoking its interactive/build steps in this non-GUI regression.
bash -n "$REPO_ROOT/scripts/cockpit-setup.sh"
grep -Fq 'MARKER_IDENTIFIERS=("com.petal.app" "com.petal.app.testpeer")' "$REPO_ROOT/scripts/cockpit-setup.sh"
grep -Fq 'marker="$MARKER_ROOT/$identifier/.cockpit-setup-complete"' "$REPO_ROOT/scripts/cockpit-setup.sh"
if grep -Eq 'COCKPIT_SETUP_.*(MARKER|IDENTIFIER).*=' "$REPO_ROOT/scripts/cockpit-setup.sh"; then
  echo "cockpit setup test: canonical helper accepts a marker or identifier override" >&2
  exit 1
fi

echo "cockpit setup test: both fixed markers written; arbitrary marker path ignored; canonical helper aligned"

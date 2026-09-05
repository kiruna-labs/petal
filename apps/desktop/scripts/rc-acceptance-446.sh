#!/bin/bash
# Bring up the one-Mac two-peer loop and run the #446 acceptance suite.
#
# Deliberately a sibling of scripts/rc-live-suite.sh rather than a flag on it:
# this run must not inherit the 29-case TextEdit matrix's setup, and it needs
# its own isolated ports so it can be run while another agent owns the
# defaults. Every port is overridable; every process it starts is recorded in
# $RUN_DIR/pids so the caller can clean up exactly what it created.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
RUN_DIR="${PETAL_ACCEPTANCE_RUN_DIR:-/tmp/petal-acceptance-446}"
LK_PORT="${PETAL_ACCEPTANCE_LK_PORT:-7880}"
VITE_PORT="${PETAL_ACCEPTANCE_VITE_PORT:-5185}"
CDP_PORT="${PETAL_ACCEPTANCE_CDP_PORT:-9222}"
SOCK="$RUN_DIR/petal-rc.sock"
mkdir -p "$RUN_DIR"
: > "$RUN_DIR/pids"

note() { echo "[acceptance-446] $*"; }
track() { echo "$1" >> "$RUN_DIR/pids"; }

note "run dir: $RUN_DIR (livekit :$LK_PORT, vite :$VITE_PORT, cdp :$CDP_PORT)"

note "1. livekit-server"
if ! lsof -iTCP:$LK_PORT -sTCP:LISTEN >/dev/null 2>&1; then
  nohup livekit-server --dev --bind 0.0.0.0 >"$RUN_DIR/livekit.log" 2>&1 &
  track $!
  sleep 3
else
  note "   port $LK_PORT already listening; reusing"
fi

note "2. web-harness vite :$VITE_PORT"
if ! lsof -iTCP:$VITE_PORT -sTCP:LISTEN >/dev/null 2>&1; then
  ( cd "$REPO/web-harness" && nohup npx vite --port $VITE_PORT --strictPort >"$RUN_DIR/webharness.log" 2>&1 & echo $! >> "$RUN_DIR/pids" )
  sleep 5
fi

note "3. Chrome + CDP :$CDP_PORT"
if ! lsof -iTCP:$CDP_PORT -sTCP:LISTEN >/dev/null 2>&1; then
  rm -rf "$RUN_DIR/chrome-profile"
  nohup "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
    --remote-debugging-port=$CDP_PORT --user-data-dir="$RUN_DIR/chrome-profile" \
    --no-first-run --no-default-browser-check --window-size=900,700 --window-position=1200,900 \
    "http://localhost:$VITE_PORT/" >"$RUN_DIR/chrome.log" 2>&1 &
  track $!
  sleep 6
fi

note "4. Petal dev + autotest socket"
rm -f "$SOCK"
# PETAL_BACKEND_URL= (empty) forces the local dev-mint fallback; see
# scripts/rc-live-suite.sh for the full rationale.
( cd "$REPO/apps/desktop" && PETAL_BACKEND_URL= \
  LIVEKIT_URL=ws://localhost:$LK_PORT LIVEKIT_API_KEY=devkey LIVEKIT_API_SECRET=secret \
  PETAL_DISABLE_AUDIO=1 PETAL_AUTOTEST_ROOM=rctest PETAL_AUTOTEST_IDENTITY=native-autotest \
  PETAL_AUTOTEST_SOCK="$SOCK" \
  nohup npm run tauri dev >"$RUN_DIR/petal-dev.log" 2>&1 & echo $! >> "$RUN_DIR/pids" )

note "5. waiting for the autotest socket (this includes the cargo build)"
JOINED=""
for i in $(seq 1 "${PETAL_ACCEPTANCE_BOOT_TIMEOUT:-2400}"); do
  if grep -q "join_room('rctest') failed" "$RUN_DIR/petal-dev.log" 2>/dev/null; then
    note "FATAL: native join failed"
    grep "join_room('rctest') failed" "$RUN_DIR/petal-dev.log"
    exit 1
  fi
  if [ -S "$SOCK" ]; then JOINED=1; sleep 3; break; fi
  sleep 1
done
if [ -z "$JOINED" ]; then
  note "FATAL: Petal never opened the autotest socket -- see $RUN_DIR/petal-dev.log"
  exit 1
fi

note "6. running the #446 acceptance suite"
cd "$REPO/apps/desktop"
PETAL_AUTOTEST_SOCK="$SOCK" \
PETAL_WEB_HARNESS_URL_MATCH="localhost:$VITE_PORT" \
PETAL_REMOTE_CONTROL_CDP_JSON="http://127.0.0.1:$CDP_PORT/json" \
  node scripts/remote-control-local-loopback.mjs --live --acceptance-446 --skip-preflight \
    --json "$RUN_DIR/acceptance-446.json"
STATUS=$?
note "done (exit $STATUS). report: $RUN_DIR/acceptance-446.json"
exit $STATUS

#!/bin/bash
# Bring up the one-Mac two-peer loop for the #416 receiver-resize-race
# acceptance run. Sibling of rc-acceptance-446.sh (same shape, different
# scenario and its OWN default ports, so it can run while another agent owns
# 7880/5185/9222). Every process it starts is recorded in $RUN_DIR/pids.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
RUN_DIR="${PETAL_ACCEPTANCE_RUN_DIR:-/tmp/petal-acceptance-416}"
LK_PORT="${PETAL_ACCEPTANCE_LK_PORT:-7941}"
VITE_PORT="${PETAL_ACCEPTANCE_VITE_PORT:-5191}"
CDP_PORT="${PETAL_ACCEPTANCE_CDP_PORT:-9231}"
ROOM="${PETAL_ACCEPTANCE_ROOM:-rctest416}"
SOCK="$RUN_DIR/petal-416.sock"
mkdir -p "$RUN_DIR"
: > "$RUN_DIR/pids"

note() { echo "[acceptance-416] $*"; }
track() { echo "$1" >> "$RUN_DIR/pids"; }

note "run dir: $RUN_DIR (livekit :$LK_PORT, vite :$VITE_PORT, cdp :$CDP_PORT, room $ROOM)"

cat > "$REPO/apps/desktop/.env" <<EOF
LIVEKIT_URL=ws://localhost:$LK_PORT
LIVEKIT_API_KEY=devkey
LIVEKIT_API_SECRET=secretsecretsecretsecretsecretsecret
EOF

note "1. livekit-server :$LK_PORT"
if ! lsof -iTCP:$LK_PORT -sTCP:LISTEN >/dev/null 2>&1; then
  cat > "$RUN_DIR/lk.yaml" <<EOF
port: $LK_PORT
rtc:
  tcp_port: $((LK_PORT + 1))
  port_range_start: 53000
  port_range_end: 53400
  use_external_ip: false
keys:
  devkey: secretsecretsecretsecretsecretsecret
EOF
  nohup livekit-server --config "$RUN_DIR/lk.yaml" >"$RUN_DIR/livekit.log" 2>&1 &
  track $!
  sleep 3
else
  note "   port $LK_PORT already listening; reusing"
fi

note "2. web-harness vite :$VITE_PORT"
if ! lsof -iTCP:$VITE_PORT -sTCP:LISTEN >/dev/null 2>&1; then
  ( cd "$REPO/web-harness" && nohup npx vite --port $VITE_PORT --strictPort >"$RUN_DIR/webharness.log" 2>&1 & echo $! >> "$RUN_DIR/pids" )
  sleep 6
fi

note "3. Chrome + CDP :$CDP_PORT"
# The browser share renders via requestAnimationFrame, which Chrome pauses for
# occluded/background tabs; a stalled share makes the receiver's 30s frozen-
# window watchdog RETIRE the panel mid-run. The three throttling flags are what
# make it safe to raise the Petal panel over this window (docs/TESTING.md).
if ! lsof -iTCP:$CDP_PORT -sTCP:LISTEN >/dev/null 2>&1; then
  rm -rf "$RUN_DIR/chrome-profile"
  nohup "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
    --remote-debugging-port=$CDP_PORT --user-data-dir="$RUN_DIR/chrome-profile" \
    --no-first-run --no-default-browser-check \
    --disable-backgrounding-occluded-windows --disable-background-timer-throttling \
    --disable-renderer-backgrounding \
    --window-size=760,620 --window-position=40,1100 \
    "http://localhost:$VITE_PORT/" >"$RUN_DIR/chrome.log" 2>&1 &
  track $!
  sleep 6
fi

note "4. Petal (prebuilt dev binary) + autotest socket"
rm -f "$SOCK"
( cd "$REPO/apps/desktop/src-tauri" && PETAL_BACKEND_URL= \
  LIVEKIT_URL=ws://localhost:$LK_PORT LIVEKIT_API_KEY=devkey \
  LIVEKIT_API_SECRET=secretsecretsecretsecretsecretsecret \
  PETAL_DISABLE_AUDIO=1 PETAL_AUTOTEST_ROOM="$ROOM" \
  PETAL_AUTOTEST_IDENTITY="p-acceptance-416-$$" \
  PETAL_AUTOTEST_NAME="Acceptance416" \
  PETAL_AUTOTEST_SOCK="$SOCK" \
  PETAL_TRACE_PANEL_GEOMETRY="${PETAL_TRACE_PANEL_GEOMETRY:-}" \
  nohup ./target/debug/desktop >"$RUN_DIR/petal.log" 2>&1 & echo $! >> "$RUN_DIR/pids" )

note "5. waiting for the autotest socket"
JOINED=""
for _ in $(seq 1 "${PETAL_ACCEPTANCE_BOOT_TIMEOUT:-180}"); do
  if [ -S "$SOCK" ]; then JOINED=1; sleep 4; break; fi
  sleep 1
done
if [ -z "$JOINED" ]; then
  note "FATAL: Petal never opened the autotest socket -- see $RUN_DIR/petal.log"
  exit 1
fi
note "ready. room=$ROOM  cdp=http://127.0.0.1:$CDP_PORT/json  vite=localhost:$VITE_PORT"

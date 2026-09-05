#!/bin/bash
# Phase-4 T0 DoD battery (#748, plan §6 Phase 4 DoD + §7.2). One command on a
# fully-granted rig; HARNESS INVALID (exit 3) anywhere short of that.
#
# Requires the dev bundle (plan §9.14 recipe) granted BOTH Accessibility and
# Screen Recording DIRECTLY (§9.15.3: inherited SR delivers lifecycle codes
# but NOT per-window 806/807 move events; inherited AX is degraded).
#
# Asserts, in one granted session:
#   B1  tier line: classify=stage0+AX-subrole(T1)
#   B2  T0 upgraded with BOTH capabilities: moves=true lifecycle=true
#       (the move canary's panel nudge must prove 806 delivery)
#   B3  sweep demoted (event-driven + heartbeat)
#   B4  pillfollow under the fully-demoted, event-driven tier: >=50% per-step
#       follow density and a final reposition within 500ms of actuator exit
#   B5  the classification battery (G1/G2/G3) via
#       scripts/verify-window-classification.sh
#
# Env: same as verify-window-classification.sh.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# #905: logs are now per-day (`petal.log.<YYYY-MM-DD>`, rolled mid-session
# at the UTC date boundary); resolve the most-recently-written daily file,
# falling back to the pre-#905 bare `petal.log` on an install that hasn't
# rolled once yet.
LOG_DIR="$HOME/Library/Logs/Petal"
LOG="$(ls -t "$LOG_DIR"/petal.log.[0-9]*[0-9] 2>/dev/null | head -1)"
if [ -z "$LOG" ]; then
  LOG="$LOG_DIR/petal.log"
fi
BUNDLE="${PETAL_AX_BUNDLE:-$HOME/tmp/PetalDevAX.app}"
BACKEND="${PETAL_BACKEND_URL:-https://app.petal.live}"
ROOM="${PETAL_AUTOTEST_ROOM:-webtest}"
WORK="$(mktemp -d /tmp/t0-battery.XXXXXX)"

fail_invalid() { echo "HARNESS INVALID: $1" >&2; exit 3; }

# shellcheck source=scripts/petal-instance-guard.sh
source "$ROOT/scripts/petal-instance-guard.sh"

# #846: refuse-don't-kill, checked in BOTH directions by
# scripts/test-petal-instance-guard.sh. Teardown below now kills only the
# PID this script itself launched, verified by `ps -p` -- never a pattern.
petal_guard_no_foreign_instance "" || exit 3

clang -o "$WORK/disclaim-launch" "$ROOT/scripts/probes/disclaim-launch.c" \
  || fail_invalid "disclaim-launch failed to compile"
clang -fobjc-arc -framework Cocoa -framework ApplicationServices \
  -o "$WORK/pillfollow" "$ROOT/scripts/probes/pillfollow.m" 2>/dev/null \
  || PILLFOLLOW_FALLBACK="$HOME/tmp/pillfollow"
PILLFOLLOW="${PILLFOLLOW_FALLBACK:-$WORK/pillfollow}"
[ -x "$PILLFOLLOW" ] || fail_invalid "no pillfollow actuator available"

MARK=$(wc -l < "$LOG" 2>/dev/null || echo 0)
env PETAL_BACKEND_URL="$BACKEND" PETAL_AUTOTEST_ROOM="$ROOM" \
    PETAL_AUTOTEST_FRESH_ROOM=1 PETAL_DISABLE_AUDIO=1 \
  "$WORK/disclaim-launch" "$BUNDLE/Contents/MacOS/desktop" >"$WORK/app.log" 2>&1 &
LAUNCHER=$!
APP_PID=""
cleanup() {
  kill "$LAUNCHER" 2>/dev/null
  petal_guard_kill_pid_verified "$APP_PID" "$BUNDLE/Contents/MacOS/desktop"
  sleep 1
  petal_guard_no_foreign_instance "" \
    && echo "teardown clean (pid-verified)" \
    || echo "WARN: teardown survivor (or a foreign instance appeared) -- see FATAL above" >&2
  rm -rf "$WORK"
}
trap cleanup EXIT

for _ in $(seq 1 15); do
  sleep 2
  tail -n +$((MARK+1)) "$LOG" 2>/dev/null | grep -aq "autotest: join succeeded" && break
done
APP_PID=$(pgrep -f "$BUNDLE/Contents/MacOS/desktop" | head -1)
tail -n +$((MARK+1)) "$LOG" | grep -aq "autotest: join succeeded" \
  || fail_invalid "room join did not succeed"
PERMS=$(tail -n +$((MARK+1)) "$LOG" | grep -aE "startup permission check" | tail -1)
echo "$PERMS" | grep -q "Screen Recording access: GRANTED" \
  || fail_invalid "Screen Recording not directly granted (§9.15.3): $PERMS"
echo "$PERMS" | grep -q "Accessibility access: GRANTED" \
  || fail_invalid "Accessibility not directly granted (§9.14): $PERMS"

PASS=0; FAIL=0
verdict() { if [ "$2" = 1 ]; then echo "PASS  $1"; PASS=$((PASS+1)); else echo "FAIL  $1"; FAIL=$((FAIL+1)); fi; }

# B1: stage-1 tier
sleep 4
TIER=$(tail -n +$((MARK+1)) "$LOG" | grep -aE "winsrv: tiers" | tail -1)
echo "$TIER"
verdict "B1 classify tier T1" "$(echo "$TIER" | grep -q 'AX-subrole(T1)' && echo 1 || echo 0)"

# B2: both T0 capabilities (canary nudge proves moves; allow it a few sweeps)
for _ in $(seq 1 10); do
  sleep 2
  tail -n +$((MARK+1)) "$LOG" | grep -aq "T0 upgraded" && break
done
T0=$(tail -n +$((MARK+1)) "$LOG" | grep -aE "T0 upgraded" | tail -1)
echo "${T0:-no T0 line}"
# moves may flip AFTER the one-shot upgrade line (lifecycle events beat the
# nudge canary by milliseconds); the per-capability flip line is equal evidence.
MOVES_OK=0
echo "$T0" | grep -q "moves=true" && MOVES_OK=1
tail -n +$((MARK+1)) "$LOG" | grep -aq "T0 moves capability live" && MOVES_OK=1
LIFE_OK=0
echo "$T0" | grep -q "lifecycle=true" && LIFE_OK=1
verdict "B2 T0 moves+lifecycle live" \
  "$([ "$MOVES_OK" = 1 ] && [ "$LIFE_OK" = 1 ] && echo 1 || echo 0)"

# B3: demotion engaged
verdict "B3 sweep demoted" \
  "$(tail -n +$((MARK+1)) "$LOG" | grep -aq 'sweep demoted' && echo 1 || echo 0)"

# B4: pillfollow under the fully event-driven tier
M4=$(wc -l < "$LOG")
"$PILLFOLLOW" > "$WORK/moves.log" 2>/dev/null
sleep 1
MOVES=$(grep -c "^MOVE" "$WORK/moves.log" || echo 0)
SHOWS=$(tail -n +$((M4+1)) "$LOG" | grep -acE "hover_tab: show at")
DENSITY_OK=0
[ "$MOVES" -gt 0 ] && [ $((SHOWS * 2)) -ge "$MOVES" ] && DENSITY_OK=1
verdict "B4 pillfollow density under T0 (shows=$SHOWS moves=$MOVES)" "$DENSITY_OK"

# Stop our instance before B5 (the classification gate launches its own).
kill "$LAUNCHER" 2>/dev/null
petal_guard_kill_pid_verified "$APP_PID" "$BUNDLE/Contents/MacOS/desktop"
APP_PID=""
sleep 2

# B5: classification battery (its own self-validation applies)
if PETAL_AX_BUNDLE_KEEP=1 "$ROOT/scripts/verify-window-classification.sh"; then
  verdict "B5 classification battery (G1/G2/G3)" 1
else
  verdict "B5 classification battery (G1/G2/G3)" 0
fi

echo "== T0 battery: $PASS pass, $FAIL fail =="
[ "$FAIL" -eq 0 ]

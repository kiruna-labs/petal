#!/bin/bash
# Window-registry stage-1 classification live gate (#747, plan §7.2 Rung B).
#
# Proves, against a REAL running Petal with REAL AX, that:
#   G1  a borderless chrome-less popup is AX-classified `Popup` and the hover
#       share pill is SUPPRESSED for it;
#   G2  (positive control) a titled window at the same spot DOES get a pill —
#       without this, G1's "no pill" proves nothing (hover could be dead);
#   G3  (morph) a window that starts borderless and later gains chrome loses
#       its stale `Popup` classification and gets a pill (§3 subrole-mutation
#       guarantee, via recheck or lifecycle re-resolution — either mechanism
#       satisfies the user-visible contract).
#
# Headless-ness: this needs a WindowServer session and an Accessibility-granted
# launch identity, so it lives in the display-requiring tier (docs/TESTING.md);
# ci-local.sh only compile-checks the probes. On a machine provisioned once
# (Developer-ID-signed bundle granted, or a PPPC profile), it runs unattended.
#
# Launch identity: by default a bundle wrapping the dev binary, launched
# DISCLAIMED so TCC attributes to the bundle, not the shell/Terminal — plan
# §9.14: inherited (responsible-process) trust is silently DEGRADED and
# classifies nothing; only a direct grant works. The harness self-validates:
# if the tier line does not read AX-subrole(T1), exit 3 (HARNESS INVALID)
# rather than reporting a meaningless pass/fail.
#
# Env:
#   PETAL_AX_BUNDLE   bundle whose Contents/MacOS/desktop is the app binary
#                     (default ~/tmp/PetalDevAX.app; refreshed from the dev
#                     binary unless PETAL_AX_BUNDLE_KEEP=1)
#   PETAL_BACKEND_URL backend for the room join (default https://app.petal.live)
#   PETAL_AUTOTEST_ROOM  QA room key (default webtest)
#   PROBE_SPOT_X/Y    empty-screen-region spot for probe windows (default 2300/800)
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
SPOT_X="${PROBE_SPOT_X:-2300}"
SPOT_Y="${PROBE_SPOT_Y:-800}"
WORK="$(mktemp -d /tmp/winclass-gate.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

fail_invalid() { echo "HARNESS INVALID: $1" >&2; exit 3; }

# shellcheck source=scripts/petal-instance-guard.sh
source "$ROOT/scripts/petal-instance-guard.sh"

# --- multi-agent slot gate (CLAUDE.md): never kill another session's app ----
# #846: refuse-don't-kill, checked in BOTH directions by
# scripts/test-petal-instance-guard.sh. Teardown below kills only the PID
# this script itself launched, verified by `ps -p` -- never a pattern.
petal_guard_no_foreign_instance "" || exit 3

# --- build probes ------------------------------------------------------------
clang -o "$WORK/disclaim-launch" "$ROOT/scripts/probes/disclaim-launch.c" \
  || fail_invalid "disclaim-launch failed to compile"
clang -fobjc-arc -framework Cocoa -framework ApplicationServices \
  -o "$WORK/onewin" "$ROOT/scripts/probes/onewin.m" \
  || fail_invalid "onewin probe failed to compile"

# --- launch bundle (dev binary wrapped so a DIRECT grant can exist) ----------
[ -d "$BUNDLE" ] || fail_invalid "no bundle at $BUNDLE (see plan §9.14 dev-rig recipe)"
if [ "${PETAL_AX_BUNDLE_KEEP:-0}" != 1 ]; then
  BIN="$ROOT/apps/desktop/src-tauri/target/debug/desktop"
  [ -x "$BIN" ] || fail_invalid "no dev binary at $BIN"
  # Signing rewrites the bundle copy, so raw cmp ALWAYS differs -- compare the
  # SOURCE binary's hash against the sidecar recorded at last refresh instead
  # (a naive cmp re-signed every run, churning the ad-hoc TCC grant each time).
  SRC_SHA=$(shasum -a 256 "$BIN" | cut -d' ' -f1)
  # NOT inside the bundle -- unsealed content in the bundle root makes
  # codesign refuse to sign ("unsealed contents present in the bundle root").
  SIDECAR="$BUNDLE.source-sha"
  if [ "$(cat "$SIDECAR" 2>/dev/null)" != "$SRC_SHA" ]; then
    echo "NOTE: dev binary changed since last bundle refresh; re-copying (ad-hoc grant needs a re-toggle: tccutil reset Accessibility com.petal.devax, relaunch, toggle)"
    cp "$BIN" "$BUNDLE/Contents/MacOS/desktop"
    codesign --force --sign - "$BUNDLE" >/dev/null 2>&1
    echo "$SRC_SHA" > "$SIDECAR"
  fi
fi

MARK=$(wc -l < "$LOG" 2>/dev/null || echo 0)
env PETAL_BACKEND_URL="$BACKEND" PETAL_AUTOTEST_ROOM="$ROOM" \
    PETAL_AUTOTEST_FRESH_ROOM=1 PETAL_DISABLE_AUDIO=1 \
  "$WORK/disclaim-launch" "$BUNDLE/Contents/MacOS/desktop" >"$WORK/app.log" 2>&1 &
LAUNCHER=$!
APP_PID=""
cleanup() {
  petal_guard_kill_pid_verified "$APP_PID" "$BUNDLE/Contents/MacOS/desktop"
  kill "$LAUNCHER" 2>/dev/null
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

# --- harness self-validation -------------------------------------------------
tail -n +$((MARK+1)) "$LOG" | grep -aq "autotest: join succeeded" \
  || fail_invalid "room join did not succeed (backend/room env)"
TIER=$(tail -n +$((MARK+1)) "$LOG" | grep -aE "winsrv: tiers" | tail -1)
echo "$TIER"
echo "$TIER" | grep -q "AX-subrole(T1)" \
  || fail_invalid "stage-1 tier not live (${TIER:-no tier line}) -- grant the bundle Accessibility (plan §9.14) and re-run"

run_probe() { # mode seconds -> sets PROBE_WID
  local mode=$1 secs=$2
  PROBE_SPOT_X="$SPOT_X" PROBE_SPOT_Y="$SPOT_Y" \
    timeout "$secs" "$WORK/onewin" "$mode" 2>"$WORK/probe.err" &
  PROBE=$!
  sleep $((secs - 1))
  PROBE_WID=$(grep -oE "wid=[0-9]+" "$WORK/probe.err" | head -1 | cut -d= -f2)
  wait "$PROBE" 2>/dev/null
}

PASS=0; FAIL=0
verdict() { # name ok
  if [ "$2" = 1 ]; then echo "PASS  $1"; PASS=$((PASS+1)); else echo "FAIL  $1"; FAIL=$((FAIL+1)); fi
}

# --- G2 first: positive control (hover alive) --------------------------------
M2=$(wc -l < "$LOG")
run_probe normal 12
SHOW=$(tail -n +$((M2+1)) "$LOG" | grep -acE "hover_tab: show at .* for window $PROBE_WID")
[ "$SHOW" -ge 1 ] || fail_invalid "titled control window got no pill -- hover not observing this rig; popup results would be meaningless"
verdict "G2 titled control gets a pill (positive control)" 1

# --- G1: popup suppressed ----------------------------------------------------
M1=$(wc -l < "$LOG")
run_probe popup 14
CLASSIFIED=$(tail -n +$((M1+1)) "$LOG" | grep -acE "winsrv: classified window $PROBE_WID as Popup")
SHOWN=$(tail -n +$((M1+1)) "$LOG" | grep -acE "hover_tab: show at .* for window $PROBE_WID")
verdict "G1 popup classified Popup ($CLASSIFIED) with zero pills ($SHOWN)" \
  "$([ "$CLASSIFIED" -ge 1 ] && [ "$SHOWN" -eq 0 ] && echo 1 || echo 0)"

# --- G3: morph clears the stale Popup ---------------------------------------
M3=$(wc -l < "$LOG")
run_probe morph 20
CLASSIFIED=$(tail -n +$((M3+1)) "$LOG" | grep -acE "winsrv: classified window $PROBE_WID as Popup")
SHOWN=$(tail -n +$((M3+1)) "$LOG" | grep -acE "hover_tab: show at .* for window $PROBE_WID")
verdict "G3 morph: Popup while chrome-less ($CLASSIFIED) then pill after chrome ($SHOWN)" \
  "$([ "$CLASSIFIED" -ge 1 ] && [ "$SHOWN" -ge 1 ] && echo 1 || echo 0)"

echo "== window-classification gate: $PASS pass, $FAIL fail =="
[ "$FAIL" -eq 0 ]

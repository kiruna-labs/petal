#!/bin/bash
# Run the remote-control live suite end to end on one Mac.
# Pass --press-to-photon to run the AppKit sentinel latency gate instead of the
# 29-case TextEdit matrix.
# Pass --input-only to run the SAME matrix with a video-independent
# share-readiness bar (plan 6c). Its results land in a distinct artifact so a
# relaxed run can never be cited as the full gate.
# Step 0 verifies per-window capture for real before the stack is brought up --
# see scripts/capture-preflight.sh for what the old check failed to check.
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Item 7: this script had NO trap and NO pid tracking at all, so an interrupted
# run left every service it had started behind. Cleanup is deliberately
# asymmetric: on an abnormal exit everything this run started is torn down; on a
# clean pass the services are left up (they are reuse-if-present by design --
# the pgrep/lsof guards below exist precisely so a second run reuses them) and
# their pids are printed so they can be killed by pid, never by pattern.
# shellcheck source=scripts/owned-process-cleanup.sh
source "$REPO/scripts/owned-process-cleanup.sh"
# shellcheck source=scripts/capture-preflight.sh
source "$REPO/scripts/capture-preflight.sh"

suite_exit_cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  if [ "$status" -ne 0 ]; then
    echo "== suite exited $status; releasing every process this run started =="
    release_owned_processes
  else
    echo "== suite passed; leaving reusable services up: pids ${OWNED_PIDS[*]-none} =="
  fi
  exit "$status"
}
trap suite_exit_cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP
# ---------------------------------------------------------------------------
# Foreign-instance guard. Step 4 runs `npm run dev:clean`, and scripts/dev.sh
# CANNOT tell a stale instance from another agent's live one -- it kills any
# running target/debug/desktop. Several agents work this repo in parallel, so a
# live GUI measurement destroyed mid-run is both lost work and a silently
# corrupted result for its owner, who will most likely blame their own change.
#
# CLAUDE.md is explicit that this check must live HERE and not in an agent's
# instructions: a coordination signal ("the slot is free") can be correct when
# given and wrong ten minutes later, and only a check at the moment of action
# catches that. Keep it even when you have been explicitly cleared to proceed.
#
# By PID, never by pattern: `pgrep -f` matches the watcher's OWN command line,
# which is how a self-matching check reports both false positives and false
# negatives (CLAUDE.md, "How to build & verify" rule 5).
assert_no_foreign_petal() {
  local self="$$" pids=() pid
  while read -r pid; do
    [ -z "$pid" ] && continue
    [ "$pid" = "$self" ] && continue
    kill -0 "$pid" 2>/dev/null || continue
    pids+=("$pid")
  done < <(pgrep -x desktop 2>/dev/null || true)
  [ "${#pids[@]}" -eq 0 ] && return 0
  echo "FATAL: a Petal dev binary is already running -- not mine. Refusing to start." >&2
  echo "       Step 4's dev:clean would KILL it, destroying another agent's live run." >&2
  for pid in "${pids[@]}"; do
    ps -p "$pid" -o pid=,etime=,command= 2>/dev/null | sed 's/^/       /' >&2
  done
  echo "       If it is genuinely yours and stale, kill it BY PID and re-run." >&2
  exit 3
}
if [ -n "${PETAL_RC_SUITE_SKIP_INSTANCE_GUARD:-}" ]; then
  echo "== WARNING: foreign-instance guard disabled by PETAL_RC_SUITE_SKIP_INSTANCE_GUARD =="
else
  assert_no_foreign_petal
fi

MODE_ARGS=()
RESULTS_JSON=/tmp/rc-results.json
MODE_LABEL="TextEdit matrix"
if [ "${1:-}" = "--press-to-photon" ]; then
  MODE_ARGS+=(--press-to-photon)
  RESULTS_JSON=/tmp/rc-photon.json
  MODE_LABEL="press-to-photon"
elif [ "${1:-}" = "--input-only" ]; then
  MODE_ARGS+=(--input-only)
  RESULTS_JSON=/tmp/rc-results-input-only.json
  MODE_LABEL="INPUT-ONLY -- video path NOT verified"
elif [ -n "${1:-}" ]; then
  echo "FATAL: unknown argument '$1' (expected --press-to-photon or --input-only)" >&2
  exit 2
fi
rm -f "$RESULTS_JSON"
cd "$REPO"
echo "== 0. verify per-window capture is healthy =="
# The artifact is removed FIRST and required afterwards. The old check never
# removed its probe file, so a file left by any earlier run satisfied it.
rm -f "$CAPTURE_PREFLIGHT_ARTIFACT"
PREFLIGHT_WINDOW_ID="${PREFLIGHT_WINDOW_ID:-}"
if [ -z "$PREFLIGHT_WINDOW_ID" ]; then
  PREFLIGHT_WINDOW_ID="$(capture_preflight_select_window \
    "$(xcrun swift "$REPO/apps/desktop/scripts/petal-window-probe.swift" --find 2>/dev/null || true)" || true)"
fi
if [ -z "$PREFLIGHT_WINDOW_ID" ]; then
  echo "FATAL: step 0 found no window to probe. Open a normal app window (>=400x300) and re-run," >&2
  echo "       or pass one explicitly: PREFLIGHT_WINDOW_ID=<id> scripts/rc-live-suite.sh ..." >&2
  exit 1
fi
echo "== 0. probing window $PREFLIGHT_WINDOW_ID with publish_probe --capture-preflight-only =="
( cd "$REPO/apps/desktop/src-tauri" \
  && cargo run --quiet --example publish_probe -- \
       "$PREFLIGHT_WINDOW_ID" --source real --capture-preflight-only ) \
  >"$CAPTURE_PREFLIGHT_ARTIFACT" 2>&1 || true
if ! evaluate_capture_preflight "$CAPTURE_PREFLIGHT_ARTIFACT"; then
  echo "FATAL: per-window capture is not healthy; refusing to spend the run. Evidence: $CAPTURE_PREFLIGHT_ARTIFACT" >&2
  exit 1
fi
echo "== 1. livekit-server =="
if ! pgrep -f "livekit-server --dev" >/dev/null; then
  # `( cmd & )` throws the real pid away -- `$!` in a subshell is the SUBSHELL.
  owned_spawn_group nohup livekit-server --dev >/tmp/livekit.log 2>&1
  sleep 2
fi
echo "== 2. web-harness dependencies + :5185 =="
if ! (cd "$REPO/web-harness" && npm ls --depth=0 >/dev/null 2>&1); then
  (cd "$REPO/web-harness" && npm ci)
fi
if ! lsof -iTCP:5185 -sTCP:LISTEN >/dev/null 2>&1; then
  owned_spawn_group bash -c 'cd "$1/web-harness" && exec npx vite --port 5185 --strictPort >/tmp/webharness.log 2>&1' _ "$REPO"
  sleep 4
fi
echo "== 3. Chrome + CDP :9222 =="
if ! lsof -iTCP:9222 -sTCP:LISTEN >/dev/null 2>&1; then
  rm -rf /tmp/petal-cdp-chrome
  owned_spawn_group "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --remote-debugging-port=9222 --user-data-dir=/tmp/petal-cdp-chrome --no-first-run --no-default-browser-check "http://localhost:5185/" >/tmp/chrome-cdp.log 2>&1
  sleep 5
fi
echo "== 4. Petal debug + autotest =="
rm -f /tmp/petal-rc.sock
# PETAL_BACKEND_URL= (empty, explicitly set) is load-bearing: apps/desktop/.env
# commonly pins a real backend URL for other manual testing, and dotenvy does
# not override an already-set process env var. Without this, Petal silently
# mints tokens against that remote backend instead of the local LIVEKIT_URL/
# KEY/SECRET dev-mint fallback -- which then rejects the fixed 'native-autotest'
# identity ("identity must be a generated participant id") since that check
# only applies remotely. An empty string is treated as unset by
# backend_base_url() (token.rs), so this forces the local fallback regardless
# of .env drift.
owned_spawn_group bash -c 'cd "$1/apps/desktop" && exec env PETAL_BACKEND_URL= LIVEKIT_URL=ws://localhost:7880 LIVEKIT_API_KEY=devkey LIVEKIT_API_SECRET=secret PETAL_ACCESSORY_UI=1 PETAL_DISABLE_AUDIO=1 PETAL_AUTOTEST_ROOM=rctest PETAL_AUTOTEST_FRESH_ROOM=1 PETAL_AUTOTEST_IDENTITY=native-autotest PETAL_AUTOTEST_SOCK=/tmp/petal-rc.sock npm run dev:clean >/tmp/petal-dev-rc.log 2>&1' _ "$REPO"
echo "== 5. wait for Petal join + socket =="
JOINED=""
# Raise-only env override (multi-session CLAUDE.md rule: hard-coded hang
# detectors get one the first time they bite): a cold cargo/tauri dev build
# can take far longer than 120s, and the timeout then impersonates a launch
# failure. Values below the default are ignored (units mistakes).
SOCKET_WAIT_S="${RC_SUITE_SOCKET_TIMEOUT_S:-120}"
[ "$SOCKET_WAIT_S" -lt 120 ] 2>/dev/null && SOCKET_WAIT_S=120
for i in $(seq 1 "$SOCKET_WAIT_S"); do
  if grep -q "join_room('rctest') failed" /tmp/petal-dev-rc.log 2>/dev/null; then
    echo "FATAL: native join failed -- see /tmp/petal-dev-rc.log:"
    grep "join_room('rctest') failed" /tmp/petal-dev-rc.log
    exit 1
  fi
  # The socket is the stable readiness contract. The live scenario immediately
  # follows with a retrying current_room command, so do not depend on an
  # informational join log string that has changed across logging revisions.
  if [ -S /tmp/petal-rc.sock ]; then
    JOINED=1
    sleep 2
    break
  fi
  sleep 1
done
if [ -z "$JOINED" ]; then
  echo "FATAL: Petal never opened the autotest socket within ${SOCKET_WAIT_S}s -- see /tmp/petal-dev-rc.log"
  exit 1
fi
echo "== 6. run the remote-control suite: $MODE_LABEL =="
cd "$REPO/apps/desktop"
PETAL_AUTOTEST_SOCK=/tmp/petal-rc.sock PETAL_WEB_HARNESS_URL_MATCH=localhost:5185 \
  node scripts/remote-control-local-loopback.mjs --live ${MODE_ARGS[@]+"${MODE_ARGS[@]}"} --json "$RESULTS_JSON"
echo "== done. results: $RESULTS_JSON =="

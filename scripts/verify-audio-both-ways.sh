#!/bin/bash
# Audio, both directions, measured on the decoded waveform -- never on packet
# or byte counters (#787 and #821 are both cases where counters looked healthy
# through total silence, in opposite directions).
#
#   leg 1  native -> native : the app publishes a 440Hz tone; a native
#                             subscriber (examples/audio_probe) measures the
#                             decoded PCM and Goertzel-checks the frequency.
#   leg 2  native -> web    : the same publish, measured in a REAL browser by
#                             the cockpit's AUD-N2W journey (AUD-04).
#
# Two things this script exists to encode, both of which cost a day to learn:
#
#   * Petal joins MUTED. A rig that only publishes measures digital silence and
#     cannot tell that from a broken pipeline. `PETAL_AUDIO_PUBLISH_UNMUTED=1`
#     is mandatory here, not a convenience.
#   * HEADLESS CHROME CANNOT DECODE REMOTE AUDIO. It has no output device, so
#     packets arrive while `totalSamplesReceived` stays 0 -- a real tone reads
#     as perfect silence. Leg 2 runs the browser headed (off-screen). No flag
#     fixes headless; `--use-fake-device-for-media-stream` and
#     `--alsa-output-device` were both measured still-blind.
#
# Usage: scripts/verify-audio-both-ways.sh [--leg1-only|--leg2-only]
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LEG="${1:-both}"
BIN="$REPO/apps/desktop/src-tauri/target/debug/desktop"
PROBE="$REPO/apps/desktop/src-tauri/target/debug/examples/audio_probe"
SWIFT_DYLD=/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx

# Never race another agent's live GUI run (see CLAUDE.md's dev:clean hazard).
if pgrep -f 'target/debug/desktop' >/dev/null 2>&1; then
  echo "FATAL: a Petal dev binary is already running -- not mine. Refusing to start." >&2
  exit 1
fi

OWNED=()
cleanup() {
  for pid in "${OWNED[@]:-}"; do
    [ -z "$pid" ] && continue
    if ps -p "$pid" >/dev/null 2>&1; then
      kill -TERM "$pid" 2>/dev/null; sleep 1
      ps -p "$pid" >/dev/null 2>&1 && kill -KILL "$pid" 2>/dev/null
    fi
  done
  # The cockpit names its Chrome profile `petal-cockpit-chrome-*`; leg 2 can
  # leave a HEADED, user-visible browser behind if its app is SIGKILLed, and a
  # pattern that matches nothing is worse than no cleanup at all.
  pkill -f 'petal-cockpit-chrome' 2>/dev/null
  if pgrep -f 'target/debug/desktop' >/dev/null 2>&1; then
    echo "WARNING: a desktop binary survived cleanup"
  else
    echo "cleanup: nothing this script started is still alive"
  fi
}
trap cleanup EXIT INT TERM

fail() { echo "FAIL: $*" >&2; exit 1; }

# ---------------------------------------------------------------- leg 1 -----
leg1() {
  echo "== leg 1: native -> native (local LiveKit, decoded PCM oracle) =="
  [ -x "$PROBE" ] || fail "missing $PROBE -- build it: cargo build --example audio_probe"
  if ! lsof -iTCP:7880 -sTCP:LISTEN >/dev/null 2>&1; then
    ( cd /tmp && exec livekit-server --dev >/tmp/petal-audio-verify-livekit.log 2>&1 ) &
    OWNED+=($!)
    for _ in $(seq 1 20); do lsof -iTCP:7880 -sTCP:LISTEN >/dev/null 2>&1 && break; sleep 1; done
  fi
  lsof -iTCP:7880 -sTCP:LISTEN >/dev/null 2>&1 || fail "no local livekit on :7880"

  local sock=/tmp/petal-audio-verify.sock
  rm -f "$sock"
  ( cd "$REPO/apps/desktop" && exec env PETAL_BACKEND_URL= LIVEKIT_URL=ws://localhost:7880 \
      LIVEKIT_API_KEY=devkey LIVEKIT_API_SECRET=secret \
      PETAL_ACCESSORY_UI=1 PETAL_DISABLE_AUDIO=0 PETAL_AUDIO_SYNTH_TONE=1 PETAL_AUDIO_PUBLISH_UNMUTED=1 \
      PETAL_AUTOTEST_ROOM=audverify PETAL_AUTOTEST_FRESH_ROOM=1 \
      PETAL_AUTOTEST_IDENTITY=native-audverify PETAL_AUTOTEST_SOCK="$sock" \
      "$BIN" >/tmp/petal-audio-verify-app.log 2>&1 ) &
  local app=$!
  OWNED+=($app)
  for _ in $(seq 1 120); do [ -S "$sock" ] && break; sleep 1; done
  [ -S "$sock" ] || fail "the app never opened its autotest socket"
  sleep 4

  grep -aq "PETAL_AUDIO_SYNTH_TONE=1" /tmp/petal-audio-verify-app.log \
    || fail "the synthetic tone hook never engaged"
  grep -aq "ignoring mute request" /tmp/petal-audio-verify-app.log \
    || echo "note: no mute request arrived to ignore (fine -- state was already unmuted)"

  # Ask the app which room it is in rather than scraping a log: the LiveKit
  # room name is `petal-room-<credential>` (rooms.rs::livekit_room_name_for),
  # and the app's own log redacts it.
  local credential room
  credential=$(printf '{"cmd":"current_room"}\n' | timeout 15 nc -U "$sock" 2>/dev/null \
    | python3 -c "import sys,json;d=json.load(sys.stdin);r=d.get('result') or {};print(r.get('name') or r.get('room') or '')" 2>/dev/null)
  [ -n "$credential" ] || fail "the app reported no current room"
  room="petal-room-$credential"
  echo "   room=$room"

  local out
  out=$(cd "$REPO/apps/desktop/src-tauri" && LIVEKIT_URL=ws://localhost:7880 \
        LIVEKIT_API_KEY=devkey LIVEKIT_API_SECRET=secret \
        DYLD_FALLBACK_LIBRARY_PATH="$SWIFT_DYLD" \
        timeout 90 "$PROBE" subscribe "$room" 2>&1)
  echo "$out" | grep -E "RMS amplitude|Goertzel power @440|dominant frequency"
  echo "$out" | grep -q "dominant frequency matches injected 440Hz tone: YES" \
    || fail "leg 1: the native subscriber did not receive the injected tone"
  local rms
  rms=$(echo "$out" | sed -n 's/.*RMS amplitude: \([0-9.]*\).*/\1/p')
  awk -v r="$rms" 'BEGIN { exit (r+0 > 500) ? 0 : 1 }' \
    || fail "leg 1: RMS $rms is below the 500 non-silence floor"
  echo "   leg 1 PASS (RMS $rms, 440Hz dominant)"
  # Wait for the process to actually die: `tauri-plugin-single-instance` makes
  # leg 2's launch forward-and-exit if leg 1 is still alive, which surfaces as
  # a baffling timeout rather than a clear error.
  kill -TERM "$app" 2>/dev/null
  for _ in $(seq 1 30); do ps -p "$app" >/dev/null 2>&1 || break; sleep 1; done
  ps -p "$app" >/dev/null 2>&1 && { kill -KILL "$app" 2>/dev/null; sleep 2; }
}

# ---------------------------------------------------------------- leg 2 -----
leg2() {
  echo "== leg 2: native -> web (cockpit AUD-N2W, real browser) =="
  echo "   (the cockpit launches the peer headed + off-screen; headless cannot decode audio)"

  # Serve THIS checkout's harness bundle rather than whatever is deployed, so
  # the run tests the code in the tree. The bundle keeps prod backend/LiveKit
  # wiring -- it is a production build, just served locally (docs/TESTING.md
  # hiccup 11); `npm run dev` would mint local tokens instead and is NOT a
  # substitute.
  if lsof -iTCP:4173 -sTCP:LISTEN >/dev/null 2>&1; then
    fail "port 4173 is already in use -- another session may own it"
  fi
  ( cd "$REPO/web-harness" && npm run build >/tmp/petal-audio-verify-harness-build.log 2>&1 ) \
    || fail "web-harness build failed (see /tmp/petal-audio-verify-harness-build.log)"
  ( cd "$REPO/web-harness" && exec npx vite preview --port 4173 --strictPort \
      >/tmp/petal-audio-verify-preview.log 2>&1 ) &
  OWNED+=($!)
  for _ in $(seq 1 30); do lsof -iTCP:4173 -sTCP:LISTEN >/dev/null 2>&1 && break; sleep 1; done
  lsof -iTCP:4173 -sTCP:LISTEN >/dev/null 2>&1 || fail "the harness preview never started"
  grep -rql 'remoteAudioAudible' "$REPO/web-harness/dist/assets/" >/dev/null 2>&1 \
    || fail "the served bundle does not carry the AUD-N2W oracle -- stale build"

  # Deliberately NOT PETAL_AUDIO_PUBLISH_UNMUTED: the cockpit unmutes through
  # the same SessionState path the menubar toggle uses, so this leg exercises
  # the real join-muted -> unmute-after-publish transition. Setting the hook
  # here would publish unmuted and skip the property most worth pinning.
  # Snapshot the results dirs first: on this multi-agent Mac, "newest" can be
  # a FOREIGN cockpit run, which is wrong in both directions.
  local before_dirs
  before_dirs=$(ls -d "$HOME/Library/Logs/Petal/test-runs/"*/ 2>/dev/null | sort)

  ( cd "$REPO/apps/desktop" && exec env PETAL_ACCESSORY_UI=1 PETAL_DISABLE_AUDIO=0 PETAL_AUDIO_SYNTH_TONE=1 \
      PETAL_HARNESS_URL=http://localhost:4173 \
      "$BIN" --test-case=AUD-N2W \
      >/tmp/petal-audio-verify-n2w.log 2>&1 ) &
  local app=$!
  OWNED+=($app)
  local waited=0
  while ps -p "$app" >/dev/null 2>&1 && [ $waited -lt 300 ]; do sleep 5; waited=$((waited+5)); done
  ps -p "$app" >/dev/null 2>&1 && { kill -KILL "$app" 2>/dev/null; fail "leg 2 timed out after ${waited}s"; }

  local dir
  dir=$(comm -13 <(printf '%s\n' "$before_dirs") \
                 <(ls -d "$HOME/Library/Logs/Petal/test-runs/"*/ 2>/dev/null | sort) | tail -1)
  [ -n "$dir" ] || fail "leg 2: this run produced no new cockpit results directory"
  python3 - "$dir" <<'PY'
import json, sys, pathlib
run = pathlib.Path(sys.argv[1]) / 'run.jsonl'
verdict = None
for line in run.read_text().splitlines():
    d = json.loads(line)
    if d.get('kind') == 'scenario-verdict' and d['payload'].get('scenarioId') == 'AUD-N2W':
        verdict = d['payload']
if not verdict:
    print('FAIL: leg 2 produced no scenario verdict'); sys.exit(1)
print('   ', verdict['verdict'].upper(), '|', verdict['message'][:200])
sys.exit(0 if verdict['verdict'] == 'pass' else 1)
PY
  [ $? -eq 0 ] || fail "leg 2: AUD-N2W did not pass"
  echo "   leg 2 PASS"
}

case "$LEG" in
  --leg1-only) leg1 ;;
  --leg2-only) leg2 ;;
  both) leg1; leg2 ;;
  *) echo "usage: $0 [--leg1-only|--leg2-only]"; exit 2 ;;
esac
echo "AUDIO VERIFIED BOTH WAYS"

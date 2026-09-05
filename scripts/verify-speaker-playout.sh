#!/bin/bash
# Speaker-playout gate for the #787 ordering: a web peer publishes a 440Hz
# tone FIRST, the native app joins the already-active meeting AFTER, and the
# verdict is measured at the SPEAKERS (this Mac's microphone), not at any
# counter, decode tap, or log line.
#
# Instrument rules learned the hard way (#821, and again on #787 where a bad
# metric "reproduced" a deaf native 5/5 while the speakers were audibly
# playing):
#   * The oracle must prove it can hear before its silence means anything:
#     an afplay positive control and a noise-floor control run FIRST, and a
#     failed control is INFRA-FAIL, never a product verdict.
#   * Score BAND energy (420-470Hz) against the rest of the spectrum. The
#     capture chain lands the tone at ~430-435Hz (resample clock skew), so an
#     exact-440 bin reads a fraction of the energy and lies.
#
# Live tier: needs a display session, speakers+mic, prod backend access, and
# the TCC-granted dev binary. Usage: scripts/verify-speaker-playout.sh [iters]
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/apps/desktop/src-tauri/target/debug/desktop"
CHROME="${PETAL_CHROME_BIN:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
ITERS="${1:-2}"
OUT="$(mktemp -d /tmp/petal-speaker-gate.XXXXXX)"
MIC_DEV="${PETAL_MIC_AVF_INDEX:-1}"   # ffmpeg avfoundation audio index of the real mic

if pgrep -f 'target/debug/desktop' >/dev/null 2>&1; then
  echo "FATAL: a Petal dev binary is already running -- not mine. Refusing." >&2; exit 1
fi
ORIG_VOL=$(osascript -e 'output volume of (get volume settings)')
OWNED=()
cleanup() {
  osascript -e "set volume output volume $ORIG_VOL" 2>/dev/null
  for pid in "${OWNED[@]:-}"; do
    [ -z "$pid" ] && continue
    ps -p "$pid" >/dev/null 2>&1 && { kill -TERM "$pid" 2>/dev/null; sleep 1; }
    ps -p "$pid" >/dev/null 2>&1 && kill -KILL "$pid" 2>/dev/null
  done
  pkill -f "petal-speaker-gate-chrome-$$" 2>/dev/null
  rm -rf "/tmp/petal-speaker-gate-chrome-$$" /tmp/petal-speaker-gate-tone.wav 2>/dev/null
  if pgrep -f 'target/debug/desktop' >/dev/null 2>&1; then
    echo "WARNING: a desktop binary survived cleanup"
  else
    echo "cleanup: nothing this script started is still alive (volume restored to $ORIG_VOL)"
  fi
}
trap cleanup EXIT INT TERM
fail() { echo "FAIL: $*" >&2; exit 1; }
infra() { echo "INFRA-FAIL: $*" >&2; exit 2; }

analyze() { # analyze <wav> -> prints "ratio=<r> rms_db=<db>", exits 0 tone / 1 no-tone
  python3 - "$1" <<'PY'
import sys, wave, struct, math
w = wave.open(sys.argv[1]); n=w.getnframes(); sr=w.getframerate()
s = struct.unpack(f"<{n}h", w.readframes(n))
N = min(32768, n); mid = max(0,(n-N)//2); seg = s[mid:mid+N]
def g(f):
    k=2*math.cos(2*math.pi*f/sr); s0=s1=s2=0.0
    for x in seg: s0=x+k*s1-s2; s2=s1; s1=s0
    return s1*s1+s2*s2-k*s1*s2
inband = sum(g(f) for f in range(415, 476, 5))
out = sum(g(f) for f in list(range(80, 415, 5)) + list(range(480, 2001, 5)))
ratio = inband/(out+1e-9)
rms = math.sqrt(sum(x*x for x in seg)/len(seg))
db = 20*math.log10(rms/32767) if rms else -120
print(f"ratio={ratio:.2f} rms_db={db:.1f}")
sys.exit(0 if ratio > 1.0 else 1)
PY
}
record() { ffmpeg -hide_banner -loglevel error -f avfoundation -i ":$MIC_DEV" -t 6 -ar 16000 -ac 1 -y "$1"; }

# --- instrument self-check: the oracle must hear afplay, and not hear noise --
osascript -e 'set volume output volume 55'
python3 - <<'PY'
import wave, math, struct
sr=44100; n=sr*8
w=wave.open('/tmp/petal-speaker-gate-tone.wav','w'); w.setnchannels(1); w.setsampwidth(2); w.setframerate(sr)
w.writeframes(b''.join(struct.pack('<h', int(12000*math.sin(2*math.pi*440*i/sr))) for i in range(n)))
w.close()
PY
record "$OUT/noise.wav"
if analyze "$OUT/noise.wav"; then infra "noise-floor control heard a 440Hz tone with nothing playing"; fi
afplay /tmp/petal-speaker-gate-tone.wav & AF=$!
sleep 1; record "$OUT/control.wav"; kill $AF 2>/dev/null
analyze "$OUT/control.wav" || infra "positive control failed -- this oracle cannot hear afplay, so a Petal silence verdict would be meaningless"
echo "instrument OK (positive + noise controls passed)"

# --- room: create/reuse the i787 QA room on prod -----------------------------
SOCK=/tmp/petal-speaker-gate.sock; rm -f "$SOCK"
( cd "$REPO/apps/desktop" && exec env PETAL_ACCESSORY_UI=1 PETAL_DISABLE_AUDIO=1 \
    PETAL_AUTOTEST_ROOM=i787 PETAL_AUTOTEST_FRESH_ROOM=1 \
    PETAL_AUTOTEST_IDENTITY=p-i787-resolve \
    PETAL_AUTOTEST_SOCK="$SOCK" "$BIN" >"$OUT/resolve-app.log" 2>&1 ) &
RESOLVE=$!; OWNED+=($RESOLVE)
for _ in $(seq 1 120); do [ -S "$SOCK" ] && break; sleep 1; done
[ -S "$SOCK" ] || infra "resolver app never opened its autotest socket"
CODE=""
for _ in $(seq 1 30); do
  CODE=$(printf '{"cmd":"current_room"}\n' | timeout 15 nc -U "$SOCK" 2>/dev/null \
    | python3 -c "import sys,json;d=json.load(sys.stdin);r=d.get('result') or {};print(r.get('accessCode') or '')" 2>/dev/null)
  [ -n "$CODE" ] && break; sleep 2
done
[ -n "$CODE" ] || { tail -5 "$OUT/resolve-app.log" >&2; infra "no accessCode from resolver"; }
kill -TERM "$RESOLVE" 2>/dev/null
for _ in $(seq 1 30); do ps -p "$RESOLVE" >/dev/null 2>&1 || break; sleep 1; done
ps -p "$RESOLVE" >/dev/null 2>&1 && { kill -KILL "$RESOLVE"; sleep 2; }

# --- serve this checkout's harness bundle ------------------------------------
# A pre-existing listener on the port is FOREIGN (another session's server,
# serving who-knows-what bundle) -- refuse rather than measure the wrong code.
if lsof -iTCP:4173 -sTCP:LISTEN >/dev/null 2>&1; then
  infra "port 4173 is already in use -- another session may own it"
fi
( cd "$REPO/web-harness" && npm run build >"$OUT/harness-build.log" 2>&1 ) \
  || infra "web-harness build failed (see $OUT/harness-build.log)"
( cd "$REPO/web-harness" && exec npx vite preview --port 4173 --strictPort \
    >"$OUT/preview.log" 2>&1 ) &
OWNED+=($!)
for _ in $(seq 1 30); do lsof -iTCP:4173 -sTCP:LISTEN >/dev/null 2>&1 && break; sleep 1; done
lsof -iTCP:4173 -sTCP:LISTEN >/dev/null 2>&1 || infra "the harness preview never started"

# --- web publishes FIRST and holds the tone ----------------------------------
PROFILE="/tmp/petal-speaker-gate-chrome-$$"
( exec "$CHROME" --user-data-dir="$PROFILE" --no-first-run --use-mock-keychain \
    --autoplay-policy=no-user-gesture-required \
    --window-position=-3000,0 --window-size=900,700 \
    "http://localhost:4173/?code=$CODE&auto=aud" >"$OUT/chrome.log" 2>&1 ) &
OWNED+=($!)
W=0; until [ $W -ge 15 ]; do sleep 3; W=$((W+3)); done

# --- native joins AFTER; the speakers are the verdict ------------------------
PASSES=0
INFRA_ITERS=0
for i in $(seq 1 "$ITERS"); do
  echo "=== iteration $i/$ITERS: native joins the already-publishing room ==="
  LOG="$OUT/native-$i.log"
  ( cd "$REPO/apps/desktop" && exec env PETAL_ACCESSORY_UI=1 PETAL_DISABLE_AUDIO=0 \
      PETAL_AUTOTEST_ROOM=i787 PETAL_AUTOTEST_IDENTITY="p-i787-gate$i" \
      "$BIN" >"$LOG" 2>&1 ) &
  APP=$!; OWNED+=($APP)
  SUB=0
  for _ in $(seq 1 60); do
    grep -aq "subscribed to remote audio track" "$LOG" 2>/dev/null && { SUB=1; break; }
    ps -p "$APP" >/dev/null 2>&1 || break
    sleep 1
  done
  if [ "$SUB" != 1 ]; then
    INFRA_ITERS=$((INFRA_ITERS+1))
    echo "  iter $i: no audio subscription (web peer down or join failed) -- INFRA"
  else
    grep -aq "pre-existing at join" "$LOG" \
      && echo "  telemetry: pre-existing-track coverage present (#787)" \
      || echo "  note: track arrived via live event, not pre-existing enumeration"
    U=0; until [ $U -ge 8 ]; do sleep 2; U=$((U+2)); done
    osascript -e 'set volume output volume 55'
    record "$OUT/mic-$i.wav"
    osascript -e "set volume output volume $ORIG_VOL"
    if RES=$(analyze "$OUT/mic-$i.wav"); then
      echo "  iter $i: SPEAKERS AUDIBLE ($RES)"; PASSES=$((PASSES+1))
    else
      echo "  iter $i: SPEAKERS SILENT ($RES) -- with the instrument self-check green, this IS a product failure (#787)"
      grep -aE "AdmProxy|playout" "$LOG" | tail -6 | sed 's/^/    log: /'
    fi
  fi
  kill -TERM "$APP" 2>/dev/null
  for _ in $(seq 1 30); do ps -p "$APP" >/dev/null 2>&1 || break; sleep 1; done
  ps -p "$APP" >/dev/null 2>&1 && { kill -KILL "$APP"; sleep 2; }
done
echo "result: $PASSES/$ITERS iterations audible at the speakers (artifacts: $OUT)"
if [ "$PASSES" -eq "$ITERS" ]; then
  echo "SPEAKER PLAYOUT VERIFIED (join-into-active-meeting ordering)"
elif [ "$INFRA_ITERS" -gt 0 ] && [ $((PASSES + INFRA_ITERS)) -eq "$ITERS" ]; then
  infra "every non-passing iteration was an instrument/rig failure -- nothing proven about the product"
else
  fail "speaker playout not verified"
fi

#!/usr/bin/env bash
# Contract tests for scripts/capture-preflight.sh (plan 6d step 2). No display,
# no Screen Recording grant, no real capture -- every probe result here is a
# synthesised artifact, which is what lets the NEGATIVE direction be tested at
# all. The one thing these cannot cover is that a genuinely healthy machine
# produces status "ready"; that needs the live run and is stated in the commit.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIB="$ROOT/scripts/capture-preflight.sh"
SUITE="$ROOT/scripts/rc-live-suite.sh"

TMP_ROOT="$(mktemp -d /tmp/petal-capture-preflight-test.XXXXXX)"
trap 'rm -rf "$TMP_ROOT"' EXIT INT TERM

fail() { echo "FAIL: $*" >&2; exit 1; }

# shellcheck source=scripts/capture-preflight.sh
source "$LIB"

result_line() {
  printf 'CAPTURE_PREFLIGHT_RESULT {"status":"%s","reason":"%s","window_id":42,"accepted_frames":0}\n' "$1" "$2"
}

# 1. A healthy probe result passes.
READY="$TMP_ROOT/ready.json"
result_line ready accepted-frame >"$READY"
evaluate_capture_preflight "$READY" >/dev/null || fail "a ready probe result must pass"

# 2. Every degraded reason fails, and each one is named in the output. A gate
#    whose negative result carries no information is worth nothing.
for reason in layout-rejection pixel-format-rejection stream-error no-image-buffer no-sck-output; do
  ARTIFACT="$TMP_ROOT/$reason.json"
  result_line failed "$reason" >"$ARTIFACT"
  if OUTPUT="$(evaluate_capture_preflight "$ARTIFACT")"; then
    fail "reason '$reason' must not pass"
  fi
  grep -q "$reason" <<<"$OUTPUT" || fail "verdict for '$reason' does not name it: $OUTPUT"
done

# 3. The Petal-side reasons are labelled Petal-side; the ambiguous ones are NOT
#    reported as proof Petal is broken. Losing this distinction is how a
#    runbook blames the wrong component.
for reason in layout-rejection pixel-format-rejection stream-error; do
  ARTIFACT="$TMP_ROOT/$reason.json"
  OUTPUT="$(evaluate_capture_preflight "$ARTIFACT" || true)"
  grep -q "FATAL" <<<"$OUTPUT" || fail "'$reason' must be FATAL: $OUTPUT"
  grep -q "Petal-side" <<<"$OUTPUT" || fail "'$reason' must be labelled Petal-side: $OUTPUT"
done
OUTPUT="$(evaluate_capture_preflight "$TMP_ROOT/no-sck-output.json" || true)"
grep -q "INCONCLUSIVE" <<<"$OUTPUT" || fail "no-sck-output must be INCONCLUSIVE: $OUTPUT"
grep -q "NOT proof Petal" <<<"$OUTPUT" || fail "no-sck-output must not blame Petal: $OUTPUT"

# 4. A permission block carries no JSON line at all, and must still fail closed
#    as inconclusive rather than being read as a Petal capture failure.
BLOCKED="$TMP_ROOT/blocked.json"
printf 'BLOCKED: Screen Recording permission not granted to this binary.\n' >"$BLOCKED"
OUTPUT="$(evaluate_capture_preflight "$BLOCKED" || true)"
grep -q "INCONCLUSIVE" <<<"$OUTPUT" || fail "a permission block must be INCONCLUSIVE: $OUTPUT"

# 5. THE STALE-FILE DEFECT. A missing artifact, an empty one, and one with no
#    result line must all fail. The old check passed on a file no run had
#    written.
if evaluate_capture_preflight "$TMP_ROOT/does-not-exist.json" >/dev/null; then
  fail "a missing artifact must never pass"
fi
: >"$TMP_ROOT/empty.json"
if evaluate_capture_preflight "$TMP_ROOT/empty.json" >/dev/null; then
  fail "an empty artifact must never pass"
fi
printf 'cargo output, no probe result\n' >"$TMP_ROOT/noresult.json"
if evaluate_capture_preflight "$TMP_ROOT/noresult.json" >/dev/null; then
  fail "an artifact with no CAPTURE_PREFLIGHT_RESULT must never pass"
fi

# 6. An unrecognised reason fails closed rather than falling through to pass.
UNKNOWN="$TMP_ROOT/unknown.json"
result_line failed brand-new-reason >"$UNKNOWN"
if evaluate_capture_preflight "$UNKNOWN" >/dev/null; then
  fail "an unrecognised reason must fail closed"
fi

# 7. Window selection is deliberate: layer 0, big enough, never Petal's own,
#    largest wins. A tiny helper window that never redraws would report
#    no-image-buffer and fail a healthy machine.
SELECTED="$(capture_preflight_select_window '[
  {"windowNumber":11,"owner":"Finder","layer":0,"w":500,"h":400},
  {"windowNumber":12,"owner":"Petal","layer":0,"w":1600,"h":1200},
  {"windowNumber":13,"owner":"Dock","layer":25,"w":1600,"h":1200},
  {"windowNumber":14,"owner":"Notes","layer":0,"w":100,"h":80},
  {"windowNumber":15,"owner":"Safari","layer":0,"w":900,"h":700}
]')"
[ "$SELECTED" = "15" ] || fail "expected the largest eligible window (15), got '$SELECTED'"

# No eligible window is a refusal, not a silent pass.
if capture_preflight_select_window '[{"windowNumber":9,"owner":"Petal","layer":0,"w":1600,"h":1200}]' >/dev/null 2>&1; then
  fail "a list with only ineligible windows must not yield a window"
fi
if capture_preflight_select_window 'not json' >/dev/null 2>&1; then
  fail "malformed probe output must not yield a window"
fi

# 8. The suite must actually USE all of this, and must not still carry the
#    three defects. Pure-function correctness proves nothing about the caller.
grep -q 'rm -f "$CAPTURE_PREFLIGHT_ARTIFACT"' "$SUITE" \
  || fail "rc-live-suite.sh must remove the probe artifact before the run"
grep -q 'evaluate_capture_preflight "$CAPTURE_PREFLIGHT_ARTIFACT"' "$SUITE" \
  || fail "rc-live-suite.sh must evaluate the probe artifact"
grep -q -- '--capture-preflight-only' "$SUITE" \
  || fail "rc-live-suite.sh must invoke the real publish_probe preflight"
grep -q 'screencapture -x -D 1' "$SUITE" \
  && fail "rc-live-suite.sh still runs the display-wide screencapture heuristic"
grep -q 'DID=' "$SUITE" \
  && fail "rc-live-suite.sh still assigns the unread \$DID"

echo "test result: capture preflight contract tests passed"

#!/usr/bin/env bash
# Per-window capture health check for scripts/rc-live-suite.sh step 0 (plan 6d
# step 2). Sourceable so the decision logic is testable without a display.
#
# What it replaces, and why: step 0 used to be
#
#   DID=$(screencapture -x -D 1 /tmp/_cap_probe.png 2>&1; ls -s /tmp/_cap_probe.png >/dev/null 2>&1 && echo ok)
#
# which could not fail, three times over. `$DID` was assigned and never read, so
# the result was discarded entirely. It captured a whole DISPLAY, which says
# nothing about per-window capture -- the thing under test. And it never removed
# the probe file, so a file left by any earlier run satisfied the `ls`.
#
# Reuse, don't build: examples/publish_probe.rs --capture-preflight-only already
# starts a real WindowCapture and prints one JSON line carrying a
# `capture_preflight_reason` taxonomy. That taxonomy is exactly the signal that
# was missing while five consecutive E2E attempts failed.
#
# THE TCC CAVEAT, and it must not be lost: the example binary has its OWN TCC
# identity, separate from Petal's. So `no-sck-output` (and an outright
# permission BLOCK) may reflect the EXAMPLE's Screen Recording grant rather than
# Petal's capture being broken. Those are reported as INCONCLUSIVE and must
# never be written up as proof that Petal's capture is broken.
# `layout-rejection`, `pixel-format-rejection` and `stream-error` are
# unambiguously Petal-side.

CAPTURE_PREFLIGHT_ARTIFACT="${CAPTURE_PREFLIGHT_ARTIFACT:-/tmp/petal-capture-preflight.json}"

# Classify one (status, reason) pair. Prints "<verdict> <explanation>" and
# returns 0 only for a verdict that may proceed.
capture_preflight_verdict() {
  local status="$1" reason="$2"
  if [ "$status" = "ready" ]; then
    echo "OK real per-window capture delivered an accepted frame"
    return 0
  fi
  case "$reason" in
    layout-rejection)
      echo "FATAL layout-rejection -- Petal-side: the capture raster was rejected before publish"
      return 1
      ;;
    pixel-format-rejection)
      echo "FATAL pixel-format-rejection -- Petal-side: SCK delivered an unsupported pixel format"
      return 1
      ;;
    stream-error)
      echo "FATAL stream-error -- Petal-side: the SCK stream itself errored"
      return 1
      ;;
    no-image-buffer)
      # The 6e signature: stream alive, source not drawing. Petal-side in the
      # sense that it is the real observed E2E failure, but it can also mean the
      # probed window genuinely never redrew.
      echo "FATAL no-image-buffer -- SCK samples arrived with NO image buffer (stream alive, source not drawing)"
      return 1
      ;;
    no-sck-output)
      echo "INCONCLUSIVE no-sck-output -- may be the EXAMPLE binary's own Screen Recording grant, NOT proof Petal's capture is broken"
      return 1
      ;;
    blocked)
      echo "INCONCLUSIVE blocked -- Screen Recording not granted to the example binary; this says nothing about Petal's own grant"
      return 1
      ;;
    *)
      echo "FATAL unrecognised capture_preflight_reason '${reason:-<none>}'"
      return 1
      ;;
  esac
}

# Read one field out of the probe's single JSON line without a JSON dependency.
capture_preflight_field() {
  local artifact="$1" field="$2"
  sed -n 's/.*"'"$field"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$artifact" | head -1
}

# Decide from the artifact on disk. Absence is FATAL, never a pass -- that is
# the whole point: a check whose negative result carries no information is worth
# nothing, and this one previously passed on a file no run had written.
evaluate_capture_preflight() {
  local artifact="$1" line status reason verdict rc
  if [ ! -s "$artifact" ]; then
    echo "== capture preflight FATAL: no probe artifact at $artifact -- the probe did not run, or wrote nothing =="
    return 1
  fi
  line="$(grep -m1 'CAPTURE_PREFLIGHT_RESULT' "$artifact" || true)"
  if [ -z "$line" ]; then
    if grep -q 'BLOCKED: Screen Recording permission not granted' "$artifact"; then
      status="blocked"
      reason="blocked"
    else
      echo "== capture preflight FATAL: artifact $artifact carries no CAPTURE_PREFLIGHT_RESULT line =="
      return 1
    fi
  else
    status="$(capture_preflight_field "$artifact" status)"
    reason="$(capture_preflight_field "$artifact" reason)"
  fi
  verdict="$(capture_preflight_verdict "$status" "$reason")"
  rc=$?
  echo "== capture preflight: $verdict =="
  return "$rc"
}

# Pick a window to probe from petal-window-probe.swift --find output. Layer 0
# only (real app windows), big enough to be a plausible share target, and never
# Petal's own. Deliberate rather than "first hit": a tiny helper window that
# never redraws would report no-image-buffer and fail a healthy machine.
capture_preflight_select_window() {
  local json="$1"
  printf '%s' "$json" | /usr/bin/python3 -c '
import json, sys
try:
    windows = json.load(sys.stdin)
except Exception:
    sys.exit(1)
best = None
for window in windows:
    if window.get("layer") != 0:
        continue
    if window.get("w", 0) < 400 or window.get("h", 0) < 300:
        continue
    if "petal" in (window.get("owner") or "").lower():
        continue
    if window.get("windowNumber", -1) <= 0:
        continue
    area = window["w"] * window["h"]
    if best is None or area > best[0]:
        best = (area, window["windowNumber"])
if best is None:
    sys.exit(1)
print(best[1])
'
}

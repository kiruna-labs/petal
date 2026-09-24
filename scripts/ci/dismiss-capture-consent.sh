#!/usr/bin/env bash
# Answer macOS screen-capture consent alerts while a capture job runs (#234).
#
# THE FAILURE THIS EXISTS FOR. macOS periodically re-asks
#   "<client> is requesting to bypass the system private window picker and
#    directly access your screen and audio"
# for a client that captures through ScreenCaptureKit without the system
# picker -- even when kTCCServiceScreenCapture is already allowed
# (auth_value 2). The alert is opaque and sits over the Aqua session.
#
# It does NOT fail a job outright. It occludes the capture region, so a
# pixel-sampling scenario silently measures the dialog instead of the share.
# That is how it broke the 0.9.28 release gate: SHARE-W2N-STALL scored
# test-fail on `stall-resumed-pixels` (1-3% of the sampled lattice changed
# against a 5% bar) while the media half was healthy at 30fps -- which reads
# exactly like a product regression, not like a runner problem.
#
# WHAT DOES NOT WORK, so nobody retries it (both verified on the golden image):
#   * Refreshing the TCC row's `last_reminded`. The prompt returned on the
#     very next run with the reminder minutes old.
#   * A guest LaunchAgent that clicks it. TCC attributes the click to the
#     agent's own process (/bin/bash), not to Runner.Listener, so every
#     System Events call hangs on an Apple Events consent prompt that no
#     one can answer. There is also no kTCCService* string for this consent.
#
# WHY THIS PLACE WORKS. Started from a workflow step, the responsible process
# is Runner.Listener, which provision-guest.sh already grants Accessibility
# and Apple Events control of System Events -- the same grants the
# remote-control suite drives TextEdit with. So the click is permitted here
# and nowhere else.
#
# It only clicks Allow on a window whose own text mentions screen capture, so
# an unrelated dialog cannot be answered by accident. It logs every window it
# considered, so a run that still fails says what was on screen.
set -uo pipefail

LOG="${1:-${RUNNER_TEMP:-/tmp}/capture-consent-dismisser.log}"
STOP="${2:-${RUNNER_TEMP:-/tmp}/capture-consent-dismisser.stop}"
INTERVAL_S="${CONSENT_POLL_SECONDS:-1}"

rm -f "$STOP"
echo "== dismisser start $(date -u +%FT%TZ) pid $$ (log $LOG)" >> "$LOG"
trap 'echo "== dismisser stop $(date -u +%FT%TZ)" >> "$LOG"; exit 0' TERM INT

# `every process` -- NOT `whose background only is false`. The consent alert
# belongs to a background UI agent, which that filter would skip.
read -r -d '' SCRIPT <<'AS' || true
tell application "System Events"
  set report to ""
  repeat with p in every process
    try
      repeat with w in (every window of p)
        set txt to ""
        try
          repeat with t in (every static text of w)
            set txt to txt & (value of t as text) & " "
          end repeat
        end try
        if txt is not "" then
          set report to report & "SAW[" & (name of p) & "]: " & txt & linefeed
          if (txt contains "screen" or txt contains "window picker" or txt contains "Screen") then
            try
              click (first button of w whose name is "Allow")
              set report to report & "CLICKED[" & (name of p) & "]" & linefeed
            end try
          end if
        end if
      end repeat
    end try
  end repeat
  return report
end tell
AS

while [ ! -f "$STOP" ]; do
  out="$(osascript -e "$SCRIPT" 2>&1)"
  if [ -n "$out" ]; then
    printf '%s %s\n' "$(date -u +%FT%TZ)" "$out" >> "$LOG"
  fi
  sleep "$INTERVAL_S"
done
echo "== dismisser stop (stop file) $(date -u +%FT%TZ)" >> "$LOG"

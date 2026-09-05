#!/bin/bash
# CLAUDE.md "Never show a black frame" (#627) -- NATIVE receiver half.
#
# The web-side counterpart is scripts/verify-no-black-frame.mjs, which runs in
# ci-local.sh because headless Chromium needs nothing from the host. This one
# needs a real window server and Screen Recording access, so it lives in the
# display-requiring tier (docs/TESTING.md) and ci-local.sh only compiles the
# probe to keep it from rotting.
#
# Runs the forced gap in BOTH directions in one pass:
#   * --stale-guard (positive control) reproduces the pre-#627 hide and must
#     see the share's pixels LEAVE the screen. If it does not, the harness
#     cannot observe the failure and the other direction proves nothing.
#   * the default run must see the last frame HELD across the gap.
#
# The probe validates itself first: it samples a known-bright window before
# deciding anything, and exits 3 (HARNESS INVALID) rather than reporting a
# pass or a fail if that baseline is not bright -- a denied screen capture
# returns black, which is indistinguishable from the failure being measured.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT/apps/desktop/src-tauri" || exit 1

# `cargo test`'s Swift concurrency dylib quirk applies to any binary that links
# the Swift-linked crates at launch (see CLAUDE.md "How to build & verify").
export DYLD_FALLBACK_LIBRARY_PATH="${DYLD_FALLBACK_LIBRARY_PATH:-/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx}"

BIN="$ROOT/apps/desktop/src-tauri/target/debug/examples/hold_last_frame_probe"

printf '\033[1m== building hold_last_frame_probe ==\033[0m\n'
if ! timeout 1800 cargo build --example hold_last_frame_probe; then
  echo "FAIL: could not build hold_last_frame_probe" >&2
  exit 1
fi

run_direction() {
  local label="$1"
  shift
  printf '\n\033[1m== %s ==\033[0m\n' "$label"
  timeout 120 "$BIN" "$@"
  local code=$?
  echo "(exit $code)"
  return $code
}

# Control FIRST: a harness whose control does not trip makes the other
# direction meaningless, so there is no reason to run it before this passes.
run_direction "POSITIVE CONTROL: pre-#627 hide must take the pixels off screen" --stale-guard
control=$?
if [ "$control" -eq 3 ]; then
  echo
  echo "SKIPPED: harness invalid on this host (see the probe's own diagnosis above)." >&2
  echo "  This gate needs a real window server AND Screen Recording access for the" >&2
  echo "  probe binary. Nothing was verified -- do NOT read this as a pass." >&2
  exit 0
fi
if [ "$control" -ne 0 ]; then
  echo "FAIL: the positive control did not trip; this harness cannot observe the #627" >&2
  echo "  native failure, so a passing fixed-path run would prove nothing." >&2
  exit 1
fi

run_direction "FIXED: teardown_decision must hold the last frame across the gap"
fixed=$?
if [ "$fixed" -ne 0 ]; then
  echo "FAIL: the share's last frame did not survive the forced gap (exit $fixed)." >&2
  exit 1
fi

run_direction "POSITIVE CONTROL: pre-#631 participant disconnect must take pixels off screen" --participant-disconnected --stale-guard
reconnect_control=$?
if [ "$reconnect_control" -ne 0 ]; then
  echo "FAIL: the #631 positive control did not trip; this harness cannot observe the reconnect hold path." >&2
  exit 1
fi

run_direction "FIXED: ParticipantDisconnected must hold the last frame across the gap" --participant-disconnected
reconnect_fixed=$?
if [ "$reconnect_fixed" -ne 0 ]; then
  echo "FAIL: the #631 participant-disconnect hold path did not preserve rendered pixels (exit $reconnect_fixed)." >&2
  exit 1
fi

run_direction "POSITIVE CONTROL: pre-fix TrackUnsubscribed must take pixels off screen" --track-unsubscribed --stale-guard
unsubscribe_control=$?
if [ "$unsubscribe_control" -ne 0 ]; then
  echo "FAIL: the #631 track-unsubscribe control did not trip; this harness cannot observe the reconnect race." >&2
  exit 1
fi

run_direction "FIXED: TrackUnsubscribed must hold the last frame across the reconnect gap" --track-unsubscribed
unsubscribe_fixed=$?
if [ "$unsubscribe_fixed" -ne 0 ]; then
  echo "FAIL: the #631 track-unsubscribe hold path did not preserve rendered pixels (exit $unsubscribe_fixed)." >&2
  exit 1
fi

run_direction "POSITIVE CONTROL: pre-#840 retire->reuse must leave the pixels off screen" --retire-reuse --stale-guard
reuse_control=$?
if [ "$reuse_control" -ne 0 ]; then
  echo "FAIL: the #840 retire/reuse control did not trip; this harness cannot observe a reuse" >&2
  echo "  that comes back ordered-out, so a passing fixed run would prove nothing." >&2
  exit 1
fi

run_direction "FIXED: retire->reuse must bring the retained layer content back on screen" --retire-reuse
reuse_fixed=$?
if [ "$reuse_fixed" -ne 0 ]; then
  echo "FAIL: #840 -- a reused window whose display layer still holds a frame did not come" >&2
  echo "  back on screen (exit $reuse_fixed). A hidden window with a live frame in its layer" >&2
  echo "  is the same product-rule violation as a black one." >&2
  exit 1
fi

printf '\n\033[1;32mno-black-frame native gate: PASS in both directions.\033[0m\n'
printf 'All controls took pixels off screen; teardown, participant-disconnect, track-unsubscribe,\n'
printf 'and retire/reuse holds kept the frame.\n'

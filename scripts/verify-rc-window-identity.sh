#!/bin/bash
# #779 AX window-identity regression guard -- live runner.
#
# `real_ax_window_identity_accepts_exact_window_and_refuses_same_app_sibling`
# is the ONLY guard on the fix that made remote control work again in 0.8.5,
# and it is opt-in behind PETAL_RUN_REAL_AX_WINDOW_IDENTITY_TEST=1 because it
# needs a WindowServer session, an Accessibility grant, and a real application
# serving two sibling windows. Nothing set that variable anywhere in the repo,
# so the guard ran in exactly zero automated invocations. This script is what
# runs it.
#
# Tier: display-requiring (docs/TESTING.md). `scripts/ci-local.sh` only
# compile-checks scripts/probes/twowin.m, the same anti-rot rule the other
# probes get.
#
# The fail-closed contract, in the shape scripts/verify-window-classification.sh
# established: anything that makes the run MEANINGLESS exits 3 (HARNESS
# INVALID) and never 0. In particular:
#   * the fixture failing to build, launch, or print its window ids  -> exit 3
#   * the guard passing without ever naming the fixture's pid        -> exit 3
#   * this process having no Accessibility grant                     -> exit 3
# A real resolver defect exits 1. Only a guard that actually exercised the
# fixture windows exits 0.
#
# Accessibility note: the cargo test harness inherits its TCC identity from
# whatever launched this script (Terminal/iTerm and friends are the
# responsible process). Grant THAT app Accessibility, or the run exits 3.
#
# Env:
#   PROBE_SPOT_X / PROBE_SPOT_Y  where to put the fixture windows (see twowin.m)
#   TWOWIN_SECONDS               fixture backstop lifetime (default 300)
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE="$ROOT/apps/desktop/src-tauri"
TEST_PATH="remote_control::input::tests::real_ax_window_identity_accepts_exact_window_and_refuses_same_app_sibling"
TEST_NAME="rc-real-ax-window-identity"
WORK="$(mktemp -d /tmp/rc-winid-gate.XXXXXX)"
FIXTURE_PID=""

fail_invalid() { echo "HARNESS INVALID: $1" >&2; exit 3; }

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if [ -n "$FIXTURE_PID" ]; then
    if kill -0 "$FIXTURE_PID" 2>/dev/null; then
      kill -TERM "$FIXTURE_PID" 2>/dev/null
      for _ in 1 2 3 4 5; do
        kill -0 "$FIXTURE_PID" 2>/dev/null || break
        sleep 1
      done
      kill -0 "$FIXTURE_PID" 2>/dev/null && kill -KILL "$FIXTURE_PID" 2>/dev/null
      sleep 1
    fi
    # Verify, never assume: a leaked AppKit fixture is a GUI process the
    # operator has to hunt down by hand.
    if kill -0 "$FIXTURE_PID" 2>/dev/null; then
      echo "WARN: twowin fixture pid $FIXTURE_PID SURVIVED teardown" >&2
    else
      echo "teardown clean: twowin fixture pid $FIXTURE_PID is gone (pid-verified)"
    fi
  fi
  rm -rf "$WORK"
  exit "$status"
}
trap cleanup EXIT INT TERM

command -v clang >/dev/null 2>&1 || fail_invalid "clang is not available to build the fixture"

# --- build the fixture -------------------------------------------------------
clang -fobjc-arc -framework Cocoa -o "$WORK/twowin" "$ROOT/scripts/probes/twowin.m" \
  || fail_invalid "scripts/probes/twowin.m failed to compile"

# --- build the test binary BEFORE the fixture is up --------------------------
# A cold `cargo test` can compile for minutes; starting the fixture first would
# mean racing its backstop and leaving windows on screen for the whole build.
echo "building the test harness (this may take a while on a cold target dir)..."
( cd "$CRATE" && DYLD_FALLBACK_LIBRARY_PATH=/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx \
    cargo test --lib --no-run ) > "$WORK/build.log" 2>&1 \
  || { tail -40 "$WORK/build.log" >&2; fail_invalid "cargo test --lib --no-run failed to build"; }

# --- launch the fixture ------------------------------------------------------
PROBE_SPOT_X="${PROBE_SPOT_X:-2300}" PROBE_SPOT_Y="${PROBE_SPOT_Y:-800}" \
TWOWIN_SECONDS="${TWOWIN_SECONDS:-300}" \
  "$WORK/twowin" > "$WORK/fixture.out" 2> "$WORK/fixture.err" &
FIXTURE_PID=$!

FIXTURE_LINE=""
for _ in $(seq 1 20); do
  sleep 1
  FIXTURE_LINE=$(grep -m1 '^TWOWIN pid=' "$WORK/fixture.err" 2>/dev/null)
  [ -n "$FIXTURE_LINE" ] && break
done
[ -n "$FIXTURE_LINE" ] \
  || fail_invalid "the twowin fixture never printed its window ids (no display, or it failed to open windows); nothing this run could report would mean anything"

REPORTED_PID=${FIXTURE_LINE#*pid=}; REPORTED_PID=${REPORTED_PID%% *}
WID_A=${FIXTURE_LINE#*wid_a=}; WID_A=${WID_A%% *}
WID_B=${FIXTURE_LINE#*wid_b=}; WID_B=${WID_B%% *}
# `$!` is the pid of the command we backgrounded; the fixture reports its own
# getpid(). If those disagree we are holding the wrong pid and our teardown
# would kill nothing (SHARED_RULES: capture the real pid, never a subshell's).
[ "$REPORTED_PID" = "$FIXTURE_PID" ] \
  || fail_invalid "fixture pid mismatch: backgrounded $FIXTURE_PID but the fixture reports $REPORTED_PID; teardown would leak a GUI process"
[ -n "$WID_A" ] && [ -n "$WID_B" ] && [ "$WID_A" != "$WID_B" ] \
  || fail_invalid "fixture did not open two distinct windows ($FIXTURE_LINE)"
echo "fixture up: pid=$FIXTURE_PID windows=$WID_A,$WID_B"

# --- run the guard -----------------------------------------------------------
( cd "$CRATE" && PETAL_RUN_REAL_AX_WINDOW_IDENTITY_TEST=1 \
    DYLD_FALLBACK_LIBRARY_PATH=/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx \
    cargo test --lib -- --exact "$TEST_PATH" --nocapture --test-threads=1 ) \
  > "$WORK/test.log" 2>&1
TEST_STATUS=$?
cat "$WORK/test.log"

# A missing Accessibility grant is a harness problem, not a resolver defect.
# The test panics on it (opted-in-but-unusable is a failure, never a skip);
# translate that one panic into HARNESS INVALID so it is never read as "the
# #779 fix regressed".
if grep -q "has no macOS Accessibility grant" "$WORK/test.log"; then
  fail_invalid "this process has no Accessibility grant -- grant it to the terminal app that launched this script and re-run"
fi
if grep -q "_AXUIElementGetWindow is unavailable" "$WORK/test.log"; then
  fail_invalid "_AXUIElementGetWindow is unavailable on this OS build; the production primary path cannot be exercised"
fi

if [ "$TEST_STATUS" -ne 0 ]; then
  echo "FAIL  $TEST_NAME: the guard did not pass (see the output above)" >&2
  exit 1
fi

# The guard walks every application with two sibling windows and returns on the
# first that qualifies. Passing on SOMEONE ELSE'S windows proves the resolver
# works but proves nothing about this run's fixture -- and would mean the gate
# still passes with the fixture absent, i.e. exactly the "green regardless"
# shape this script exists to remove. Demand our own pid in a PASS line.
if ! grep -q "PASS\[$TEST_NAME\]: pid=$FIXTURE_PID " "$WORK/test.log"; then
  fail_invalid "the guard passed but never exercised the twowin fixture (pid $FIXTURE_PID); its result is not attributable to this run"
fi

echo "PASS  $TEST_NAME: resolver told twowin windows $WID_A and $WID_B apart (pid $FIXTURE_PID)"
exit 0

#!/usr/bin/env bash
# Both-directions contract test for scripts/petal-instance-guard.sh (#846).
#
# CLAUDE.md, "How to build & verify" rule 8: a gate must be tested in BOTH
# directions before it is relied on -- it must fire on a foreign instance AND
# pass when there is none / when the running instance is on the allowlist.
#
# The subject is a SYMLINK to /bin/sleep named `desktop` inside a directory
# whose path stands in for a bundle's Contents/MacOS -- so `pgrep -f
# "Contents/MacOS/desktop"` sees a real process with a real matching command
# line and nothing else is touched. Must be a symlink, not a copy: copying a
# signed Apple binary strips its signature and the kernel SIGKILLs it on exec,
# which would vanish the fake before the guard ever ran.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/petal-instance-guard.sh
source "$ROOT/scripts/petal-instance-guard.sh"

TMP="$(mktemp -d /tmp/petal-instance-guard-unit-test.XXXXXX)"
FAKE_PID=""
cleanup() {
  [ -n "$FAKE_PID" ] && kill -KILL "$FAKE_PID" 2>/dev/null
  rm -rf "$TMP"
}
trap cleanup EXIT INT TERM
fail() { echo "FAIL: $*" >&2; exit 1; }

mkdir -p "$TMP/Contents/MacOS"
ln -s /bin/sleep "$TMP/Contents/MacOS/desktop"

# --- Direction 1: nothing running -> petal_guard_no_foreign_instance PASSES.
if pgrep -f "Contents/MacOS/desktop" >/dev/null 2>&1; then
  echo "SKIP direction 1: a real Contents/MacOS/desktop process is already running on this machine; cannot test the clean case"
else
  petal_guard_no_foreign_instance "" || fail "guard rejected a CLEAN machine (false positive)"
  echo "ok  1 - a clean machine passes the guard"
fi

# --- Direction 2: an untracked matching process -> the guard REFUSES.
"$TMP/Contents/MacOS/desktop" 120 &
FAKE_PID=$!
for _ in $(seq 1 40); do
  pgrep -f "Contents/MacOS/desktop" 2>/dev/null | grep -qx "$FAKE_PID" && break
  sleep 0.05
done
pgrep -f "Contents/MacOS/desktop" 2>/dev/null | grep -qx "$FAKE_PID" \
  || fail "test setup: the fake desktop process never appeared to pgrep"

set +e
OUTPUT="$(petal_guard_no_foreign_instance "" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -eq 3 ] || fail "guard must return 3 for an untracked instance, got $STATUS"
grep -q "already running" <<<"$OUTPUT" || fail "guard must say what it found: $OUTPUT"
grep -q "$FAKE_PID" <<<"$OUTPUT" || fail "guard must name the offending PID: $OUTPUT"
echo "ok  2 - an untracked instance is refused, by pid, with a usable message"

# --- Direction 3: the SAME process, but on the allowlist -> the guard PASSES.
petal_guard_no_foreign_instance "$FAKE_PID" \
  || fail "guard rejected an ALLOWLISTED pid (false positive)"
echo "ok  3 - an allowlisted pid does not trip the guard"

# --- Direction 4: petal_guard_kill_pid_verified refuses a command-line mismatch.
set +e
OUTPUT="$(petal_guard_kill_pid_verified "$FAKE_PID" "/some/other/unrelated/path" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -eq 1 ] || fail "kill-verified must refuse a substring mismatch, got status $STATUS"
kill -0 "$FAKE_PID" 2>/dev/null || fail "kill-verified must NOT have killed a command-line mismatch"
echo "ok  4 - kill-verified refuses to kill a pid whose command line doesn't match"

# --- Direction 5: petal_guard_kill_pid_verified kills a genuine match.
petal_guard_kill_pid_verified "$FAKE_PID" "$TMP/Contents/MacOS/desktop"
kill -0 "$FAKE_PID" 2>/dev/null && fail "kill-verified left a genuine match alive"
echo "ok  5 - kill-verified kills a pid whose command line does match"
FAKE_PID=""

# ==========================================================================
# Phase-handoff helpers (#150). Every one of these is exercised in BOTH
# directions -- the branch that fires AND the branch that must not -- because
# a gate whose failing direction has never run is exactly the defect #133 was
# about.
# ==========================================================================

EXTRA_PIDS=()
# Redefined so the new fakes are torn down too. Every kill is `|| true`: under
# `set -e` a failing kill (an already-exited pid, which is the NORMAL case
# here) aborts the trap where it stands and silently leaks every process after
# it -- observed while writing these tests, as a leaked fake `desktop` that
# then failed the NEXT run's clean-machine direction.
cleanup() {
  [ -n "$FAKE_PID" ] && kill -KILL "$FAKE_PID" 2>/dev/null
  for p in ${EXTRA_PIDS[@]+"${EXTRA_PIDS[@]}"}; do kill -KILL "$p" 2>/dev/null || true; done
  rm -rf "$TMP"
}

# --- Direction 6: a pid that DOES exit -> the wait returns promptly, 0.
# This also proves the zombie case: the exited process is this shell's own
# unreaped child, and a naive `ps -p` check would sit on it for the whole
# budget.
sleep 1 &
SHORT_PID=$!
EXTRA_PIDS+=("$SHORT_PID")
START="$SECONDS"
set +e
OUTPUT="$(petal_guard_wait_for_pid_exit "$SHORT_PID" 20 "the short-lived process" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -eq 0 ] || fail "wait must return 0 for a process that exits, got $STATUS: $OUTPUT"
[ $((SECONDS - START)) -lt 15 ] || fail "wait sat on an exited (zombie) child instead of returning"
echo "ok  6 - waiting on a pid that exits returns 0, and a zombie counts as exited"

# --- Direction 7: a pid that does NOT exit -> the wait fails, loudly, by pid.
ln -s /bin/sleep "$TMP/slow-desktop"
"$TMP/slow-desktop" 120 &
SLOW_PID=$!
EXTRA_PIDS+=("$SLOW_PID")
set +e
OUTPUT="$(petal_guard_wait_for_pid_exit "$SLOW_PID" 2 "the stuck process" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -eq 4 ] || fail "wait must return 4 when the budget expires, got $STATUS"
grep -q "$SLOW_PID" <<<"$OUTPUT" || fail "the timeout must name the pid: $OUTPUT"
grep -q "the stuck process" <<<"$OUTPUT" || fail "the timeout must name the cause: $OUTPUT"
echo "ok  7 - a pid that never exits fails the wait with a named cause"

# --- Direction 8: the multi-instance wait, both ways, on explicit pids.
set +e
OUTPUT="$(petal_guard_wait_for_instances_exit 2 "$SLOW_PID" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -eq 4 ] || fail "instance wait must fail while an instance is alive, got $STATUS"
set +e
OUTPUT="$(petal_guard_wait_for_instances_exit 5 "$SHORT_PID" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -eq 0 ] || fail "instance wait must pass once the instance has exited, got $STATUS: $OUTPUT"
echo "ok  8 - the phase-handoff wait fails on a live instance and passes on an exited one"

# --- Directions 9-13: the socket diagnosis names each distinct cause.
CRASH_DIR="$TMP/crashes"
mkdir -p "$CRASH_DIR"
MARK="$TMP/phase-start.mark"
touch "$MARK"

sleep 120 &
LAUNCHER_PID=$!
EXTRA_PIDS+=("$LAUNCHER_PID")

# 9: an instance that predates the launch is still alive -> the lock cause.
set +e
OUTPUT="$(petal_guard_diagnose_missing_socket "$TMP/petal-rc.sock" "$LAUNCHER_PID" "$CRASH_DIR" "$MARK" "$SLOW_PID" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -eq 2 ] || fail "a live pre-launch instance must diagnose as the single-instance lock, got $STATUS: $OUTPUT"
grep -q "cause=single-instance-lock-held" <<<"$OUTPUT" || fail "missing cause token: $OUTPUT"
grep -q "$SLOW_PID" <<<"$OUTPUT" || fail "the lock diagnosis must name the holding pid: $OUTPUT"
echo "ok  9 - a held single-instance lock is reported as its own cause, by pid"

# 10: no live pre-launch instance, but a fresh crash report -> the crash cause.
# (The pre-launch pid is passed DEAD here, which also proves the lock branch
# does not fire on a pid that has since exited.)
kill -KILL "$SLOW_PID" 2>/dev/null || true
wait "$SLOW_PID" 2>/dev/null || true
sleep 1
touch "$CRASH_DIR/desktop-2026-09-10-000000.ips"
set +e
OUTPUT="$(petal_guard_diagnose_missing_socket "$TMP/petal-rc.sock" "$LAUNCHER_PID" "$CRASH_DIR" "$MARK" "$SLOW_PID" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -eq 3 ] || fail "a fresh crash report must diagnose as a crash, got $STATUS: $OUTPUT"
grep -q "cause=petal-crashed" <<<"$OUTPUT" || fail "missing cause token: $OUTPUT"
grep -q "desktop-2026-09-10-000000.ips" <<<"$OUTPUT" || fail "the crash diagnosis must name the report: $OUTPUT"
echo "ok 10 - a fresh crash report is reported as a crash, and names the .ips"

# 11: no crash, launcher gone -> the launcher cause.
rm -f "$CRASH_DIR"/*.ips
kill -KILL "$LAUNCHER_PID" 2>/dev/null || true
wait "$LAUNCHER_PID" 2>/dev/null || true
set +e
OUTPUT="$(petal_guard_diagnose_missing_socket "$TMP/petal-rc.sock" "$LAUNCHER_PID" "$CRASH_DIR" "$MARK" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -eq 4 ] || fail "a dead launcher must diagnose as launcher-exited, got $STATUS: $OUTPUT"
grep -q "cause=launcher-exited" <<<"$OUTPUT" || fail "missing cause token: $OUTPUT"
echo "ok 11 - a launcher that exited is reported as its own cause"

# 12/13: with a live launcher, the two remaining causes turn on whether any
# Petal-ish process exists.
sleep 120 &
LAUNCHER2_PID=$!
EXTRA_PIDS+=("$LAUNCHER2_PID")

if petal_guard_live_instances >/dev/null && [ -n "$(petal_guard_live_instances)" ]; then
  echo "SKIP 12 - a real Petal-ish process is running on this machine; cannot test the empty case"
else
  set +e
  OUTPUT="$(petal_guard_diagnose_missing_socket "$TMP/petal-rc.sock" "$LAUNCHER2_PID" "$CRASH_DIR" "$MARK" 2>&1)"
  STATUS=$?
  set -e
  [ "$STATUS" -eq 5 ] || fail "no process at all must diagnose as no-petal-process, got $STATUS: $OUTPUT"
  grep -q "cause=no-petal-process" <<<"$OUTPUT" || fail "missing cause token: $OUTPUT"
  echo "ok 12 - 'nothing ever came up' is reported distinctly from a held lock"
fi

"$TMP/Contents/MacOS/desktop" 120 &
RUNNING_PID=$!
EXTRA_PIDS+=("$RUNNING_PID")
for _ in $(seq 1 40); do
  pgrep -f "Contents/MacOS/desktop" 2>/dev/null | grep -qx "$RUNNING_PID" && break
  sleep 0.05
done
set +e
OUTPUT="$(petal_guard_diagnose_missing_socket "$TMP/petal-rc.sock" "$LAUNCHER2_PID" "$CRASH_DIR" "$MARK" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -eq 6 ] || fail "a live instance with no socket must diagnose as running-but-no-socket, got $STATUS: $OUTPUT"
grep -q "cause=running-but-no-socket" <<<"$OUTPUT" || fail "missing cause token: $OUTPUT"
grep -q "$RUNNING_PID" <<<"$OUTPUT" || fail "the diagnosis must name the live pid: $OUTPUT"
echo "ok 13 - 'up but not answering' is reported distinctly from every other cause"

echo "test result: petal-instance-guard contract tests passed"

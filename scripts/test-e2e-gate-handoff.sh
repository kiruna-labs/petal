#!/usr/bin/env bash
# Contract test for the live e2e gate's Cockpit -> loopback handoff (#150).
#
# Two halves, because they fail differently:
#
#  1. A STATIC half: the workflow must actually be wired to the by-pid
#     helpers, and must actually upload the evidence its own steps collect.
#     Every assertion is run against a MUTATED copy too -- an assertion that
#     has never been seen to fail is worth nothing (CLAUDE.md, "test a gate in
#     BOTH directions"; the crashes-upload glob is here precisely because a
#     gate printed "Petal wrote crash report(s)" for weeks while the artifact
#     upload silently dropped every one of them).
#
#  2. A BEHAVIOURAL half: the socket wait is EXTRACTED FROM THE WORKFLOW and
#     executed, so what is proved is the real wiring rather than a re-typed
#     copy of it. This is the #497 lesson -- a green unit test on a helper
#     proves the helper, not that anything calls it with the right inputs from
#     the real path. `PETAL_SOCKET_BUDGET_S` exists only so this half can run
#     in seconds; CI leaves it unset and the budget stays 600.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKFLOW="$ROOT/.github/workflows/nightly-loopback.yml"
GUARD="$ROOT/scripts/petal-instance-guard.sh"

TMP="$(mktemp -d /tmp/petal-e2e-gate-handoff-test.XXXXXX)"
PIDS=()
cleanup() {
  for p in ${PIDS[@]+"${PIDS[@]}"}; do kill -KILL "$p" 2>/dev/null || true; done
  rm -rf "$TMP"
}
trap cleanup EXIT INT TERM
fail() { echo "FAIL: $*" >&2; exit 1; }

[ -f "$WORKFLOW" ] || fail "missing $WORKFLOW"

# --------------------------------------------------------------------------
# Static half.
# --------------------------------------------------------------------------

# check_wiring <workflow file>
# Prints one `missing: <what>` line per broken requirement; returns 1 if any.
check_wiring() {
  local wf="$1" rc=0
  grep -q 'name: Wait for the Cockpit'"'"'s Petal to exit (phase handoff)' "$wf" \
    || { echo "missing: the phase-handoff step"; rc=1; }
  grep -q 'petal_guard_wait_for_instances_exit' "$wf" \
    || { echo "missing: the by-pid handoff wait"; rc=1; }
  grep -q 'petal_guard_diagnose_missing_socket' "$wf" \
    || { echo "missing: the socket-timeout diagnosis"; rc=1; }
  grep -q 'source scripts/petal-instance-guard.sh' "$wf" \
    || { echo "missing: the guard is never sourced"; rc=1; }
  grep -q 'petal-nightly-loopback/crashes/\*\*' "$wf" \
    || { echo "missing: the loopback crash reports are not uploaded"; rc=1; }
  grep -q 'petal-nightly-loopback/\*\.txt' "$wf" \
    || { echo "missing: the wedge evidence (.txt) is not uploaded"; rc=1; }
  return "$rc"
}

# --- Direction 1: the real workflow satisfies every requirement.
set +e
OUTPUT="$(check_wiring "$WORKFLOW" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -eq 0 ] || fail "the real workflow is not wired for #150: $OUTPUT"
echo "ok  1 - the live gate is wired to the by-pid handoff, the diagnosis, and the evidence uploads"

# --- Direction 2: each requirement, deleted, is actually caught.
mutate_and_expect() {
  local pattern="$1" expect="$2" copy="$TMP/mutant.yml"
  grep -v -- "$pattern" "$WORKFLOW" > "$copy"
  cmp -s "$copy" "$WORKFLOW" && fail "mutation '$pattern' changed nothing -- the test would pass for the wrong reason"
  set +e
  local out; out="$(check_wiring "$copy" 2>&1)"
  local st=$?
  set -e
  [ "$st" -ne 0 ] || fail "deleting '$pattern' was NOT caught by check_wiring"
  grep -q "$expect" <<<"$out" || fail "deleting '$pattern' reported the wrong thing: $out"
}
mutate_and_expect 'petal_guard_wait_for_instances_exit' 'missing: the by-pid handoff wait'
mutate_and_expect 'petal_guard_diagnose_missing_socket' 'missing: the socket-timeout diagnosis'
mutate_and_expect 'petal-nightly-loopback/crashes/' 'missing: the loopback crash reports are not uploaded'
mutate_and_expect 'petal-nightly-loopback/\*\.txt' 'missing: the wedge evidence'
echo "ok  2 - removing any one of them fails the check, and says which"

# --------------------------------------------------------------------------
# Behavioural half: run the workflow's OWN socket wait.
# --------------------------------------------------------------------------
BLOCK="$TMP/socket-wait.sh"
{
  echo 'set -euo pipefail'
  echo "source '$GUARD'"
  awk '/==> Wait for native join and autotest socket/,/==> Run live remote-control local loopback/' "$WORKFLOW" \
    | sed '$d' \
    | sed 's/^          //'
} > "$BLOCK"
grep -q 'petal_guard_diagnose_missing_socket' "$BLOCK" \
  || fail "extraction failed: the socket wait block does not contain the diagnosis call"
bash -n "$BLOCK" || fail "extracted socket-wait block is not valid bash"

# run_block <socket-path> -> prints the block's output, returns its status.
run_block() {
  set +e
  (
    export PETAL_SOCKET_BUDGET_S="$BUDGET"
    export SOCKET="$1" PETAL_LOG="$PETAL_LOG" HARNESS_LOG="$HARNESS_LOG"
    export LAUNCHER_PID="$LAUNCHER_PID" PRE_LAUNCH_INSTANCES="$PRE_LAUNCH_INSTANCES"
    export CRASH_MARK="$CRASH_MARK" HOME="$TMP/fakehome" LOG_DIR="$TMP/logdir"
    bash "$BLOCK" 2>&1
  )
  local st=$?
  set -e
  return "$st"
}

mkdir -p "$TMP/fakehome/Library/Logs/DiagnosticReports" "$TMP/logdir"
PETAL_LOG="$TMP/petal-dev.log"; : > "$PETAL_LOG"
HARNESS_LOG="$TMP/harness.log"; : > "$HARNESS_LOG"
CRASH_MARK="$TMP/start.mark"; touch "$CRASH_MARK"
PRE_LAUNCH_INSTANCES=""
BUDGET=4

sleep 300 &
LAUNCHER_PID=$!
PIDS+=("$LAUNCHER_PID")

# --- Direction 3: the socket IS there -> the wait passes and says nothing.
SOCK_OK="$TMP/petal-rc.sock"
python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1])' "$SOCK_OK"
[ -S "$SOCK_OK" ] || fail "test setup: could not create a real unix socket"
set +e
OUTPUT="$(run_block "$SOCK_OK")"
STATUS=$?
set -e
[ "$STATUS" -eq 0 ] || fail "the wait must PASS when the socket exists (false positive), got $STATUS: $OUTPUT"
grep -q "::error::" <<<"$OUTPUT" && fail "a healthy wait must not emit an error: $OUTPUT"
echo "ok  3 - a socket that appears passes the wait with no error"

# --- Direction 4: a native-join failure still fails on its own message.
printf "session: join_room('room-x') failed: nope\n" >> "$PETAL_LOG"
set +e
OUTPUT="$(run_block "$TMP/never.sock")"
STATUS=$?
set -e
[ "$STATUS" -eq 1 ] || fail "a join failure must fail the wait, got $STATUS: $OUTPUT"
grep -q "Native join failed" <<<"$OUTPUT" || fail "a join failure must keep its own message: $OUTPUT"
: > "$PETAL_LOG"
echo "ok  4 - a native-join failure is still reported as a join failure"

# --- Direction 5: no socket, and an instance from BEFORE the launch is still
# alive -> the timeout names the single-instance lock instead of shrugging.
mkdir -p "$TMP/Contents/MacOS"
ln -s /bin/sleep "$TMP/Contents/MacOS/desktop"
"$TMP/Contents/MacOS/desktop" 300 &
STALE_PID=$!
PIDS+=("$STALE_PID")
for _ in $(seq 1 40); do
  pgrep -f "Contents/MacOS/desktop" 2>/dev/null | grep -qx "$STALE_PID" && break
  sleep 0.05
done
PRE_LAUNCH_INSTANCES="$STALE_PID"
set +e
OUTPUT="$(run_block "$TMP/never.sock")"
STATUS=$?
set -e
[ "$STATUS" -eq 1 ] || fail "a missing socket must fail the wait, got $STATUS: $OUTPUT"
grep -q "cause: single-instance-lock-held" <<<"$OUTPUT" \
  || fail "the timeout must name the held lock as its cause: $OUTPUT"
grep -q "$STALE_PID" <<<"$OUTPUT" || fail "the timeout must name the holding pid: $OUTPUT"
[ -d "$TMP/logdir/crashes" ] \
  || fail "the timeout path must create the crash-report directory the artifact upload covers"
echo "ok  5 - a socket timeout with a live foreign instance names the single-instance lock, by pid"

# --- Direction 6: same missing socket, but nothing is holding a lock and the
# launcher died -> a DIFFERENT named cause, and it does not sit out the budget.
kill -KILL "$STALE_PID" 2>/dev/null || true
wait "$STALE_PID" 2>/dev/null || true
kill -KILL "$LAUNCHER_PID" 2>/dev/null || true
wait "$LAUNCHER_PID" 2>/dev/null || true
PRE_LAUNCH_INSTANCES=""
if [ -n "$(bash -c "source '$GUARD'; petal_guard_live_instances | tr -d '[:space:]'")" ]; then
  echo "SKIP 6 - a real Petal-ish process is running on this machine; cannot test the empty case"
else
  BUDGET=60
  START="$SECONDS"
  set +e
  OUTPUT="$(run_block "$TMP/never.sock")"
  STATUS=$?
  set -e
  ELAPSED=$((SECONDS - START))
  [ "$STATUS" -eq 1 ] || fail "a missing socket must fail the wait, got $STATUS: $OUTPUT"
  grep -q "cause: launcher-exited" <<<"$OUTPUT" \
    || fail "a dead launcher must be reported as its own cause, not as a held lock: $OUTPUT"
  [ "$ELAPSED" -lt 30 ] || fail "the wait sat out the full budget on a decided outcome (${ELAPSED}s)"
  echo "ok  6 - a dead launcher gets a different named cause, in ${ELAPSED}s rather than the full budget"
fi

# --------------------------------------------------------------------------
# The handoff step itself, extracted and executed -- both directions.
# --------------------------------------------------------------------------
HANDOFF="$TMP/handoff-step.sh"
awk '
  /name: Wait for the Cockpit/ { instep = 1 }
  instep && /^        run: \|$/ { body = 1; next }
  body {
    if ($0 ~ /^[[:space:]]*$/) { print ""; next }
    if ($0 !~ /^          /) exit
    print
  }
' "$WORKFLOW" | sed 's/^          //' > "$HANDOFF"
grep -q 'petal_guard_wait_for_instances_exit' "$HANDOFF" \
  || fail "extraction failed: the handoff step body has no wait call"
bash -n "$HANDOFF" || fail "extracted handoff step is not valid bash"

# --- Direction 7: a Petal instance that will not exit -> the handoff REFUSES,
# and says so as the gate's own error, naming the phase.
"$TMP/Contents/MacOS/desktop" 300 &
STUCK_PID=$!
PIDS+=("$STUCK_PID")
for _ in $(seq 1 40); do
  pgrep -f "Contents/MacOS/desktop" 2>/dev/null | grep -qx "$STUCK_PID" && break
  sleep 0.05
done
set +e
OUTPUT="$(cd "$ROOT" && PETAL_PHASE_HANDOFF_TIMEOUT_S=2 bash "$HANDOFF" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -eq 1 ] || fail "the handoff must fail while an instance is still running, got $STATUS: $OUTPUT"
grep -q "::error::Cockpit -> loopback handoff" <<<"$OUTPUT" \
  || fail "the handoff failure must name the phase it broke: $OUTPUT"
grep -q "$STUCK_PID" <<<"$OUTPUT" || fail "the handoff failure must name the pid: $OUTPUT"
echo "ok  7 - the handoff refuses to start the loopback phase while a Petal is still alive"

# --- Direction 8: nothing running -> the handoff passes, quickly and quietly.
kill -KILL "$STUCK_PID" 2>/dev/null || true
wait "$STUCK_PID" 2>/dev/null || true
if [ -n "$(bash -c "source '$GUARD'; petal_guard_live_instances | tr -d '[:space:]'")" ]; then
  echo "SKIP 8 - a real Petal-ish process is running on this machine; cannot test the clean case"
else
  START="$SECONDS"
  set +e
  OUTPUT="$(cd "$ROOT" && PETAL_PHASE_HANDOFF_TIMEOUT_S=300 bash "$HANDOFF" 2>&1)"
  STATUS=$?
  set -e
  [ "$STATUS" -eq 0 ] || fail "the handoff rejected a CLEAN machine (false positive): $OUTPUT"
  [ $((SECONDS - START)) -lt 10 ] || fail "the handoff waited on a clean machine instead of returning"
  echo "ok  8 - a clean handoff passes immediately, without spending its budget"
fi

echo "test result: e2e-gate handoff contract tests passed"

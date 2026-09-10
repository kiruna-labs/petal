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
  grep -q 'GUARD_SH="scripts/petal-instance-guard.sh"' "$wf" \
    || { echo "missing: the guard is never sourced"; rc=1; }
  [ "$(grep -c '>>> petal-guard availability assertion' "$wf")" -eq 2 ] \
    || { echo "missing: a step relies on the petal_guard_* helpers without asserting they exist"; rc=1; }
  grep -q 'declare -F "$fn"' "$wf" \
    || { echo "missing: the helpers' existence is never actually proved"; rc=1; }
  if grep -q 'pids named above' "$wf"; then
    echo "missing: the timeout still claims 'pids named above' unconditionally"; rc=1
  fi
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
mutate_and_expect '>>> petal-guard availability assertion' \
  'missing: a step relies on the petal_guard_\* helpers without asserting they exist'
mutate_and_expect 'declare -F "\$fn"' 'missing: the helpers'"'"' existence is never actually proved'
# The 'pids named above' rule is a NEGATIVE requirement, so deleting a line can
# never exercise it -- add the phrase back instead and check it is rejected.
ADDED="$TMP/mutant-added.yml"
{ cat "$WORKFLOW"; echo '          # (pids named above)'; } > "$ADDED"
set +e
OUTPUT="$(check_wiring "$ADDED" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -ne 0 ] || fail "re-introducing 'pids named above' was NOT caught"
grep -q "unconditionally" <<<"$OUTPUT" || fail "re-introducing 'pids named above' reported the wrong thing: $OUTPUT"
echo "ok  2 - removing any one of them fails the check, and says which"

# --- Direction 2b: the two availability assertions must stay byte-identical,
# so a fix to one can never silently leave the other lying.
extract_assertion_blocks() {
  awk '/>>> petal-guard availability assertion/ { n++; inblock = 1 }
       inblock { print > ("'"$TMP"'/assertion-" n ".txt") }
       /<<< petal-guard availability assertion/ { inblock = 0 }' "$WORKFLOW"
}
extract_assertion_blocks
[ -s "$TMP/assertion-1.txt" ] && [ -s "$TMP/assertion-2.txt" ] \
  || fail "expected both steps to carry an availability assertion"
cmp -s "$TMP/assertion-1.txt" "$TMP/assertion-2.txt" \
  || fail "the phase-handoff and loopback availability assertions have drifted apart"
echo "ok  2b - both steps carry the same availability assertion, byte for byte"

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

# --------------------------------------------------------------------------
# The "I could not look" direction (#150).
#
# `workflow_dispatch` runs the workflow definition from the DEFAULT BRANCH but
# checks out the ref you name, so a current step can legitimately meet an older
# tree whose scripts/petal-instance-guard.sh predates these helpers. Bash then
# returns 127 for the call, and `if ! helper ...` cannot tell that from a real
# timeout: run 34475294637 printed a confident lingering-instance message and
# "pids named above" having never looked, with no pids above. These directions
# are the point of the whole issue -- a check must distinguish "I looked and
# found a problem" from "I could not look".
# --------------------------------------------------------------------------

# assert_cannot_check <output> <status> <string the message must name>
# The message must say it could not check, name what is missing, and must NOT
# make the lingering-instance claim or cite pids it never saw.
assert_cannot_check() {
  local out="$1" st="$2" needle="$3"
  [ "$st" -eq 1 ] || fail "a missing helper must fail the step, got $st: $out"
  grep -q "CANNOT CHECK" <<<"$out" || fail "a missing helper must say it could not check: $out"
  grep -q -- "$needle" <<<"$out" || fail "the message must name '$needle': $out"
  if grep -q "STILL running" <<<"$out"; then
    fail "a missing helper was reported as a lingering instance -- the exact #150 defect: $out"
  fi
  if grep -q "pids named above" <<<"$out"; then
    fail "the message cites pids it never collected: $out"
  fi
  if grep -q "pid(s):" <<<"$out"; then
    fail "the message cites pids it never collected: $out"
  fi
}

# --- Direction 9: the guard FILE is absent from the checkout.
NOGUARD="$TMP/root-noguard"; mkdir -p "$NOGUARD/scripts"
set +e
OUTPUT="$(cd "$NOGUARD" && PETAL_PHASE_HANDOFF_TIMEOUT_S=2 bash "$HANDOFF" 2>&1)"
STATUS=$?
set -e
assert_cannot_check "$OUTPUT" "$STATUS" "the file itself"
grep -q "workflow_dispatch" <<<"$OUTPUT" \
  || fail "the message must name the likely cause (dispatch runs the default branch's workflow against the ref you name): $OUTPUT"
echo "ok  9 - a checkout with no petal-instance-guard.sh says it could NOT check, not that an instance is stuck"

# --- Direction 10: the guard file exists but predates the handoff helpers --
# exactly the v0.9.21 shape that produced the false report.
OLDROOT="$TMP/root-oldguard"; mkdir -p "$OLDROOT/scripts"
cat > "$OLDROOT/scripts/petal-instance-guard.sh" <<'OLDGUARD'
# The pre-#150 guard: the #846 helpers only, none of the phase-handoff ones.
petal_guard_no_foreign_instance() { return 0; }
petal_guard_kill_pid_verified() { return 0; }
OLDGUARD
set +e
OUTPUT="$(cd "$OLDROOT" && PETAL_PHASE_HANDOFF_TIMEOUT_S=2 bash "$HANDOFF" 2>&1)"
STATUS=$?
set -e
assert_cannot_check "$OUTPUT" "$STATUS" "petal_guard_wait_for_instances_exit"
echo "ok 10 - an older tree's guard names the missing function instead of inventing a lingering instance"

# --- Direction 11: the helpers ARE present and the wait genuinely fails, but
# names no pid. The step may not upgrade that into a pid claim either.
MUTEROOT="$TMP/root-mute-wait"; mkdir -p "$MUTEROOT/scripts"
cat > "$MUTEROOT/scripts/petal-instance-guard.sh" <<'MUTEGUARD'
petal_guard_pid_alive() { return 1; }
petal_guard_live_instances() { :; }
# Fails the way the real helper does, but silently -- no "(pid N) had not
# exited" line for the step to quote.
petal_guard_wait_for_instances_exit() { return 4; }
MUTEGUARD
set +e
OUTPUT="$(cd "$MUTEROOT" && PETAL_PHASE_HANDOFF_TIMEOUT_S=2 bash "$HANDOFF" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -eq 1 ] || fail "a failing wait must fail the step, got $STATUS: $OUTPUT"
grep -q "WITHOUT naming a lingering pid" <<<"$OUTPUT" \
  || fail "a wait that named no pid must be reported as such: $OUTPUT"
if grep -q "pid(s):" <<<"$OUTPUT"; then
  fail "the step claimed pids the wait never produced: $OUTPUT"
fi
echo "ok 11 - a wait that fails without naming a pid is not dressed up as one that did"

# --------------------------------------------------------------------------
# The loopback step's sibling assertion, extracted and executed.
# --------------------------------------------------------------------------
LOOPASSERT="$TMP/loopback-assert.sh"
{
  echo 'set -euo pipefail'
  awk '/GUARD_PHASE="loopback tier"/, /<<< petal-guard availability assertion/' "$WORKFLOW" \
    | sed 's/^          //'
} > "$LOOPASSERT"
grep -q 'petal_guard_diagnose_missing_socket' "$LOOPASSERT" \
  || fail "extraction failed: the loopback assertion does not require the diagnosis helper"
bash -n "$LOOPASSERT" || fail "extracted loopback assertion is not valid bash"

# --- Direction 12: the same older tree stops the loopback tier too, naming the
# diagnosis helper -- rather than silently reading "no instance was running"
# out of a `petal_guard_live_instances` that never ran.
set +e
OUTPUT="$(cd "$OLDROOT" && bash "$LOOPASSERT" 2>&1)"
STATUS=$?
set -e
assert_cannot_check "$OUTPUT" "$STATUS" "petal_guard_diagnose_missing_socket"
grep -q "loopback tier" <<<"$OUTPUT" || fail "the loopback failure must name its own phase: $OUTPUT"
echo "ok 12 - the loopback tier refuses to run blind when its diagnosis helpers are absent"

# --- Direction 13: and the real checkout passes that same assertion, so 9-12
# are not passing because the assertion rejects everything.
set +e
OUTPUT="$(cd "$ROOT" && bash "$LOOPASSERT" 2>&1)"
STATUS=$?
set -e
[ "$STATUS" -eq 0 ] || fail "the availability assertion rejected the REAL checkout: $OUTPUT"
[ -z "$OUTPUT" ] || fail "a satisfied availability assertion must say nothing: $OUTPUT"
echo "ok 13 - the real checkout satisfies the assertion silently"

echo "test result: e2e-gate handoff contract tests passed"

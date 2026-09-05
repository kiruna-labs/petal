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

echo "test result: petal-instance-guard contract tests passed"

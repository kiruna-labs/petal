#!/usr/bin/env bash
# Both-directions contract test for rc-live-suite.sh's foreign-instance guard.
#
# CLAUDE.md, "How to build & verify" rule 8: a gate must be tested in BOTH
# directions before it is relied on. A permission gate written for this repo
# once grepped for a log line that never existed -- it reported "not granted"
# for a condition it had never actually tested, and aborted a healthy run.
#
# The subject is a SYMLINK to /bin/sleep named `desktop`, so `pgrep -x desktop`
# sees a real process with the real name and nothing else is touched. It must be
# a symlink, not a copy: copying a signed Apple binary strips its signature and
# the kernel SIGKILLs it on exec (observed: "Killed: 9"), so the fake would
# vanish before the guard ever ran and the test would pass for the wrong reason.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SUITE="$ROOT/scripts/rc-live-suite.sh"

TMP="$(mktemp -d /tmp/petal-instance-guard-test.XXXXXX)"
FAKE_PID=""
cleanup() {
  [ -n "$FAKE_PID" ] && kill -KILL "$FAKE_PID" 2>/dev/null
  rm -rf "$TMP"
}
trap cleanup EXIT INT TERM
fail() { echo "FAIL: $*" >&2; exit 1; }

# Extract the guard alone. Sourcing the whole suite would run it.
sed -n '/^assert_no_foreign_petal()/,/^}/p' "$SUITE" > "$TMP/guard.sh"
[ -s "$TMP/guard.sh" ] || fail "could not extract assert_no_foreign_petal from the suite"
# shellcheck source=/dev/null
source "$TMP/guard.sh"

# --- Direction 1: NOTHING running -> the guard must PASS.
# This is the half that is usually skipped, and the half that makes a false
# positive abort every healthy run. It also proves the check does not match
# its own command line.
if pgrep -x desktop >/dev/null 2>&1; then
  echo "SKIP: a real \`desktop\` process is running; cannot test the clean case"
else
  # In a SUBSHELL: the guard exits rather than returning, so calling it inline
  # would abort this script with the guard's own status and no verdict of ours
  # -- a test that dies quietly is indistinguishable from one that passed.
  set +e
  CLEAN_OUTPUT="$( assert_no_foreign_petal 2>&1 )"
  CLEAN_STATUS=$?
  set -e
  [ "$CLEAN_STATUS" -eq 0 ] \
    || fail "guard rejected a CLEAN machine (false positive, status $CLEAN_STATUS): $CLEAN_OUTPUT"
  echo "ok  1 - a clean machine passes the guard"
fi

# --- Direction 2: a process literally named `desktop` -> the guard must REFUSE.
ln -s /bin/sleep "$TMP/desktop"
"$TMP/desktop" 120 &
FAKE_PID=$!
for _ in $(seq 1 40); do
  pgrep -x desktop >/dev/null 2>&1 && break
  sleep 0.05
done
pgrep -x desktop >/dev/null 2>&1 || fail "test setup: the fake \`desktop\` never appeared to pgrep"

set +e
OUTPUT="$( assert_no_foreign_petal 2>&1 )"
STATUS=$?
set -e
[ "$STATUS" -eq 3 ] || fail "guard must exit 3 when a foreign instance is running, got $STATUS"
grep -q "already running" <<<"$OUTPUT" || fail "guard must say what it found: $OUTPUT"
grep -q "$FAKE_PID" <<<"$OUTPUT" || fail "guard must name the offending PID so it can be killed BY PID: $OUTPUT"
echo "ok  2 - a foreign \`desktop\` is refused, by pid, with a usable message"

kill -KILL "$FAKE_PID" 2>/dev/null
FAKE_PID=""
echo "test result: rc-live-suite instance-guard contract tests passed"

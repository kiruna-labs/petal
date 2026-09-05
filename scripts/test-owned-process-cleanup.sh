#!/usr/bin/env bash
# Contract tests for scripts/owned-process-cleanup.sh (plan Item 7). No GUI, no
# display, no remote machine -- every subject is a `sleep` this test started.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIB="$ROOT/scripts/owned-process-cleanup.sh"

TMP_ROOT="$(mktemp -d /tmp/petal-owned-cleanup-test.XXXXXX)"
TMPDIR_CHILD_FILE="$TMP_ROOT/child.pid"
: >"$TMPDIR_CHILD_FILE"
trap 'rm -rf "$TMP_ROOT"' EXIT INT TERM

fail() { echo "FAIL: $*" >&2; exit 1; }
alive() { kill -0 "$1" 2>/dev/null; }
wait_gone() {
  local pid="$1" i
  for ((i = 0; i < 60; i++)); do
    alive "$pid" || return 0
    sleep 0.05
  done
  return 1
}

# 1. An owned process is terminated and verified.
(
  # shellcheck source=scripts/owned-process-cleanup.sh
  source "$LIB"
  sleep 120 &
  OWNED="$!"
  own_process "$OWNED"
  alive "$OWNED" || fail "owned process did not start"
  OUTPUT="$(release_owned_processes)"
  wait_gone "$OWNED" || fail "owned process survived cleanup"
  grep -q "nothing this script started is still alive" <<<"$OUTPUT" \
    || fail "cleanup did not report a clean teardown: $OUTPUT"
)

# 2. A process this script did NOT own is never signalled. livekit-server, a
#    vite server and a CDP Chrome are all reuse-if-present and may belong to
#    another agent's session.
(
  source "$LIB"
  sleep 120 &
  FOREIGN="$!"
  sleep 120 &
  OWNED="$!"
  own_process "$OWNED"
  release_owned_processes >/dev/null
  wait_gone "$OWNED" || fail "owned process survived cleanup"
  alive "$FOREIGN" || fail "cleanup killed a process it did not own"
  kill -KILL "$FOREIGN" 2>/dev/null || true
)

# 3. An already-exited pid is reported, not treated as a failure.
(
  source "$LIB"
  sleep 120 &
  OWNED="$!"
  own_process "$OWNED"
  kill -KILL "$OWNED" 2>/dev/null || true
  wait_gone "$OWNED" || fail "setup: process did not die"
  OUTPUT="$(release_owned_processes)"
  grep -q "pid $OWNED already exited" <<<"$OUTPUT" || fail "missing already-exited report: $OUTPUT"
)

# 4. A group leader is signalled as a GROUP, so descendants go too. Job control
#    (set -m) puts each background job in its own process group, which is the
#    shell equivalent of Node's `detached: true`.
(
  source "$LIB"
  set -m
  # A leader with one child. Killing only the leader would leave the child.
  bash -c 'sleep 120 & echo "$!" >"$1"; sleep 120' _ "$TMPDIR_CHILD_FILE" &
  LEADER="$!"
  set +m
  for _ in $(seq 1 60); do
    [ -s "$TMPDIR_CHILD_FILE" ] && break
    sleep 0.05
  done
  CHILD="$(cat "$TMPDIR_CHILD_FILE")"
  [ -n "$CHILD" ] || fail "group child never reported its pid"
  alive "$CHILD" || fail "group child did not start"
  [ "$(owned_signal_target "$LEADER")" = "-$LEADER" ] \
    || fail "a job-control leader must be signalled as a group"
  own_process "$LEADER"
  release_owned_processes >/dev/null
  wait_gone "$LEADER" || fail "group leader survived cleanup"
  wait_gone "$CHILD" || fail "group member survived cleanup -- the leak this closes"
)

# 5. A non-leader is signalled by pid, never as -pid (which would be this
#    script's own group).
(
  source "$LIB"
  sleep 120 &
  OWNED="$!"
  [ "$(owned_signal_target "$OWNED")" = "$OWNED" ] \
    || fail "a non-leader must be signalled by pid, not by group"
  kill -KILL "$OWNED" 2>/dev/null || true
)

# 6. #798: the leak this file's case 4 does NOT cover. A wrapper that is NOT a
#    group leader (the real shape: `npm run dev:clean` spawned without job
#    control) execs a chain ending in a long-lived grandchild. Killing the
#    wrapper alone reparents the grandchild to PID 1 -- and cleanup printed
#    "nothing this script started is still alive" while `target/debug/desktop`
#    kept running. Reproduced 7 times live. Two independent closures are
#    asserted here: the descendant snapshot taken BEFORE any signal, and
#    `owned_spawn_group` giving the tree a signallable process group.
GRANDCHILD_FILE="$TMP_ROOT/grandchild.pid"
WRAPPER="$TMP_ROOT/wrapper.sh"
cat >"$WRAPPER" <<WRAPPER_EOF
#!/usr/bin/env bash
exec bash -c 'sleep 120 & echo \$! > "$GRANDCHILD_FILE"; wait'
WRAPPER_EOF
chmod +x "$WRAPPER"

await_grandchild() {
  local i
  for ((i = 0; i < 100; i++)); do
    [ -s "$GRANDCHILD_FILE" ] && { cat "$GRANDCHILD_FILE"; return 0; }
    sleep 0.05
  done
  return 1
}

# 6a. Plain `own_process` on a non-leader wrapper: the descendant snapshot must
#     still reach the grandchild.
(
  source "$LIB"
  : >"$GRANDCHILD_FILE"
  "$WRAPPER" >/dev/null 2>&1 &
  WRAP_PID="$!"
  GRANDCHILD="$(await_grandchild)" || fail "grandchild never reported its pid"
  alive "$GRANDCHILD" || fail "grandchild did not start"
  [ "$(owned_signal_target "$WRAP_PID")" = "$WRAP_PID" ] \
    || fail "precondition: this wrapper must NOT be a group leader, or the case is not #798"
  own_process "$WRAP_PID"
  OUTPUT="$(release_owned_processes)"
  wait_gone "$GRANDCHILD" \
    || fail "#798: grandchild $GRANDCHILD survived cleanup -- reparented to PID 1 and lost"
  grep -q "descendant pid(s) recorded before signalling" <<<"$OUTPUT" \
    || fail "#798: descendants must be snapshotted before any signal: $OUTPUT"
)

# 6b. `owned_spawn_group` makes the same wrapper a group leader, so the whole
#     tree is reachable by a single group signal.
(
  source "$LIB"
  : >"$GRANDCHILD_FILE"
  owned_spawn_group "$WRAPPER" >/dev/null 2>&1
  WRAP_PID="$OWNED_SPAWN_PID"
  GRANDCHILD="$(await_grandchild)" || fail "grandchild never reported its pid under owned_spawn_group"
  [ "$(owned_signal_target "$WRAP_PID")" = "-$WRAP_PID" ] \
    || fail "owned_spawn_group must produce a process-group leader"
  release_owned_processes >/dev/null
  wait_gone "$WRAP_PID" || fail "wrapper survived cleanup"
  wait_gone "$GRANDCHILD" || fail "grandchild survived a group signal"
)

# 6c. The all-clear line must be earned. It may only print when nothing owned
#     OR descended survived -- a false all-clear is what made #798 invisible.
(
  source "$LIB"
  : >"$GRANDCHILD_FILE"
  "$WRAPPER" >/dev/null 2>&1 &
  own_process "$!"
  GRANDCHILD="$(await_grandchild)" || fail "grandchild never reported its pid"
  OUTPUT="$(release_owned_processes)"
  grep -q "nothing this script started is still alive" <<<"$OUTPUT" \
    || fail "a genuinely clean release must report the all-clear: $OUTPUT"
  alive "$GRANDCHILD" && fail "all-clear printed while grandchild $GRANDCHILD was alive"
  true
)

echo "test result: owned-process cleanup contract tests passed"

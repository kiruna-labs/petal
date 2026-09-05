#!/usr/bin/env bash
# Sourceable cleanup for background processes a script STARTED ITSELF.
#
# Two rules this encodes, both learned the hard way on this repo:
#
# 1. Only signal what you started. livekit-server, a vite dev server and a CDP
#    Chrome are reuse-if-present services; another agent's session may own the
#    one you found running. Nothing is signalled unless it was passed to
#    own_process, and own_process is only ever called on a pid this script
#    just spawned.
# 2. Never signal a pattern. `pkill -f <pattern>` matches the killer's own
#    command line -- that is how a CI run once killed itself. Every signal here
#    goes to a numeric pid (or to -pid, and only once `ps` has confirmed that
#    pid really leads its own process group).
#
# Cleanup verifies rather than assumes: SIGTERM, wait, SIGKILL, wait, and then
# print one line per survivor. `( cmd & echo $! )` records the SUBSHELL's pid,
# not the command's -- use `( cd dir && exec cmd ) &` so `$!` is the real one.
#
# 3. #798: owning the pid you spawned is NOT enough when that pid is a wrapper.
#    `npm run dev:clean` execs a chain ending in `target/debug/desktop`; kill
#    only the wrapper and the real binary is reparented to PID 1 and survives,
#    while cleanup cheerfully prints "nothing this script started is still
#    alive". Reproduced 7 times. So: snapshot each owned pid's DESCENDANTS
#    before signalling anything (after the parent dies the ppid link is gone
#    and they are unrecoverable except by pattern, which rule 2 forbids), and
#    spawn under `owned_spawn_group` so the whole tree shares a signallable
#    process group.

OWNED_PIDS=()

# Run a background spawn with job control on, so the job leads its own process
# group and `owned_signal_target` can signal the entire tree with `-pid`.
# Without this a non-interactive bash puts background jobs in the SCRIPT's
# group, so `-pid` would signal the script itself and cleanup falls back to
# the bare pid -- which is exactly how #798's grandchild escaped.
owned_spawn_group() {
  local had_monitor=1
  case "$-" in *m*) ;; *) had_monitor=0 ;; esac
  set -m
  "$@" &
  OWNED_SPAWN_PID=$!
  [ "$had_monitor" -eq 1 ] || set +m
  own_process "$OWNED_SPAWN_PID"
}

# Every descendant of $1, deepest-last, by walking ppid links in one ps pass.
# Numeric pids only -- never a pattern.
owned_descendants() {
  local root="$1" table frontier next pid parent child
  table="$(ps -eo pid=,ppid= 2>/dev/null || true)"
  [ -n "$table" ] || return 0
  frontier="$root"
  while [ -n "$frontier" ]; do
    next=""
    for pid in $frontier; do
      while read -r child parent; do
        [ "$parent" = "$pid" ] || continue
        [ "$child" = "$root" ] && continue
        echo "$child"
        next="$next $child"
      done <<<"$table"
    done
    frontier="$next"
  done
}

own_process() {
  local pid="$1"
  if [[ ! "$pid" =~ ^[0-9]+$ ]] || [ "$pid" -le 1 ]; then
    echo "== cleanup: refusing to own non-pid '$pid' ==" >&2
    return 1
  fi
  OWNED_PIDS+=("$pid")
}

# Signal the process GROUP only when the pid genuinely leads one (a job started
# under job control, or a detached spawn). Otherwise the group is this script's
# own, and -pid would signal the script and its siblings.
owned_signal_target() {
  local pid="$1" pgid
  pgid="$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ')"
  if [ -n "$pgid" ] && [ "$pgid" = "$pid" ]; then
    printf -- '-%s' "$pid"
  else
    printf -- '%s' "$pid"
  fi
}

owned_wait_for_exit() {
  local pid="$1" attempts="${2:-40}" i
  for ((i = 0; i < attempts; i++)); do
    kill -0 "$pid" 2>/dev/null || return 0
    sleep 0.05
  done
  ! kill -0 "$pid" 2>/dev/null
}

# SIGTERM, wait, SIGKILL, wait. Returns 0 if the pid is gone, 1 if it survived.
owned_release_one() {
  local pid="$1" label="$2" target
  kill -0 "$pid" 2>/dev/null || return 0
  target="$(owned_signal_target "$pid")"
  echo "== cleanup: SIGTERM $target ($label $pid) =="
  kill -TERM "$target" 2>/dev/null || true
  if owned_wait_for_exit "$pid"; then
    echo "== cleanup: $label $pid exited on SIGTERM =="
    return 0
  fi
  echo "== cleanup: SIGKILL $target ($label $pid) =="
  kill -KILL "$target" 2>/dev/null || true
  if owned_wait_for_exit "$pid"; then
    echo "== cleanup: $label $pid exited on SIGKILL =="
    return 0
  fi
  echo "== WARN cleanup: $label $pid SURVIVED SIGKILL ==" >&2
  return 1
}

release_owned_processes() {
  local pid child survivors=0
  local -a descendants=()
  # #798: snapshot descendants BEFORE any signal. Once the wrapper dies its
  # children reparent to PID 1 and the only way left to find them is a
  # pattern match, which rule 2 forbids for good reason.
  for pid in ${OWNED_PIDS[@]+"${OWNED_PIDS[@]}"}; do
    kill -0 "$pid" 2>/dev/null || continue
    while read -r child; do
      [ -n "$child" ] && descendants+=("$child")
    done < <(owned_descendants "$pid")
  done
  if [ "${#descendants[@]}" -gt 0 ]; then
    echo "== cleanup: ${#descendants[@]} descendant pid(s) recorded before signalling: ${descendants[*]} =="
  fi
  for pid in ${OWNED_PIDS[@]+"${OWNED_PIDS[@]}"}; do
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "== cleanup: pid $pid already exited =="
      continue
    fi
    owned_release_one "$pid" "pid" || survivors=$((survivors + 1))
  done
  # Anything the group signal above did not reach -- e.g. a grandchild that
  # called setsid, or one whose parent exited before the group was signalled.
  for pid in ${descendants[@]+"${descendants[@]}"}; do
    kill -0 "$pid" 2>/dev/null || continue
    echo "== cleanup: descendant $pid outlived its parent; releasing it too =="
    owned_release_one "$pid" "descendant" || survivors=$((survivors + 1))
  done
  if [ "$survivors" -eq 0 ]; then
    echo "== cleanup: nothing this script started is still alive (checked ${#OWNED_PIDS[@]-0} owned + ${#descendants[@]} descendant pid(s)) =="
  fi
  return 0
}

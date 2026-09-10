#!/usr/bin/env bash
# Shared "refuse, don't kill" foreign-Petal-instance guard (#839's pattern,
# applied repo-wide by #846 after RC verification scripts SIGKILLed a user's
# live installed Petal.app four times in 90 minutes -- see CLAUDE.md "Sharing
# this machine" and internal/docs/ISSUE_WORKFLOW.md).
#
# Two rules:
# 1. Detect a foreign/live Petal BEFORE doing anything destructive and REFUSE
#    to proceed, rather than clearing the way for it (CLAUDE.md gate rule:
#    the guard must live in the acting script, not in agent briefs).
# 2. Kill only PIDs the script itself recorded starting, verified by `ps -p`
#    against an expected command-line substring -- never a bare `pkill -f`
#    pattern, which can match an unrelated Petal.app whose command line
#    happens to contain the same substring (the actual #846 root cause: an
#    unanchored `pkill -f "Petal.app/Contents/MacOS/desktop"` matches
#    /Applications/Petal.app just as well as the intended QA/dev bundle).
#
# Sourced by scripts/verify-t0-battery.sh and scripts/verify-window-classification.sh.

# petal_guard_no_foreign_instance <space-separated allowlist of PIDs>
# Returns 0 if every "Contents/MacOS/desktop" process currently running is in
# the allowlist (or nothing is running); returns 3 and prints a FATAL message
# naming the offending PID(s) otherwise. Caller decides whether to exit.
petal_guard_no_foreign_instance() {
  local -a allowed=($1)
  local pid allow_pid is_allowed
  local -a foreign=()
  while read -r pid; do
    [ -z "$pid" ] && continue
    is_allowed=0
    for allow_pid in ${allowed[@]+"${allowed[@]}"}; do
      [ "$pid" = "$allow_pid" ] && is_allowed=1 && break
    done
    [ "$is_allowed" -eq 1 ] && continue
    kill -0 "$pid" 2>/dev/null || continue
    foreign+=("$pid")
  done < <({ pgrep -f "Contents/MacOS/desktop" 2>/dev/null; pgrep -f "target/debug/desktop" 2>/dev/null; } | sort -u || true)
  [ "${#foreign[@]}" -eq 0 ] && return 0
  echo "FATAL: a Petal instance is already running -- not mine. Refusing to proceed." >&2
  for pid in "${foreign[@]}"; do
    ps -p "$pid" -o pid=,etime=,command= 2>/dev/null | sed 's/^/       /' >&2
  done
  return 3
}

# petal_guard_kill_pid_verified <pid> <expected command-line substring>
# SIGTERM then SIGKILL a single pid, but only after confirming (via `ps -p`,
# never a pattern match against the whole process table) that its command
# line actually contains the expected substring. Refuses silently-safely
# (warns, does not kill) if the pid's command line doesn't match -- e.g. it
# already exited and the pid was recycled by an unrelated process.
petal_guard_kill_pid_verified() {
  local pid="$1" expected_substr="$2" cmd
  [ -n "$pid" ] || return 0
  kill -0 "$pid" 2>/dev/null || return 0
  cmd="$(ps -p "$pid" -o command= 2>/dev/null || true)"
  case "$cmd" in
    *"$expected_substr"*) ;;
    *)
      echo "WARN: refusing to kill pid $pid -- command line does not match expected '$expected_substr': ${cmd:-<gone>}" >&2
      return 1
      ;;
  esac
  kill "$pid" 2>/dev/null || true
  for _ in $(seq 1 20); do
    kill -0 "$pid" 2>/dev/null || return 0
    sleep 0.1
  done
  kill -KILL "$pid" 2>/dev/null || true
  return 0
}

# --------------------------------------------------------------------------
# Phase-handoff helpers (#150).
#
# Two consecutive live phases (Test Cockpit, then the remote-control loopback)
# each run their own Petal. They are the SAME single-instance app: if the
# first is still tearing down when the second launches,
# `tauri-plugin-single-instance` hands the new process's argv to the old one
# and the new process exits -- after which the second phase waits its whole
# socket budget for a socket that will never be opened, and reports only "did
# not open $SOCKET within 600s".
#
# Everything below waits and decides BY PID (`ps -p`), never by re-running a
# `pgrep -f` pattern in a loop: `pgrep -f` matches the watcher's own command
# line, so it is wrong in BOTH directions -- it has reported a finished job as
# running and a killed process as alive (CLAUDE.md, "How to build & verify"
# rule 5). A pattern is used ONCE, to discover candidates; every subsequent
# decision about a candidate is made against its pid.
# --------------------------------------------------------------------------

# petal_guard_pid_alive <pid>
# True only while the pid names a live, non-zombie process. A zombie counts as
# exited: a child the caller has not reaped yet still answers `ps -p`, and
# treating that as "still running" would hang every wait below for its full
# budget.
petal_guard_pid_alive() {
  local pid="$1" state
  [ -n "$pid" ] || return 1
  state="$(ps -p "$pid" -o state= 2>/dev/null | tr -d '[:space:]')"
  case "$state" in
    "" | Z*) return 1 ;;
    *) return 0 ;;
  esac
}

# petal_guard_live_instances [pid_to_exclude ...]
# Prints, one per line, the pid of every live Petal-ish process on this
# machine. Discovery only -- the caller decides what to do with each pid, and
# every later check is by pid. The calling shell and its parent are always
# excluded so a caller whose own command line contains one of the patterns
# cannot match itself.
# shellcheck disable=SC2120  # the excludes are optional by design
petal_guard_live_instances() {
  local -a exclude=("$$" "${PPID:-0}" "$@")
  local pid ex skip
  while read -r pid; do
    [ -z "$pid" ] && continue
    skip=0
    for ex in ${exclude[@]+"${exclude[@]}"}; do
      [ "$pid" = "$ex" ] && skip=1 && break
    done
    [ "$skip" -eq 1 ] && continue
    petal_guard_pid_alive "$pid" || continue
    printf '%s\n' "$pid"
  done < <({
    pgrep -f "Contents/MacOS/desktop" 2>/dev/null
    pgrep -f "target/debug/desktop" 2>/dev/null
    pgrep -f "target/release/desktop" 2>/dev/null
    pgrep -x desktop 2>/dev/null
  } | sort -u || true)
}

# petal_guard_wait_for_pid_exit <pid> <timeout_s> [label]
# Returns 0 as soon as the pid is gone (or was never alive), 4 if it is still
# alive after the budget -- printing a FATAL that names the pid, the budget,
# and the process's own `ps` line, so the failure carries its cause.
petal_guard_wait_for_pid_exit() {
  local pid="$1" timeout_s="${2:-120}" label="${3:-a Petal instance}"
  local waited=0
  [ -n "$pid" ] || return 0
  while petal_guard_pid_alive "$pid"; do
    if [ "$waited" -ge "$timeout_s" ]; then
      echo "FATAL: $label (pid $pid) had not exited after ${timeout_s}s." >&2
      ps -p "$pid" -o pid=,state=,etime=,command= 2>/dev/null | sed 's/^/       /' >&2
      return 4
    fi
    sleep 1
    waited=$((waited + 1))
  done
  if [ "$waited" -gt 0 ]; then
    echo "petal-guard: $label (pid $pid) exited after ${waited}s"
  fi
  return 0
}

# petal_guard_wait_for_instances_exit <timeout_s> [pid ...]
# Waits for every named pid to exit; with no pids, discovers the live
# instances once and waits for those. Returns 0 when the machine is clear, 4
# if the shared budget expires with something still alive.
petal_guard_wait_for_instances_exit() {
  local timeout_s="${1:-120}"
  shift || true
  local -a pids=("$@")
  local pid started remaining rc=0
  if [ "${#pids[@]}" -eq 0 ]; then
    while read -r pid; do
      [ -n "$pid" ] && pids+=("$pid")
    done < <(petal_guard_live_instances)
  fi
  if [ "${#pids[@]}" -eq 0 ]; then
    echo "petal-guard: no Petal instance is running -- the phase handoff is clear"
    return 0
  fi
  echo "petal-guard: waiting up to ${timeout_s}s for Petal instance(s) to exit: ${pids[*]}"
  for pid in "${pids[@]}"; do
    ps -p "$pid" -o pid=,state=,etime=,command= 2>/dev/null | sed 's/^/       /'
  done
  started="$SECONDS"
  for pid in "${pids[@]}"; do
    remaining=$((timeout_s - (SECONDS - started)))
    [ "$remaining" -lt 0 ] && remaining=0
    petal_guard_wait_for_pid_exit "$pid" "$remaining" "a Petal instance" || rc=4
  done
  [ "$rc" -eq 0 ] && echo "petal-guard: every tracked Petal instance has exited"
  return "$rc"
}

# petal_guard_diagnose_missing_socket <socket> <launcher_pid> <crash_dir> <crash_mark> [pre_launch_pid ...]
# Says WHY an autotest socket never appeared. "did not open $SOCKET" on its
# own is shared by four different failures; this names which one, on stdout,
# as a stable `petal-guard: cause=<token>` line plus the evidence for it.
# Returns a distinct status per cause:
#   2  single-instance-lock-held   an instance that predates this launch is
#                                  STILL alive, so ours forwarded and exited
#   3  petal-crashed               a fresh crash report exists
#   4  launcher-exited             the launching process is gone
#   5  no-petal-process            nothing ever came up
#   6  running-but-no-socket       alive, but never opened the socket
petal_guard_diagnose_missing_socket() {
  local socket="$1" launcher_pid="$2" crash_dir="$3" crash_mark="$4"
  shift 4
  local -a pre_launch=("$@")
  local pid crashes live
  local -a stale=()

  echo "petal-guard: diagnosing why ${socket} never appeared"

  for pid in ${pre_launch[@]+"${pre_launch[@]}"}; do
    [ -n "$pid" ] || continue
    petal_guard_pid_alive "$pid" && stale+=("$pid")
  done
  if [ "${#stale[@]}" -gt 0 ]; then
    echo "petal-guard: cause=single-instance-lock-held"
    echo "petal-guard: a Petal instance that predates this launch is STILL RUNNING (pid(s): ${stale[*]})."
    echo "petal-guard: tauri-plugin-single-instance forwards a second instance's argv to that one and exits,"
    echo "petal-guard: so the socket this phase is waiting for was never going to be opened."
    for pid in "${stale[@]}"; do
      ps -p "$pid" -o pid=,state=,etime=,command= 2>/dev/null | sed 's/^/       /'
    done
    return 2
  fi

  if [ -n "$crash_dir" ] && [ -e "$crash_mark" ]; then
    crashes="$(find "$crash_dir" -maxdepth 1 -name 'desktop*.ips' -newer "$crash_mark" 2>/dev/null || true)"
    if [ -n "$crashes" ]; then
      echo "petal-guard: cause=petal-crashed"
      echo "petal-guard: Petal wrote a crash report after this phase started:"
      echo "$crashes" | sed 's/^/       /'
      return 3
    fi
  fi

  if ! petal_guard_pid_alive "$launcher_pid"; then
    echo "petal-guard: cause=launcher-exited"
    echo "petal-guard: the process that launched Petal (pid ${launcher_pid:-<none>}) is gone; it never got as far as a socket."
    echo "petal-guard: read the launcher's own log (a build failure, or an instance that forwarded and exited immediately)."
    return 4
  fi

  live="$(petal_guard_live_instances | tr '\n' ' ')"
  if [ -z "${live// /}" ]; then
    echo "petal-guard: cause=no-petal-process"
    echo "petal-guard: the launcher (pid $launcher_pid) is alive but no Petal process is running at all."
    return 5
  fi

  echo "petal-guard: cause=running-but-no-socket"
  echo "petal-guard: Petal is running (pid(s): ${live% }) but never opened ${socket} -- it is up and not answering."
  for pid in $live; do
    ps -p "$pid" -o pid=,state=,etime=,command= 2>/dev/null | sed 's/^/       /'
  done
  return 6
}

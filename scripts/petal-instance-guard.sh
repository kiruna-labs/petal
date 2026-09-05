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

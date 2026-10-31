#!/usr/bin/env bash
# #436: the Tart runner's job hand-off to the guest agent, against a fake
# `tart`. Sources the hand-off block of scripts/runner/tart/ephemeral-runner.sh
# (the host runs a single copied file, so it lives inline there).
#   - the launchd-state parser, both directions, on real `launchctl print` shapes
#   - job.env is only delivered once the boot-time agent run has finished
#   - the agent is started WITHOUT -k (never kill one that holds the job)
#   - no listener -> re-deliver with a fresh token; give up after 3 attempts
# Variables below are read by the eval-ed block and inside check strings.
# shellcheck disable=SC2034
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNNER="$ROOT/scripts/runner/tart/ephemeral-runner.sh"
fail=0
check() { if eval "$2"; then echo "ok   $1"; else echo "FAIL $1"; fail=1; fi; }

block="$(sed -n '/^# ---- job hand-off to the guest agent/,/^# ---- end job hand-off/p' "$RUNNER")"
[ -n "$block" ] || { echo "FAIL could not find the hand-off block in $RUNNER"; exit 1; }
REPO=kiruna-labs/petal LABELS=self-hosted,macOS,tart
PETAL_RUNNER_AGENT_SETTLE_TIMEOUT_S=4 PETAL_RUNNER_LISTENER_WAIT_S=3 PETAL_RUNNER_HANDOFF_POLL_S=1
eval "$block"
sleep() { :; }
say() { echo "say: $*" >> "$T/events"; }

# Top-level keys are one tab deep; nested endpoint states are two (real shape).
running=$'com.petal.tart-runner = {\n\tactive count = 1\n\tstate = running\n\truns = 1\n\tpid = 440\n\tendpoints = {\n\t\tstate = active\n\t}\n}'
idle=$'com.petal.tart-runner = {\n\tactive count = 0\n\tstate = not running\n\truns = 1\n\tendpoints = {\n\t\tstate = active\n\t}\n}'
never=$'com.petal.tart-runner = {\n\tactive count = 0\n\tstate = not running\n\truns = 0\n}'
check "parser: a finished run is settled" 'agent_run_settled <<<"$idle"'
check "parser: a running agent is not" '! agent_run_settled <<<"$running"'
check "parser: an agent that never ran is not" '! agent_run_settled <<<"$never"'
check "parser: no output is not" '! agent_run_settled </dev/null'

# Fake guest. running_polls: launchctl reports the boot-time run as running
# this many more times. kicks_to_listener: kickstarts until a listener is up
# (0 = never).
tart() {
  [ "$1" = exec ] || return 1; shift
  [ "$1" = -i ] && shift
  shift # guest name
  local cmd="$*" n
  case "$cmd" in
    *"launchctl print"*)
      n="$(cat "$T/running_polls")"
      if [ "$n" -gt 0 ]; then echo $((n - 1)) > "$T/running_polls"; echo "$running"; else echo "$idle"; fi ;;
    *"cat > "*) cat > "$T/job.env"; echo "deliver running_polls=$(cat "$T/running_polls")" >> "$T/events" ;;
    *"grep -q"*) grep -q '^RUNNER_TOKEN=.' "$T/job.env" ;;
    "id -u") echo 501 ;;
    "launchctl kickstart"*)
      echo "kickstart ${cmd#launchctl kickstart }" >> "$T/events"
      n="$(cat "$T/kicks_to_listener")"
      if [ "$n" -eq 1 ]; then touch "$T/listener"; fi
      [ "$n" -gt 0 ] && echo $((n - 1)) > "$T/kicks_to_listener"
      return 0 ;;
    "pgrep -x Runner.Listener") [ -f "$T/listener" ] ;;
    *) echo "unexpected tart exec: $cmd" >&2; return 1 ;;
  esac
}
mint_registration_token() { echo "mint" >> "$T/events"; echo "tok-$RANDOM"; }
scenario() { T="$(mktemp -d)"; echo "$1" > "$T/running_polls"; echo "$2" > "$T/kicks_to_listener"; : > "$T/events"; }

scenario 2 1
hand_off_job guest; rc=$?
check "boot agent mid-probe: hand-off succeeds" '[ "$rc" -eq 0 ]'
check "boot agent mid-probe: job.env waits for the run to finish" 'grep -qx "deliver running_polls=0" "$T/events"'
check "kickstart has no -k" 'grep -qx "kickstart gui/501/com.petal.tart-runner" "$T/events"'
check "one token for one hand-off" '[ "$(grep -c "^mint$" "$T/events")" -eq 1 ]'

scenario 0 2
hand_off_job guest; rc=$?
check "no listener after the first hand-off: re-delivered and succeeded" '[ "$rc" -eq 0 ] && [ "$(grep -c "^mint$" "$T/events")" -eq 2 ]'

scenario 0 0
hand_off_job guest; rc=$?
check "listener never starts: gives up after 3 hand-offs" '[ "$rc" -eq 1 ] && [ "$(grep -c "^mint$" "$T/events")" -eq 3 ]'

scenario 99 1
hand_off_job guest; rc=$?
check "agent never settles: delivers anyway after the timeout, and says so" '[ "$rc" -eq 0 ] && grep -q "still running after 4s; delivering anyway" "$T/events"'

[ "$fail" -eq 0 ] && echo "tart runner hand-off: all checks passed"
exit "$fail"

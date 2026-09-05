#!/usr/bin/env bash
# Host-side loop: keep exactly one EPHEMERAL Petal runner alive inside a
# fresh clone of the golden Tart guest. Each clone registers with a
# single-use token, runs ONE job, and is deleted -- nothing a job does
# survives into the next one, and nothing on this host is mounted or
# reachable through the filesystem.
#
#   PETAL_RUNNER_REPO=owner/repo scripts/runner/tart/ephemeral-runner.sh [--once] [--softnet] [--always]
#
# --once     run a single clone/job cycle and exit (first-time verification)
# --softnet  isolate the guest from the host LAN with Softnet. Needs a
#            passwordless sudo rule (Tart invokes softnet as root):
#              echo "%admin ALL=(root) NOPASSWD: /opt/homebrew/bin/softnet, $(readlink -f /opt/homebrew/bin/softnet)" \
#                | sudo tee /etc/sudoers.d/softnet
#            Verified at startup; falls back to NAT with a warning if absent.
# --always   keep one idle guest registered at all times (previous behaviour).
#            Default is ON-DEMAND: no VM exists until a queued job on the repo
#            asks for a self-hosted runner; then clone, run it, delete, and
#            go back to polling. Costs ~1 min of latency, saves ~16 GB of RAM
#            and a permanently registered runner.
#
# Needs: tart, gh (authenticated as a repo admin -- registration tokens),
# the golden guest from make-golden.sh. Installed as a LaunchAgent by
# install-host-agent.sh so it survives logouts of the terminal.
set -uo pipefail

REPO="${PETAL_RUNNER_REPO:?PETAL_RUNNER_REPO=owner/repo is required}"
GOLDEN="${PETAL_RUNNER_GOLDEN:-petal-runner-golden}"
LABELS="${PETAL_RUNNER_LABELS:-self-hosted,macOS,tart}"
JOB_TIMEOUT_MIN="${PETAL_RUNNER_JOB_TIMEOUT_MIN:-180}"
LOG_DIR="${PETAL_RUNNER_LOG_DIR:-$HOME/Library/Logs/petal-tart-runner}"
ONCE=0; SOFTNET=0; ALWAYS=0
POLL_S="${PETAL_RUNNER_POLL_SECONDS:-30}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --once) ONCE=1; shift ;;
    --softnet) SOFTNET=1; shift ;;
    --always) ALWAYS=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
mkdir -p "$LOG_DIR"
say() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$*"; }

CURRENT=""
cleanup_vm() {
  local vm="$1"
  [ -n "$vm" ] || return 0
  say "cleanup $vm"
  tart stop "$vm" >/dev/null 2>&1 || true
  sleep 2
  tart delete "$vm" >/dev/null 2>&1 || true
  # never leave a half-registered runner behind on the repo
  local id
  id="$(gh api "repos/$REPO/actions/runners" --jq ".runners[] | select(.name==\"$vm\") | .id" 2>/dev/null | head -1)"
  [ -n "$id" ] && gh api -X DELETE "repos/$REPO/actions/runners/$id" >/dev/null 2>&1 || true
}
trap 'cleanup_vm "$CURRENT"; exit 130' INT TERM

# install-host-agent.sh runs a COPY of this file and tells us where the
# original lives; exit when it changes so launchd's KeepAlive relaunches the
# new version. Editing a script bash is executing corrupts that run, so the
# copy is what runs and the original is only ever compared by mtime. Sibling
# scripts (make-golden.sh) are resolved next to the ORIGINAL, not the copy.
SELF_SRC="${PETAL_RUNNER_SELF_SOURCE:-}"
HERE="$(cd "$(dirname "${SELF_SRC:-${BASH_SOURCE[0]}}")" && pwd)"
SELF_MTIME="$( [ -n "$SELF_SRC" ] && stat -f %m "$SELF_SRC" 2>/dev/null || echo 0 )"
self_changed() { [ -n "$SELF_SRC" ] && [ "$(stat -f %m "$SELF_SRC" 2>/dev/null || echo 0)" != "$SELF_MTIME" ]; }

MARK="$HOME/Library/Application Support/petal-tart-runner/$GOLDEN.provisioned"
if [ ! -d "$HOME/.tart/vms/$GOLDEN" ] || [ ! -f "$MARK" ]; then   # dir, not `tart list` (flakes while a VM stops)
  say "golden guest '$GOLDEN' missing or never finished provisioning -- building it (make-golden.sh --headless)"
  if ! bash "$HERE/make-golden.sh" --headless --name "$GOLDEN"; then
    say "golden build failed; see above. Retrying in 10 min (fix the script and the loop restarts itself)"
    for _ in $(seq 1 60); do self_changed && exit 0; sleep 10; done
    exit 1
  fi
fi

if [ "$SOFTNET" -eq 1 ]; then
  SOFTNET_BIN="$(command -v softnet || true)"
  if [ -n "$SOFTNET_BIN" ] && sudo -n "$SOFTNET_BIN" --help >/dev/null 2>&1; then
    say "softnet: enabled ($SOFTNET_BIN, passwordless sudo verified)"
  else
    say "softnet: requested but no passwordless sudo rule for ${SOFTNET_BIN:-softnet} -- falling back to NAT (see header for the sudoers line)"
    SOFTNET=0
  fi
fi

# A queued job whose labels include "self-hosted" is what we exist for.
queued_self_hosted_job() {
  local ids id
  ids="$(gh api "repos/$REPO/actions/runs?status=queued&per_page=20" --jq '.workflow_runs[].id' 2>/dev/null)" || return 1
  for id in $ids; do
    if gh api "repos/$REPO/actions/runs/$id/jobs?per_page=50" --jq '.jobs[] | select(.status=="queued") | .labels[]' 2>/dev/null | grep -qx 'self-hosted'; then
      return 0
    fi
  done
  return 1
}

while :; do
  self_changed && { say "source changed; exiting for relaunch"; exit 0; }
  if [ "$ALWAYS" -eq 0 ]; then
    until queued_self_hosted_job; do
      self_changed && { say "source changed; exiting for relaunch"; exit 0; }
      sleep "$POLL_S"
    done
    say "queued self-hosted job on $REPO -- provisioning a guest"
  fi
  NAME="petal-runner-$(date -u +%Y%m%d-%H%M%S)"
  CURRENT="$NAME"
  say "clone $GOLDEN -> $NAME"
  tart clone "$GOLDEN" "$NAME" || { sleep 30; continue; }

  args=(run "$NAME" --no-graphics --no-audio)
  [ "$SOFTNET" -eq 1 ] && args+=(--net-softnet)
  tart "${args[@]}" > "$LOG_DIR/$NAME.vm.log" 2>&1 &
  VMPID=$!

  if ! IP="$(tart ip "$NAME" --wait 240)"; then say "no IP for $NAME"; cleanup_vm "$NAME"; CURRENT=""; sleep 30; continue; fi
  ok=0; for _ in $(seq 1 60); do tart exec "$NAME" true >/dev/null 2>&1 && { ok=1; break; }; sleep 2; done
  [ "$ok" -eq 1 ] || { say "guest agent never answered in $NAME"; cleanup_vm "$NAME"; CURRENT=""; sleep 30; continue; }

  TOKEN="$(gh api -X POST "repos/$REPO/actions/runners/registration-token" --jq .token)" \
    || { say "could not mint a registration token for $REPO"; cleanup_vm "$NAME"; CURRENT=""; sleep 60; continue; }
  # job.env is consumed and deleted by the guest agent the moment it starts.
  # Verified write: one clone came up idle because this stdin hand-off
  # silently delivered nothing and the agent found no job.env.
  wrote=0
  for _ in 1 2 3 4 5; do
    printf 'RUNNER_URL=https://github.com/%s\nRUNNER_TOKEN=%s\nRUNNER_NAME=%s\nRUNNER_LABELS=%s\n' \
      "$REPO" "$TOKEN" "$NAME" "$LABELS" | tart exec -i "$NAME" bash -c 'umask 077; cat > "$HOME/tart-runner/job.env"'
    if tart exec "$NAME" bash -c 'grep -q "^RUNNER_TOKEN=." "$HOME/tart-runner/job.env"' 2>/dev/null; then wrote=1; break; fi
    sleep 3
  done
  unset TOKEN
  [ "$wrote" -eq 1 ] || { say "could not deliver job.env to $NAME"; cleanup_vm "$NAME"; CURRENT=""; sleep 30; continue; }
  tart exec "$NAME" launchctl kickstart -k "gui/$(tart exec "$NAME" id -u)/com.petal.tart-runner"
  say "$NAME ($IP) registered; waiting for its one job"

  # Ephemeral listener exits after one job. Watch the process, not the API
  # (a runner mid-job shows 'busy'; a finished one is simply gone).
  started=$(date +%s); busy_since=""
  while kill -0 "$VMPID" 2>/dev/null; do
    if self_changed && [ -z "$busy_since" ]; then say "source changed while idle; recycling $NAME"; break; fi
    if ! tart exec "$NAME" pgrep -x Runner.Listener >/dev/null 2>&1; then
      # give the agent up to 90 s to come up before treating "no listener" as finished
      [ $(( $(date +%s) - started )) -lt 90 ] && { sleep 5; continue; }
      break
    fi
    if tart exec "$NAME" pgrep -x Runner.Worker >/dev/null 2>&1; then
      [ -z "$busy_since" ] && { busy_since=$(date +%s); say "$NAME picked up a job"; }
      if [ $(( $(date +%s) - busy_since )) -gt $(( JOB_TIMEOUT_MIN * 60 )) ]; then
        say "$NAME exceeded ${JOB_TIMEOUT_MIN} min on one job -- killing"; break
      fi
    fi
    sleep 20
  done
  tart exec "$NAME" bash -c 'tail -n 40 "$HOME/tart-runner/agent.log"' > "$LOG_DIR/$NAME.agent.log" 2>&1 || true
  # tccd's own account of every Screen Recording / Accessibility decision
  # the job triggered -- the only way to see WHICH client identity it judged.
  tart exec "$NAME" bash -c 'log show --last 45m --style compact --predicate "subsystem == \"com.apple.TCC\"" 2>/dev/null | grep -iE "ScreenCapture|Accessibility|responsible|AUTHREQ|Denied|Granted|allowed" | tail -n 120' \
    > "$LOG_DIR/$NAME.tcc.log" 2>&1 || true
  # Optional post-job hold for live debugging: echo <seconds> > .../hold-seconds
  HOLD="$(cat "$HOME/Library/Application Support/petal-tart-runner/hold-seconds" 2>/dev/null || echo 0)"
  if [ "${HOLD:-0}" -gt 0 ] 2>/dev/null; then say "holding $NAME for ${HOLD}s (hold-seconds file)"; sleep "$HOLD"; fi
  say "$NAME finished (job ${busy_since:+ran}${busy_since:-never started})"
  cleanup_vm "$NAME"; CURRENT=""
  [ "$ONCE" -eq 1 ] && exit 0
  sleep 5
done

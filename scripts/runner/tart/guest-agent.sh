#!/bin/bash
# Petal Tart guest agent -- runs INSIDE the ephemeral VM, in the admin user's
# Aqua (GUI) session, started by the com.petal.tart-runner LaunchAgent.
#
# Two jobs:
#   1. Always: write ~/tart-runner/probe.txt describing what a job will see
#      (session type, Screen Recording, Accessibility). make-golden.sh reads
#      it to prove the image's TCC grants work from the session that matters.
#   2. If the host has dropped ~/tart-runner/job.env (RUNNER_URL, RUNNER_TOKEN,
#      RUNNER_NAME, RUNNER_LABELS): register as an EPHEMERAL runner and exec
#      Runner.Listener, which exits after exactly one job. `exec` matters:
#      this pid is the TCC "responsible process" for every step the job
#      spawns, so its code identity must be Runner.Listener, the client the
#      grants name. The host notices the listener exiting and deletes the VM.
set -uo pipefail
DIR="$HOME/tart-runner"
LOG="$DIR/agent.log"
RUNNER_DIR="$HOME/actions-runner"
mkdir -p "$DIR"
exec >>"$LOG" 2>&1
echo "== agent start $(date -u +%FT%TZ) pid $$"

probe() {
  local sr="unknown" ax="unknown" session
  session="$(launchctl managername 2>/dev/null || echo unknown)"
  if command -v swift >/dev/null 2>&1; then
    local d; d="$(mktemp -d)"
    cat > "$d/p.swift" <<'SWIFT'
import CoreGraphics
import ApplicationServices
print("screen-recording=" + (CGPreflightScreenCaptureAccess() ? "granted" : "denied"))
print("accessibility=" + (AXIsProcessTrusted() ? "granted" : "denied"))
SWIFT
    local out
    out="$(DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer swift "$d/p.swift" 2>/dev/null)"
    sr="$(echo "$out" | grep -o 'screen-recording=[a-z]*' | cut -d= -f2)"; sr="${sr:-probe-failed}"
    ax="$(echo "$out" | grep -o 'accessibility=[a-z]*' | cut -d= -f2)"; ax="${ax:-probe-failed}"
    rm -rf "$d"
  fi
  {
    echo "session=$session"
    echo "screen-recording=$sr"
    echo "accessibility=$ax"
    echo "display=$(system_profiler SPDisplaysDataType 2>/dev/null | grep -c Resolution)"
    echo "probed-at=$(date -u +%FT%TZ)"
  } > "$DIR/probe.txt"
  cat "$DIR/probe.txt"
}
probe

if [ ! -f "$DIR/job.env" ]; then
  echo "no job.env -- idle"
  exit 0
fi
# shellcheck disable=SC1091
. "$DIR/job.env"
rm -f "$DIR/job.env"          # the registration token is single-use; never leave it on disk
: "${RUNNER_URL:?}" "${RUNNER_TOKEN:?}" "${RUNNER_NAME:?}" "${RUNNER_LABELS:?}"

export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
cd "$RUNNER_DIR" || exit 1
rm -f .runner .credentials .credentials_rsaparams
./config.sh --unattended --ephemeral --replace --disableupdate \
  --url "$RUNNER_URL" --token "$RUNNER_TOKEN" --name "$RUNNER_NAME" --labels "$RUNNER_LABELS" --work _work \
  || { echo "config.sh failed"; exit 1; }
unset RUNNER_TOKEN
echo "registered as $RUNNER_NAME ($RUNNER_LABELS); waiting for one job"
exec ./bin/Runner.Listener run

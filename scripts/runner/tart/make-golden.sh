#!/usr/bin/env bash
# Build (or refresh) the GOLDEN Tart guest that ephemeral-runner.sh clones
# for every job. Host-side. Safe to re-run: clone is skipped if the VM
# exists, provisioning is idempotent.
#
#   scripts/runner/tart/make-golden.sh [--image <oci>] [--name <vm>] [--headless] [--keep-running]
#
# Nothing from the host is mounted into the guest, ever: the provisioning
# files are streamed in over `tart exec` (a virtiofs --dir mount also served
# the guest a stale cached view of edited scripts). Ephemeral clones get
# nothing but a job.env.
#
# One-time manual step if the image ships with SIP enabled: the guest cannot
# write its own TCC database, so open the VM window (default, no --headless),
# System Settings > Privacy & Security > Screen & System Audio Recording and
# Accessibility, enable both for "Runner.Listener", then re-run this script.
# Cirrus Labs images ship with SIP disabled and need no click.
set -euo pipefail

IMAGE="ghcr.io/cirruslabs/macos-tahoe-xcode:latest"
NAME="petal-runner-golden"
CPU=8; MEM=16384; DISK=160; DISPLAY_RES="1920x1080"
GRAPHICS=1; KEEP=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --image) IMAGE="$2"; shift 2 ;;
    --name) NAME="$2"; shift 2 ;;
    --headless) GRAPHICS=0; shift ;;
    --keep-running) KEEP=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
say() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }

# Existence by directory, not `tart list`: list fails transiently while a VM
# is stopping, and a false "missing" led to `tart clone` silently OVERWRITING
# a provisioned golden with the bare image (twice, #916).
vm_exists() { [ -d "$HOME/.tart/vms/$1" ]; }
if ! vm_exists "$NAME"; then
  say "clone $IMAGE -> $NAME"
  tart clone "$IMAGE" "$NAME"
  tart set "$NAME" --cpu "$CPU" --memory "$MEM" --disk-size "$DISK" --display "$DISPLAY_RES"
else
  say "$NAME exists; reusing"
fi

if ! pgrep -f "tart run $NAME" >/dev/null; then
  say "boot $NAME"
  args=(run "$NAME" --no-audio)
  [ "$GRAPHICS" -eq 0 ] && args+=(--no-graphics)
  tart "${args[@]}" > "/tmp/tart-$NAME.log" 2>&1 &
  echo $! > "/tmp/tart-$NAME.pid"
fi
say "wait for IP"
IP="$(tart ip "$NAME" --wait 240)"; echo "guest ip: $IP"
# guest agent needs a moment after the IP shows up
for _ in $(seq 1 60); do tart exec "$NAME" true >/dev/null 2>&1 && break; sleep 2; done

say "stream provisioning files into the guest"
tar -C "$HERE" -cf - provision-guest.sh guest-agent.sh com.petal.tart-runner.plist \
  | tart exec -i "$NAME" bash -c 'rm -rf "$HOME/petal-provision" && mkdir -p "$HOME/petal-provision" && tar -xf - -C "$HOME/petal-provision"'
say "provision (inside guest)"
set +e
tart exec "$NAME" bash -lc 'bash "$HOME/petal-provision/provision-guest.sh"'
RC=$?
set -e

if [ "$RC" -eq 0 ]; then
  if [ "$KEEP" -eq 0 ]; then
    say "stop $NAME (golden image finished)"
    tart stop "$NAME"
    # Wait for the `tart run` child to actually exit: the golden vanished
    # twice when the calling LaunchAgent loop relaunched while this process
    # was still tearing the VM down and launchd killed it mid-shutdown.
    if [ -f "/tmp/tart-$NAME.pid" ]; then
      pid="$(cat "/tmp/tart-$NAME.pid")"
      for _ in $(seq 1 120); do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
      kill -0 "$pid" 2>/dev/null && say "warning: tart run $NAME (pid $pid) still alive after 120 s"
      rm -f "/tmp/tart-$NAME.pid"
    fi
    vm_exists "$NAME" || { say "FATAL: $NAME disappeared after stop"; exit 1; }
  fi
  # Marker the ephemeral loop checks: a golden VM that exists but never
  # finished provisioning (interrupted build) must be rebuilt, not cloned.
  MARK_DIR="$HOME/Library/Application Support/petal-tart-runner"; mkdir -p "$MARK_DIR"
  date -u +%FT%TZ > "$MARK_DIR/$NAME.provisioned"
  say "golden guest '$NAME' ready. Next: PETAL_RUNNER_REPO=<owner>/<repo> scripts/runner/tart/ephemeral-runner.sh --once"
else
  say "provisioning exited $RC -- guest left RUNNING for the manual TCC step; re-run this script afterwards"
  exit "$RC"
fi

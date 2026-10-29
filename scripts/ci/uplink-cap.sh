#!/usr/bin/env bash
# Cap this machine's outbound UDP (the WebRTC media uplink) for the
# camera-freeze soak (#456): field camera/share stalls need an uplink
# bottleneck the clean VM network does not have.
#
#   scripts/ci/uplink-cap.sh on <kbps>
#   scripts/ci/uplink-cap.sh off
#
# Only OUTBOUND UDP is shaped, so the native sender's uploads to LiveKit Cloud
# hit the cap while the web viewers' downloads (inbound) do not. TCP (the
# runner's GitHub connection, CDP) is untouched. Root via `sudo -n`.
set -Eeuo pipefail
readonly ANCHOR="com.apple/petal-ci-uplink-cap"
readonly PIPE_ID=37811
readonly STATE_DIR="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/petal-ci-uplink-cap"
SUDO=""; [ "$(id -u)" -ne 0 ] && SUDO="sudo -n"
die() { echo "uplink-cap: FATAL: $*" >&2; exit 1; }

teardown() {
  $SUDO /sbin/pfctl -q -a "$ANCHOR" -F all 2>/dev/null || true
  $SUDO /usr/sbin/dnctl -q pipe delete "$PIPE_ID" 2>/dev/null || true
  if [ -f "$STATE_DIR/pf-token" ]; then
    $SUDO /sbin/pfctl -q -X "$(cat "$STATE_DIR/pf-token")" 2>/dev/null || true
    rm -f "$STATE_DIR/pf-token"
  fi
}

case "${1:-}" in
  on)
    kbps="${2:-}"
    [[ "$kbps" =~ ^[0-9]+$ ]] && [ "$kbps" -ge 300 ] || die "usage: $0 on <kbps >= 300>"
    $SUDO true 2>/dev/null || die "passwordless sudo unavailable"
    mkdir -p "$STATE_DIR"
    teardown
    trap teardown ERR
    $SUDO /usr/sbin/dnctl -q pipe "$PIPE_ID" config bw "${kbps}Kbit/s" queue 50
    echo "dummynet out quick proto udp from any to any pipe ${PIPE_ID}" | $SUDO /sbin/pfctl -q -a "$ANCHOR" -f -
    out="$($SUDO /sbin/pfctl -E 2>&1 || true)"
    token="$(printf '%s\n' "$out" | awk -F': *' '/[Tt]oken/ {print $2; exit}')"
    [ -n "$token" ] || { echo "$out" >&2; teardown; die "pfctl -E returned no token"; }
    printf '%s\n' "$token" > "$STATE_DIR/pf-token"
    trap - ERR
    $SUDO /usr/sbin/dnctl pipe show "$PIPE_ID" | grep -q "${PIPE_ID}" || { teardown; die "pipe not visible after apply"; }
    echo "uplink-cap: ON outbound UDP capped at ${kbps} kbit/s (pipe ${PIPE_ID}, pf token ${token})"
    $SUDO /usr/sbin/dnctl pipe show "$PIPE_ID" | sed 's/^/uplink-cap:   /'
    ;;
  off)
    teardown
    if $SUDO /usr/sbin/dnctl pipe show "$PIPE_ID" 2>/dev/null | grep -qE "^0*${PIPE_ID}:"; then die "pipe still present"; fi
    echo "uplink-cap: OFF (verified)"
    ;;
  *) echo "usage: $0 on <kbps> | off" >&2; exit 2 ;;
esac

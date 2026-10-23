#!/usr/bin/env bash
# Network impairment for the release e2e loopback tier (#378).
#
#   scripts/ci/net-impair.sh on <one_way_delay_ms> <loss_pct>
#   scripts/ci/net-impair.sh off
#   scripts/ci/net-impair.sh check-clear      # exit 1 if any of our rules/pipes remain
#
# The loopback tier runs LiveKit on localhost, so every network leg is ~0ms
# and lossless -- the one condition no real meeting has. This puts a dummynet
# pipe on the LOCAL LiveKit ports only (signal 7880/tcp, ICE-TCP 7881, media
# 7882/udp -- `livekit-server --dev`), matched on source OR destination port,
# so each packet to or from the SFU is delayed/dropped exactly once per
# traversal. CDP (9222), vite (5185) and the autotest socket are untouched.
#
# Semantics: every participant gets a path to the SFU with <delay> each way
# (RTT 2x<delay>) and <loss_pct>% loss each way. A web-controller input crosses
# two such paths (controller->SFU, SFU->host), as it does in a real meeting.
#
# `on` PROVES the impairment took: it times a TCP connect to the SFU before and
# after (expects ~2x<delay> added), and refuses to report success otherwise --
# a silently unapplied rule would turn the impaired pass into a clean one.
# `off` tears down and proves the connect time is back. Root is taken with
# `sudo -n` (the Tart guest's admin user has passwordless sudo; see
# scripts/runner/tart/provision-guest.sh); it never prompts.
#
# PETAL_CI_NET_IMPAIR_DRY_RUN=1 prints the privileged commands instead of
# running them and skips the live measurements (scripts/test-ci-net-impair.sh).
set -Eeuo pipefail

readonly ANCHOR="${PETAL_CI_NET_IMPAIR_ANCHOR:-com.apple/petal-ci-impair}"
readonly PIPE_ID="${PETAL_CI_NET_IMPAIR_PIPE:-37801}"
readonly PORTS="${PETAL_CI_NET_IMPAIR_PORTS:-7880:7882}"
readonly PROBE_HOST="${PETAL_CI_NET_IMPAIR_PROBE_HOST:-127.0.0.1}"
readonly PROBE_PORT="${PETAL_CI_NET_IMPAIR_PROBE_PORT:-7880}"
readonly PFCTL="${PETAL_CI_NET_IMPAIR_PFCTL:-/sbin/pfctl}"
readonly DNCTL="${PETAL_CI_NET_IMPAIR_DNCTL:-/usr/sbin/dnctl}"
readonly STATE_DIR="${PETAL_CI_NET_IMPAIR_STATE_DIR:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/petal-ci-net-impair}"
readonly TOKEN_FILE="$STATE_DIR/pf-token"
readonly DRY_RUN="${PETAL_CI_NET_IMPAIR_DRY_RUN:-0}"

log() { echo "net-impair: $*"; }
die() { echo "net-impair: FATAL: $*" >&2; exit 1; }
dry() { [ "$DRY_RUN" = "1" ]; }

# A string, not an array: macOS /bin/bash is 3.2, where "${empty[@]}" trips set -u.
SUDO="${PETAL_CI_NET_IMPAIR_SUDO-auto}"
if [ "$SUDO" = "auto" ]; then
  SUDO=""
  if [ "$(id -u)" -ne 0 ]; then SUDO="sudo -n"; fi
fi

priv() {
  if dry; then
    printf '+ %s' "${SUDO:+$SUDO }"; printf '%q ' "$@"; printf '\n'
    return 0
  fi
  $SUDO "$@"
}

require_root() {
  dry && return 0
  [ -z "$SUDO" ] && return 0
  $SUDO true 2>/dev/null || die "passwordless sudo is unavailable for $(id -un); dummynet/pf need root. On the Tart runner the admin user has it (provision-guest.sh refuses to build a golden without it)."
}

rules_text() {
  # Each packet to or from the SFU ports is matched by exactly one rule
  # (`quick`), on output -- loopback traffic is also seen on input, and
  # shaping both would double the delay.
  cat <<EOF
dummynet out quick proto { tcp udp } from any to any port ${PORTS} pipe ${PIPE_ID}
dummynet out quick proto { tcp udp } from any port ${PORTS} to any pipe ${PIPE_ID}
EOF
}

# TCP connect time to the SFU, ms (median of 5). Empty when nothing listens.
connect_ms() {
  # Test seam only: a command that prints the connect time in ms.
  if [ -n "${PETAL_CI_NET_IMPAIR_PROBE_CMD:-}" ]; then
    $PETAL_CI_NET_IMPAIR_PROBE_CMD
    return 0
  fi
  local samples=() i t
  for i in 1 2 3 4 5; do
    t="$(curl -s -o /dev/null --max-time 5 -w '%{time_connect}' "http://${PROBE_HOST}:${PROBE_PORT}/" 2>/dev/null || true)"
    [ -n "$t" ] && [ "$t" != "0.000000" ] && samples+=("$t")
  done
  [ "${#samples[@]}" -gt 0 ] || return 0
  printf '%s\n' "${samples[@]}" | sort -n | awk '{v[NR]=$1} END {printf "%.1f\n", v[int((NR+1)/2)]*1000}'
}

live_state() {
  # Anchor rules (dummynet rules list under -s dummynet on macOS pfctl; older
  # builds print them under -s rules) plus the pipe table.
  {
    $SUDO "$PFCTL" -a "$ANCHOR" -s dummynet 2>/dev/null || true
    $SUDO "$PFCTL" -a "$ANCHOR" -s rules 2>/dev/null || true
    $SUDO "$DNCTL" pipe show "$PIPE_ID" 2>/dev/null || true
  }
}

remaining_impairment() {
  live_state | grep -E "pipe ${PIPE_ID}([^0-9]|$)|^0*${PIPE_ID}:" || true
}

teardown() {
  priv "$PFCTL" -q -a "$ANCHOR" -F all 2>/dev/null || true
  priv "$DNCTL" -q pipe delete "$PIPE_ID" 2>/dev/null || true
  if [ -f "$TOKEN_FILE" ]; then
    priv "$PFCTL" -q -X "$(cat "$TOKEN_FILE")" 2>/dev/null || true
    dry || rm -f "$TOKEN_FILE"
  fi
}

cmd_on() {
  local delay_ms="$1" loss_pct="$2" plr before after added expected_min
  [[ "$delay_ms" =~ ^[0-9]+$ ]] || die "delay must be an integer number of ms, got '$delay_ms'"
  [[ "$loss_pct" =~ ^[0-9]+(\.[0-9]+)?$ ]] || die "loss must be a percentage, got '$loss_pct'"
  awk -v l="$loss_pct" 'BEGIN { exit !(l < 50) }' || die "loss ${loss_pct}% is not a meeting, it is an outage (max 50)"
  [ "$delay_ms" -le 1000 ] || die "delay ${delay_ms}ms > 1000ms"
  plr="$(awk -v l="$loss_pct" 'BEGIN { printf "%.4f", l / 100 }')"
  require_root

  if dry; then
    log "dry run: rules for anchor $ANCHOR:"; rules_text
    priv "$DNCTL" -q pipe "$PIPE_ID" config delay "${delay_ms}ms" plr "$plr"
    priv "$PFCTL" -q -a "$ANCHOR" -f -
    priv "$PFCTL" -E
    return 0
  fi

  before="$(connect_ms)"
  [ -n "$before" ] || die "nothing answers on ${PROBE_HOST}:${PROBE_PORT}; start LiveKit before impairing it (the impairment must be measurable)"
  # Any failure from here on must not leave a half-applied impairment.
  trap 'teardown' ERR INT TERM
  mkdir -p "$STATE_DIR"
  teardown
  priv "$DNCTL" -q pipe "$PIPE_ID" config delay "${delay_ms}ms" plr "$plr"
  rules_text | priv "$PFCTL" -q -a "$ANCHOR" -f -
  # pf is reference-counted: -E returns a token, -X <token> releases ours only.
  local enable_out token
  enable_out="$($SUDO "$PFCTL" -E 2>&1 || true)"
  token="$(printf '%s\n' "$enable_out" | awk -F': *' '/[Tt]oken/ {print $2; exit}')"
  [ -n "$token" ] || { echo "$enable_out" >&2; teardown; die "pfctl -E returned no enable token"; }
  printf '%s\n' "$token" > "$TOKEN_FILE"
  trap - ERR INT TERM

  after="$(connect_ms)"
  if [ "$delay_ms" -gt 0 ]; then
    # SYN and SYN-ACK each cross the pipe once: ~2x delay. 0.75 absorbs
    # dummynet's 10ms clock-tick rounding; loss can only push it higher.
    expected_min="$(awk -v d="$delay_ms" 'BEGIN { printf "%.1f", 2 * d * 0.75 }')"
    added="$(awk -v a="${after:-0}" -v b="$before" 'BEGIN { printf "%.1f", a - b }')"
    log "connect time to ${PROBE_HOST}:${PROBE_PORT}: before ${before}ms, after ${after:-none}ms (added ${added}ms, need >= ${expected_min}ms)"
    if [ -z "$after" ] || ! awk -v a="$added" -v m="$expected_min" 'BEGIN { exit !(a >= m) }'; then
      live_state >&2
      teardown
      die "impairment did NOT take effect on the SFU path (added ${added}ms < ${expected_min}ms); refusing to run an 'impaired' pass over a clean network"
    fi
  else
    log "connect time to ${PROBE_HOST}:${PROBE_PORT}: before ${before}ms, after ${after:-none}ms (loss-only profile; rule presence checked instead)"
    [ -n "$(remaining_impairment)" ] || { teardown; die "loss-only impairment: rule/pipe not visible after apply"; }
  fi
  log "ON: ports ${PORTS}, ${delay_ms}ms each way per traversal, ${loss_pct}% loss each way (pipe ${PIPE_ID}, anchor ${ANCHOR}, pf token ${token})"
  $SUDO "$DNCTL" pipe show "$PIPE_ID" 2>/dev/null | sed 's/^/net-impair:   /' || true
}

cmd_off() {
  local before after left
  require_root
  if dry; then teardown; log "dry run: would verify teardown"; return 0; fi
  before="$(connect_ms)"
  teardown
  left="$(remaining_impairment)"
  [ -z "$left" ] || { echo "$left" >&2; die "impairment still present after teardown"; }
  after="$(connect_ms)"
  log "OFF (verified: no rule or pipe ${PIPE_ID} left); connect time before ${before:-n/a}ms, after ${after:-n/a}ms"
}

cmd_check_clear() {
  local left
  require_root
  if dry; then log "dry run: would check for pipe ${PIPE_ID} / anchor ${ANCHOR}"; return 0; fi
  left="$(remaining_impairment)"
  if [ -n "$left" ]; then
    echo "$left" >&2
    die "dummynet impairment is STILL INSTALLED (pipe ${PIPE_ID}, anchor ${ANCHOR})"
  fi
  log "clear: no rule or pipe ${PIPE_ID} installed"
}

case "${1:-}" in
  on) [ "$#" -eq 3 ] || die "usage: $0 on <one_way_delay_ms> <loss_pct>"; cmd_on "$2" "$3" ;;
  off) cmd_off ;;
  check-clear) cmd_check_clear ;;
  *) echo "usage: $0 on <one_way_delay_ms> <loss_pct> | off | check-clear" >&2; exit 2 ;;
esac

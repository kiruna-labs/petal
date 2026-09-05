#!/usr/bin/env bash
#
# Test Cockpit CHAOS-NET impairment helper (#261).
#
# Interface:
#   scripts/net-impair.sh on <profile>
#   scripts/net-impair.sh off
#   scripts/net-impair.sh status
#
# This script is designed to be invoked by a narrow sudoers rule with
# `sudo -n /absolute/path/scripts/net-impair.sh ...`. It never prompts for
# sudo itself. It only shapes this machine's egress to the configured LiveKit
# host's resolved IPs; it refuses to run without that host data rather than
# installing a blanket network impairment.

set -euo pipefail

readonly ANCHOR="com.apple/petal-net-impair"
readonly PIPE_ID="26101"
readonly PFCTL="${PETAL_NET_IMPAIR_PFCTL:-/sbin/pfctl}"
readonly DNCTL="${PETAL_NET_IMPAIR_DNCTL:-/usr/sbin/dnctl}"
readonly DSCACHEUTIL="${PETAL_NET_IMPAIR_DSCACHEUTIL:-/usr/bin/dscacheutil}"
readonly HOST_CMD="${PETAL_NET_IMPAIR_HOST_CMD:-/usr/bin/host}"
readonly DRY_RUN="${PETAL_NET_IMPAIR_DRY_RUN:-0}"
readonly STATE_DIR="${PETAL_NET_IMPAIR_STATE_DIR:-/var/run/petal-net-impair}"
readonly TOKEN_FILE="${STATE_DIR}/pf-token"
readonly PROFILE_FILE="${STATE_DIR}/profile"
readonly HOST_FILE="${STATE_DIR}/host"
readonly IPS_FILE="${STATE_DIR}/ips"

usage() {
  cat <<EOF
usage:
  scripts/net-impair.sh on <profile>
  scripts/net-impair.sh off
  scripts/net-impair.sh status

profiles:
  mild-loss       80ms delay, 2% packet loss, 3Mbit/s egress cap
  lossy-3pct      120ms delay, 3% packet loss, 1.5Mbit/s egress cap
  high-latency    350ms delay, no packet loss, 5Mbit/s egress cap
  constrained-4g  180ms delay, 1% packet loss, 800Kbit/s egress cap

required target:
  Set PETAL_LIVEKIT_HOST=<host> or LIVEKIT_URL=wss://<host>[...].

testability:
  PETAL_NET_IMPAIR_DRY_RUN=1 prints the pfctl/dnctl commands without applying.
EOF
}

die() {
  echo "net-impair.sh: $*" >&2
  exit 1
}

log() {
  echo "net-impair.sh: $*"
}

is_dry_run() {
  [ "$DRY_RUN" = "1" ] || [ "$DRY_RUN" = "true" ]
}

require_root_for_apply() {
  if is_dry_run; then
    return
  fi
  if [ "${EUID}" -ne 0 ]; then
    die "must be run non-interactively as root, e.g. sudo -n scripts/net-impair.sh $ACTION${PROFILE:+ $PROFILE}; this script will not prompt for sudo"
  fi
}

require_tools() {
  [ -x "$PFCTL" ] || die "pfctl not found or not executable at '$PFCTL'"
  [ -x "$DNCTL" ] || die "dnctl not found or not executable at '$DNCTL'"
}

profile_args() {
  case "$1" in
    mild-loss)
      printf '%s\n' "delay 80ms plr 0.02 bw 3Mbit/s queue 50"
      ;;
    lossy-3pct)
      printf '%s\n' "delay 120ms plr 0.03 bw 1500Kbit/s queue 40"
      ;;
    high-latency)
      printf '%s\n' "delay 350ms plr 0 bw 5Mbit/s queue 80"
      ;;
    constrained-4g)
      printf '%s\n' "delay 180ms plr 0.01 bw 800Kbit/s queue 35"
      ;;
    *)
      die "unknown profile '$1' (expected: mild-loss, lossy-3pct, high-latency, constrained-4g)"
      ;;
  esac
}

host_from_livekit_url() {
  local url="$1"
  local authority host
  authority="${url#*://}"
  authority="${authority%%/*}"
  authority="${authority##*@}"

  if [[ "$authority" == \[*\]* ]]; then
    host="${authority#\[}"
    host="${host%%\]*}"
  else
    host="${authority%%:*}"
  fi

  printf '%s\n' "$host"
}

livekit_host() {
  local host=""
  if [ -n "${PETAL_LIVEKIT_HOST:-}" ]; then
    host="$PETAL_LIVEKIT_HOST"
  elif [ -n "${LIVEKIT_URL:-}" ]; then
    host="$(host_from_livekit_url "$LIVEKIT_URL")"
  else
    die "missing LiveKit target; set PETAL_LIVEKIT_HOST or LIVEKIT_URL so impairment can be scoped to LiveKit only"
  fi

  host="${host%.}"
  if [ -z "$host" ]; then
    die "LiveKit host resolved to an empty value"
  fi
  if ! [[ "$host" =~ ^[A-Za-z0-9._:-]+$ ]]; then
    die "LiveKit host contains unsupported characters: '$host'"
  fi
  case "$host" in
    localhost|localhost.*|*.local|127.*|0.0.0.0|::1)
      die "refusing to impair loopback/local LiveKit host '$host'; CHAOS-NET is for scoped egress to a real LiveKit endpoint"
      ;;
  esac

  printf '%s\n' "$host"
}

resolve_ips() {
  local host="$1"
  local found=""

  if [[ "$host" =~ ^[0-9]+(\.[0-9]+){3}$ ]] || [[ "$host" == *:* ]]; then
    printf '%s\n' "$host"
    return
  fi

  if [ -x "$DSCACHEUTIL" ]; then
    found="$("$DSCACHEUTIL" -q host -a name "$host" 2>/dev/null | awk '/ip_address:/{print $2}' || true)"
  fi
  if [ -z "$found" ] && [ -x "$HOST_CMD" ]; then
    found="$("$HOST_CMD" "$host" 2>/dev/null | awk '/has address/{print $NF} /has IPv6 address/{print $NF}' || true)"
  fi

  if [ -z "$found" ]; then
    die "could not resolve LiveKit host '$host'; refusing to apply an unscoped impairment"
  fi

  printf '%s\n' "$found" | awk '!seen[$0]++'
}

pf_target_list() {
  local ips=("$@")
  local joined=""
  local ip

  for ip in "${ips[@]}"; do
    if ! [[ "$ip" =~ ^[0-9A-Fa-f:.]+$ ]]; then
      die "resolved LiveKit address '$ip' is not an IP literal"
    fi
    joined="${joined}${joined:+ }${ip}"
  done

  [ -n "$joined" ] || die "no LiveKit IPs resolved"
  printf '{ %s }\n' "$joined"
}

write_rules() {
  local rules_file="$1"
  local targets="$2"

  cat >"$rules_file" <<EOF
dummynet out quick proto { tcp udp } from any to ${targets} pipe ${PIPE_ID}
EOF
}

run_cmd() {
  if is_dry_run; then
    printf '+'
    printf ' %q' "$@"
    printf '\n'
    return 0
  fi
  "$@"
}

ensure_state_dir() {
  if is_dry_run; then
    return
  fi
  mkdir -p "$STATE_DIR"
  chmod 700 "$STATE_DIR"
}

disable_pf_token() {
  [ -f "$TOKEN_FILE" ] || return 0

  local token
  token="$(cat "$TOKEN_FILE")"
  if [ -n "$token" ] && ! run_cmd "$PFCTL" -X "$token" >/dev/null 2>&1; then
    log "failed to disable pf token '$token'"
    return 1
  fi
  if ! is_dry_run; then
    rm -f "$TOKEN_FILE"
  fi
}

live_pf_rules() {
  local output
  if ! output="$("$PFCTL" -a "$ANCHOR" -sr 2>&1)"; then
    die "cannot inspect pf anchor '$ANCHOR': $output"
  fi
  printf '%s\n' "$output"
}

live_dnctl_pipes() {
  local output
  if ! output="$("$DNCTL" pipe show 2>&1)"; then
    die "cannot inspect dummynet pipes: $output"
  fi
  printf '%s\n' "$output"
}

anchor_has_impairment() {
  grep -Eq "dummynet .*pipe[[:space:]]+${PIPE_ID}([[:space:]]|$)"
}

pipe_is_present() {
  grep -Eq "(^|[^0-9])0*${PIPE_ID}:"
}

verify_impairment_cleared() {
  local rules pipes
  rules="$(live_pf_rules)"
  pipes="$(live_dnctl_pipes)"
  if printf '%s\n' "$rules" | anchor_has_impairment; then
    printf '%s\n' "$rules" >&2
    die "pf anchor '$ANCHOR' still contains impairment rule after teardown"
  fi
  if printf '%s\n' "$pipes" | pipe_is_present; then
    printf '%s\n' "$pipes" >&2
    die "dummynet pipe $PIPE_ID still exists after teardown"
  fi
}

clear_impairment() {
  if is_dry_run; then
    run_cmd "$PFCTL" -a "$ANCHOR" -F all
    run_cmd "$DNCTL" -q pipe delete "$PIPE_ID"
    if [ -f "$TOKEN_FILE" ]; then
      disable_pf_token
    fi
    log "dry run: would verify impairment teardown"
    return
  fi

  local rules pipes
  rules="$(live_pf_rules)"
  pipes="$(live_dnctl_pipes)"
  if printf '%s\n' "$rules" | anchor_has_impairment; then
    run_cmd "$PFCTL" -a "$ANCHOR" -F all
  fi
  if printf '%s\n' "$pipes" | pipe_is_present; then
    run_cmd "$DNCTL" -q pipe delete "$PIPE_ID"
  fi
  disable_pf_token
  verify_impairment_cleared

  rm -f "$PROFILE_FILE" "$HOST_FILE" "$IPS_FILE"
  rmdir "$STATE_DIR" 2>/dev/null || true
}

warn_on_target_drift() {
  [ -f "$HOST_FILE" ] || return

  local host stored_ips live_ips
  host="$(cat "$HOST_FILE")"
  if [ -z "$host" ]; then
    log "WARNING: state host is empty; cannot check DNS drift"
    return
  fi
  if ! live_ips="$(resolve_ips "$host" 2>&1)"; then
    log "WARNING: could not re-resolve '$host'; live impairment scope cannot be compared"
    return
  fi
  if [ ! -f "$IPS_FILE" ]; then
    log "WARNING: no recorded target IPs; cannot check DNS drift for '$host'"
    return
  fi
  stored_ips="$(sort "$IPS_FILE")"
  if [ "$stored_ips" != "$(printf '%s\n' "$live_ips" | sort)" ]; then
    log "WARNING: DNS drift for '$host': recorded targets no longer match current resolution"
    printf '%s\n' "$live_ips" | sed 's/^/  current ip: /'
  fi
}

apply_impairment() {
  local profile="$1"
  local args host resolved targets rules_file token output
  local ips=()
  local ip

  require_tools
  args="$(profile_args "$profile")"
  host="$(livekit_host)"
  resolved="$(resolve_ips "$host")"
  while IFS= read -r ip; do
    ips+=("$ip")
  done <<<"$resolved"
  if [ "${#ips[@]}" -eq 0 ]; then
    die "no LiveKit IPs resolved"
  fi
  targets="$(pf_target_list "${ips[@]}")"
  rules_file="$(mktemp "${TMPDIR:-/tmp}/petal-net-impair.XXXXXX")"
  trap 'rm -f "$rules_file"' RETURN
  write_rules "$rules_file" "$targets"

  "$PFCTL" -q -n -a "$ANCHOR" -f "$rules_file" >/dev/null
  "$DNCTL" -n pipe "$PIPE_ID" config $args >/dev/null

  require_root_for_apply
  ensure_state_dir

  trap 'clear_impairment; rm -f "$rules_file"' ERR INT TERM

  clear_impairment
  run_cmd "$DNCTL" -q pipe "$PIPE_ID" config $args
  run_cmd "$PFCTL" -a "$ANCHOR" -f "$rules_file"

  if is_dry_run; then
    run_cmd "$PFCTL" -E
  else
    output="$("$PFCTL" -E)"
    token="$(printf '%s\n' "$output" | awk '/Token/ {print $3; exit}')"
    if [ -z "$token" ]; then
      die "pfctl -E did not return an enable token; run scripts/net-impair.sh off to clean up"
    fi
    printf '%s\n' "$token" >"$TOKEN_FILE"
    printf '%s\n' "$profile" >"$PROFILE_FILE"
    printf '%s\n' "$host" >"$HOST_FILE"
    printf '%s\n' "${ips[@]}" >"$IPS_FILE"
  fi

  trap - ERR INT TERM
  log "applied '$profile' to LiveKit host '$host' (${ips[*]})"
}

status_impairment() {
  local rules pipes pf_on pipe_on
  rules="$(live_pf_rules)"
  pipes="$(live_dnctl_pipes)"
  pf_on=0
  pipe_on=0
  if printf '%s\n' "$rules" | anchor_has_impairment; then
    pf_on=1
  fi
  if printf '%s\n' "$pipes" | pipe_is_present; then
    pipe_on=1
  fi

  if [ "$pf_on" -eq 1 ] && [ "$pipe_on" -eq 1 ]; then
    if [ -f "$PROFILE_FILE" ]; then
      log "state: on (verified live) profile=$(cat "$PROFILE_FILE") host=$(cat "$HOST_FILE" 2>/dev/null || printf unknown)"
      [ -f "$IPS_FILE" ] && sed 's/^/  recorded target ip: /' "$IPS_FILE"
      warn_on_target_drift
    else
      log "state: on (verified live), but no script state file is present"
    fi
  elif [ "$pf_on" -eq 0 ] && [ "$pipe_on" -eq 0 ]; then
    log "state: off (verified live)"
    [ ! -f "$PROFILE_FILE" ] || log "WARNING: stale script state exists although live impairment is off"
  else
    printf '%s\n' "$rules" >&2
    printf '%s\n' "$pipes" >&2
    die "INCONSISTENT LIVE STATE: pf rule=$pf_on dummynet pipe=$pipe_on; refusing to report a safe baseline"
  fi

  log "dnctl pipe $PIPE_ID:"
  printf '%s\n' "$pipes" | grep -E "(^|[^0-9])0*${PIPE_ID}:" || true
}

ACTION="${1:-status}"
PROFILE="${2:-}"

case "$ACTION" in
  on)
    [ "$#" -eq 2 ] || { usage >&2; exit 2; }
    apply_impairment "$PROFILE"
    ;;
  off)
    [ "$#" -eq 1 ] || { usage >&2; exit 2; }
    require_tools
    require_root_for_apply
    clear_impairment
    if is_dry_run; then
      log "dry run: did not clear impairment"
    else
      log "cleared impairment (verified live)"
    fi
    ;;
  status)
    [ "$#" -eq 1 ] || { usage >&2; exit 2; }
    require_tools
    status_impairment
    ;;
  -h|--help|help)
    usage
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

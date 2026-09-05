#!/usr/bin/env bash
# Provision the GOLDEN Tart macOS guest for Petal's end-to-end tiers.
# Runs INSIDE the guest (as the image's `admin` user), invoked by
# make-golden.sh through `tart exec`. Idempotent: re-run after a partial
# failure.
#
# Why a VM at all: the Cockpit, the remote-control loopback suite and the
# native no-black-frame gate need a real WindowServer session plus Screen
# Recording / Accessibility TCC grants, which GitHub-hosted macOS runners
# cannot provide -- and a runner on the bare host would execute repo code
# (and every npm/cargo build script) with the host user's entire home
# directory in reach. The guest sees only its own disk.
#
# What this installs:
#   - livekit-server, node, rustup (workflow preflight needs them on PATH)
#   - Google Chrome (headless-Chrome web peer)
#   - actions/runner (osx-arm64) under ~/actions-runner, NOT configured --
#     each ephemeral clone registers itself with a fresh token
#   - ~/tart-runner/agent.sh + a LaunchAgent in the Aqua session that runs
#     the job the host hands it (see guest-agent.sh)
#   - TCC grants for Screen Recording + Accessibility, written directly when
#     SIP is off (Cirrus images), verified from the Aqua session either way
set -euo pipefail

RUNNER_VERSION="${RUNNER_VERSION:-2.337.0}"
SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AGENT_DIR="$HOME/tart-runner"
RUNNER_DIR="$HOME/actions-runner"
DESKTOP_BIN="$RUNNER_DIR/_work/petal/petal/apps/desktop/src-tauri/target/debug/desktop"
UID_NUM="$(id -u)"

say() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }

say "guest: $(sw_vers -productName) $(sw_vers -productVersion), user $(id -un), SIP: $(csrutil status | head -1)"
# Cirrus images install Xcode as /Applications/Xcode_<ver>.app; the workflows
# and plists hardcode DEVELOPER_DIR=/Applications/Xcode.app, so link it.
if [ ! -d /Applications/Xcode.app ]; then
  real="$(ls -d /Applications/Xcode*.app 2>/dev/null | sort -V | tail -1)"
  [ -n "$real" ] || { echo "FATAL: no Xcode under /Applications -- use the *-xcode image" >&2; exit 1; }
  sudo ln -s "$real" /Applications/Xcode.app
  echo "linked /Applications/Xcode.app -> $real"
fi
sudo -n true 2>/dev/null || { echo "FATAL: passwordless sudo required for the admin user" >&2; exit 1; }
sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
sudo xcodebuild -license accept >/dev/null 2>&1 || true

# Softnet (host loop --softnet) hands the guest a single resolver, its own
# gateway (192.168.2.1), whose DNS proxy did not answer on this host: IP
# connectivity fine, every name lookup timed out, runner registered but never
# reached GitHub. Public resolvers work through Softnet's egress, and are
# harmless under plain NAT. Persisted in the golden, inherited by clones.
say "guest DNS: public resolvers (Softnet-safe)"
sudo networksetup -setdnsservers Ethernet 1.1.1.1 8.8.8.8 2>/dev/null || echo "  (could not set DNS on 'Ethernet' -- check networksetup -listallnetworkservices)"

say "Homebrew tools"
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1
command -v brew >/dev/null || { echo "FATAL: Homebrew missing in image" >&2; exit 1; }
for f in livekit node coreutils; do brew list --versions "$f" >/dev/null 2>&1 || brew install "$f"; done
# ci-selfhosted.yml, ci-local.sh and the native pixel gate all wrap long
# commands in `timeout` (CLAUDE.md rule); macOS has none, Homebrew names it
# gtimeout. The bare host links it the same way.
[ -x /opt/homebrew/bin/timeout ] || ln -s /opt/homebrew/bin/gtimeout /opt/homebrew/bin/timeout
[ -x "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" ] || brew install --cask google-chrome
if ! command -v rustup >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/rustup" ]; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null
fi
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
say "tools: livekit-server=$(command -v livekit-server) node=$(node --version) cargo=$(cargo --version | cut -d' ' -f2)"

say "actions/runner $RUNNER_VERSION"
# The Cirrus image ships its own ~/actions-runner (2.334.0 at the time), and
# GitHub refuses deprecated versions ("cannot receive messages"). Replace
# anything that is not exactly the pinned version; --disableupdate in the
# agent means nothing will bump it later.
have="$("$RUNNER_DIR/bin/Runner.Listener" --version 2>/dev/null || true)"
if [ "$have" != "$RUNNER_VERSION" ]; then
  echo "  replacing runner '${have:-none}' with $RUNNER_VERSION"
  rm -rf "$RUNNER_DIR"; mkdir -p "$RUNNER_DIR"
  curl -sSL "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-osx-arm64-${RUNNER_VERSION}.tar.gz" \
    | tar xz -C "$RUNNER_DIR"
fi
[ "$("$RUNNER_DIR/bin/Runner.Listener" --version)" = "$RUNNER_VERSION" ] || { echo "FATAL: runner version mismatch after install" >&2; exit 1; }
rm -f "$RUNNER_DIR/.runner" "$RUNNER_DIR/.credentials" "$RUNNER_DIR/.credentials_rsaparams"   # never bake a registration into the image

say "guest agent + LaunchAgent"
mkdir -p "$AGENT_DIR" "$HOME/Library/LaunchAgents"
install -m 755 "$SRC_DIR/guest-agent.sh" "$AGENT_DIR/agent.sh"
rm -f "$AGENT_DIR/job.env" "$AGENT_DIR/done"
sed -e "s#__HOME__#$HOME#g" "$SRC_DIR/com.petal.tart-runner.plist" > "$HOME/Library/LaunchAgents/com.petal.tart-runner.plist"
launchctl bootout "gui/$UID_NUM/com.petal.tart-runner" 2>/dev/null || true
launchctl bootstrap "gui/$UID_NUM" "$HOME/Library/LaunchAgents/com.petal.tart-runner.plist"

# TCC. The runner's LaunchAgent process (Runner.Listener after agent.sh
# exec's it) is the "responsible process" for everything a job spawns, so
# it is the client that needs Screen Recording + Accessibility; the desktop
# binary is added too, belt and braces. Rows are path-based (client_type 1)
# with no code requirement, which is how TCC keys unsigned/ad-hoc binaries.
# tccd validates a stored row's code requirement (csreq) against the binary
# it attributes the request to. A row with NULL csreq for a SIGNED binary is
# ignored -- tccd logged authValue=1 (unknown) / authReason=5 (service
# policy) for Runner.Listener until the compiled designated requirement was
# stored alongside the path (#916, first two VM jobs). Unsigned/ad-hoc
# binaries keep NULL.
csreq_hex() {
  local bin="$1" req tmp
  # ad-hoc binaries print '# designated => cdhash H"..."' (leading '# ')
  req="$(codesign -d -r- "$bin" 2>&1 | sed -n 's/^#* *designated => //p')"
  [ -n "$req" ] || return 1
  tmp="$(mktemp)"
  csreq -r="$req" -b "$tmp" 2>/dev/null || { rm -f "$tmp"; return 1; }
  xxd -p "$tmp" | tr -d '\n'; rm -f "$tmp"
}
grant_tcc() {
  local db="$1" service="$2" client="$3" sudo_prefix="$4" hex csreq_sql
  hex="$(csreq_hex "$client" || true)"
  csreq_sql="NULL"; [ -n "$hex" ] && csreq_sql="X'$hex'"
  $sudo_prefix sqlite3 "$db" "DELETE FROM access WHERE service='$service' AND client='$client';
    INSERT INTO access (service, client, client_type, auth_value, auth_reason, auth_version, csreq, indirect_object_identifier_type, indirect_object_identifier, flags, last_modified)
    VALUES ('$service', '$client', 1, 2, 3, 1, $csreq_sql, 0, 'UNUSED', 0, strftime('%s','now'));" 2>&1 \
    && echo "  granted $service -> $client (csreq: ${hex:+designated requirement}${hex:-none})" \
    || echo "  could not write $service for $client (see verification below)"
}
if csrutil status | grep -qi disabled; then
  say "TCC grants (SIP disabled -- writing the databases directly)"
  USER_DB="$HOME/Library/Application Support/com.apple.TCC/TCC.db"
  SYS_DB="/Library/Application Support/com.apple.TCC/TCC.db"
  # Screen Recording requests are answered by the SYSTEM tccd (uid 0) even
  # though the UI stores the grant in the user DB; tccd's trace showed it
  # never matching the user-DB row (#916). Write it to both.
  for client in "$RUNNER_DIR/bin/Runner.Listener" "$DESKTOP_BIN"; do
    grant_tcc "$USER_DB" kTCCServiceScreenCapture "$client" ""
    grant_tcc "$SYS_DB"  kTCCServiceScreenCapture "$client" "sudo"
    grant_tcc "$SYS_DB"  kTCCServiceAccessibility "$client" "sudo"
  done
  # Automation (Apple Events): the remote-control suite observes and drives
  # TextEdit through osascript / System Events. Without this every AppleScript
  # step hangs on the consent prompt and times out (22/32 cases in the first VM
  # nightly run). Rows carry the target's code identity like the UI writes.
  grant_ae() {
    local db="$1" client="$2" target_bundle="$3" target_app="$4" sudo_prefix="$5" chex thex
    chex="$(csreq_hex "$client" || true)"; thex="$(csreq_hex "$target_app" || true)"
    local c_sql="NULL" t_sql="NULL"; [ -n "$chex" ] && c_sql="X'$chex'"; [ -n "$thex" ] && t_sql="X'$thex'"
    $sudo_prefix sqlite3 "$db" "DELETE FROM access WHERE service='kTCCServiceAppleEvents' AND client='$client' AND indirect_object_identifier='$target_bundle';
      INSERT INTO access (service, client, client_type, auth_value, auth_reason, auth_version, csreq, indirect_object_identifier_type, indirect_object_identifier, indirect_object_code_identity, flags, last_modified)
      VALUES ('kTCCServiceAppleEvents', '$client', 1, 2, 3, 1, $c_sql, 0, '$target_bundle', $t_sql, 0, strftime('%s','now'));" 2>&1 \
      && echo "  granted AppleEvents -> $client may control $target_bundle" || echo "  could not write AppleEvents row for $target_bundle"
  }
  for target in "com.apple.TextEdit:/System/Applications/TextEdit.app" "com.apple.systemevents:/System/Library/CoreServices/System Events.app"; do
    grant_ae "$USER_DB" "$RUNNER_DIR/bin/Runner.Listener" "${target%%:*}" "${target#*:}" ""
    grant_ae "$SYS_DB"  "$RUNNER_DIR/bin/Runner.Listener" "${target%%:*}" "${target#*:}" "sudo"
  done
  sudo killall tccd 2>/dev/null || true
else
  say "TCC grants: SIP is ENABLED -- direct writes are not possible; grant by hand once (see make-golden.sh output)"
fi

# Test Cockpit preflight (test_cockpit::preflight_or_refuse) only checks for
# this marker; scripts/cockpit-setup.sh writes it after walking a human
# through the TCC grants, which the rows above already provide here. Both
# identifiers: the primary binary and the local N2N test peer.
say "Test Cockpit setup markers"
for ident in com.petal.app com.petal.app.testpeer; do
  mkdir -p "$HOME/Library/Application Support/$ident"
  date -u +%FT%TZ > "$HOME/Library/Application Support/$ident/.cockpit-setup-complete"
done

say "probe from the Aqua session"
rm -f "$AGENT_DIR/probe.txt"
launchctl kickstart -k "gui/$UID_NUM/com.petal.tart-runner"
for _ in $(seq 1 30); do [ -f "$AGENT_DIR/probe.txt" ] && break; sleep 1; done
if [ -f "$AGENT_DIR/probe.txt" ]; then cat "$AGENT_DIR/probe.txt"; else echo "probe did not run -- check $AGENT_DIR/agent.log"; tail -20 "$AGENT_DIR/agent.log" 2>/dev/null || true; exit 1; fi
grep -q 'session=Aqua' "$AGENT_DIR/probe.txt" || { echo "agent is not in an Aqua session -- the image must auto-login the admin user"; exit 2; }
# The probe runs as the agent's bash process BEFORE it exec's Runner.Listener,
# so its TCC answer is for /bin/bash, not for the client the rows above name.
# The authoritative check is the first job's scripts/runner/preflight.sh,
# which runs with Runner.Listener as the responsible process. With SIP off
# and the rows written, that is expected to pass; with SIP on, a denied
# probe here means the manual System Settings step is still needed.
if grep -q 'screen-recording=granted' "$AGENT_DIR/probe.txt" && grep -q 'accessibility=granted' "$AGENT_DIR/probe.txt"; then
  say "golden guest ready (grants visible even to the agent shell)"
elif csrutil status | grep -qi disabled; then
  say "golden guest ready (TCC rows written; the first job's preflight is the real grant check)"
else
  echo "TCC not granted and SIP is on -- open the VM window, System Settings > Privacy & Security, enable Screen & System Audio Recording and Accessibility for Runner.Listener, then re-run this script"; exit 2
fi

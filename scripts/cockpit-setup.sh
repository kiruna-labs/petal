#!/usr/bin/env bash
#
# Test Cockpit Phase -1 one-time setup (GitHub #253).
#
# Run ONCE per test machine, by a human, before any Test Cockpit run. Grants
# every permission a cockpit run will ever need so that no cockpit run can
# EVER be interrupted or derailed by a permission prompt, TCC dialog, or sudo
# prompt mid-run. Pairs with the non-negotiable preflight-and-refuse rule
# documented in docs/TESTING.md's "Test Cockpit" section and implemented by
# `test_cockpit::preflight_or_refuse(&AppHandle)` (apps/desktop/src-tauri/src/
# test_cockpit/mod.rs, behind the `cockpit-privileged` Cargo feature): every
# privileged cockpit entry point checks all required grants via
# non-prompting APIs FIRST and refuses immediately on any miss -- it never
# calls a prompting code path during an actual run.
#
# What this script does:
#   1. Builds the test-peer binary (target-peer/debug/desktop) used by the
#      future SHARE-N2N scenario (#262) -- a wholly separate binary (own
#      socket, own app_data_dir(), own TCC identity) from the primary dev
#      binary (target/debug/desktop).
#   2. Builds the primary dev binary if it doesn't exist yet.
#   3. Walks the human through granting Screen Recording + Accessibility for
#      BOTH binaries (TCC grant dialogs cannot be scripted -- this step is
#      inherently interactive; deep-links only get you to the right System
#      Settings pane).
#   4. Triggers the one-time Automation/AppleEvent consent for
#      osascript -> TextEdit (needed only by the dev-tier remote-control
#      suite's readback).
#   5. PRINTS (does not install) the sudoers snippet that would let a future
#      cockpit run invoke scripts/net-impair.sh without a mid-run sudo
#      password prompt. Installing a persistent passwordless-sudo rule is a
#      system-level security change this script deliberately does NOT
#      automate -- see the "ACTION REQUIRED" block this script prints. Copy
#      the printed snippet and run `sudo visudo -f ...` yourself; visudo
#      validates the syntax before it swaps the file in, which is exactly
#      the "validate before installing" safeguard a script-driven install
#      would otherwise need to reimplement.
#   6. Verifies everything it CAN automate via non-prompting checks (the
#      same `has_screen_recording_access`/`check_accessibility` preflight
#      APIs the app itself uses -- see window_source.rs/permissions.rs) and
#      writes the local `.cockpit-setup-complete` marker file only once every
#      automatable grant is confirmed. It refuses to report success while
#      any automatable grant is still missing.
#
# Idempotent: safe to re-run. Already-granted steps are detected via the
# same non-prompting checks and skipped without re-prompting.
#
# Disk space (empirically observed running this on 2026-07-08): a cold build
# of the test-peer binary in a fresh checkout/worktree (its own
# `target-peer/` directory, separate from the main `target/`) took ~9GB, and
# a from-scratch primary-binary build took ~11GB more -- ~20GB combined with
# no warm cache. Running this in a throwaway worktree without an
# already-built `target/` ran this machine out of disk space mid-run. Prefer
# running this from a checkout that already has a warm `target/` (only the
# test-peer build is then a cold ~9GB build), and make sure you have at
# least ~15GB free beforehand.
#
# Env overrides (mainly for scripted validation of this script itself, not
# normal human use):
#   COCKPIT_SETUP_MAX_WAIT_LOOPS  -- how many times to re-prompt+re-check
#                                    per binary before giving up (default 3).
#   COCKPIT_SETUP_WAIT_TIMEOUT_S  -- seconds to wait on each prompt for the
#                                    human to respond before re-checking
#                                    anyway (default 15).
#   COCKPIT_SETUP_SKIP_PEER_BUILD -- set to 1 to skip rebuilding the
#                                    test-peer binary (it already exists and
#                                    you just want to re-run the grant/verify
#                                    steps).

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# The setup preflight launches the same raw QA artifacts the cockpit uses, so
# build both through the #315 system-Swift policy rather than the CLT dev flags.
# shellcheck source=../apps/desktop/scripts/cockpit-runtime-policy.sh
source "$ROOT/apps/desktop/scripts/cockpit-runtime-policy.sh"

DESKTOP_MANIFEST="$ROOT/apps/desktop/src-tauri/Cargo.toml"
DESKTOP_DIR="$ROOT/apps/desktop/src-tauri"
TARGET_PEER_DIR="$DESKTOP_DIR/target-peer"
NATIVE_BIN="$DESKTOP_DIR/target/debug/desktop"
PEER_BIN="$TARGET_PEER_DIR/debug/desktop"
NET_IMPAIR_SCRIPT="$ROOT/scripts/net-impair.sh"
# The primary and test-peer deliberately use distinct Tauri identifiers and
# app-data directories. Keep this fixed allowlist in sync with the supported
# local N2N pair; never accept a caller-provided marker path or identifier.
MARKER_ROOT="$HOME/Library/Application Support"
MARKER_IDENTIFIERS=("com.petal.app" "com.petal.app.testpeer")

MAX_WAIT_LOOPS="${COCKPIT_SETUP_MAX_WAIT_LOOPS:-3}"
WAIT_TIMEOUT_S="${COCKPIT_SETUP_WAIT_TIMEOUT_S:-15}"

MISSING=()
OVERALL_OK=1

# ---------------------------------------------------------------------------
# Output helpers
# ---------------------------------------------------------------------------
step()  { printf '\n\033[1;36m==> %s\033[0m\n' "$1"; }
ok()    { printf '  \033[1;32m[OK]\033[0m %s\n' "$1"; }
warn()  { printf '  \033[1;33m[WAIT]\033[0m %s\n' "$1"; }
fail()  { printf '  \033[1;31m[MISSING]\033[0m %s\n' "$1"; MISSING+=("$1"); OVERALL_OK=0; }
info()  { printf '  %s\n' "$1"; }

# ---------------------------------------------------------------------------
# Non-prompting socket query helper. Speaks the exact newline-JSON protocol
# `apps/desktop/src-tauri/src/autotest.rs` implements. Prints the raw JSON
# response line to stdout; caller does simple substring matching so this
# script has no external JSON-parsing dependency.
# ---------------------------------------------------------------------------
query_socket() {
  local sock="$1" cmd_json="$2"
  python3 - "$sock" "$cmd_json" <<'PY' 2>/dev/null
import socket, sys
sock_path, cmd = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(5)
try:
    s.connect(sock_path)
    s.sendall((cmd + "\n").encode())
    print(s.recv(65536).decode(errors="replace"), end="")
except OSError as e:
    print('{"ok":false,"error":"socket connect/query failed: %s"}' % e)
PY
}

# Launches $1 (binary path) with an autotest socket at $2, writing its log to
# $3. No PETAL_AUTOTEST_ROOM is set, so it never joins a room -- this is
# purely so the non-prompting preflight checks can run against a live
# process, exactly as the real app would evaluate them at its own startup.
# Prints the child PID on stdout.
launch_binary() {
  local bin="$1" sock="$2" log="$3"
  rm -f "$sock"
  PETAL_AUTOTEST_SOCK="$sock" PETAL_DISABLE_AUDIO=1 RUST_LOG=info \
    nohup "$bin" >"$log" 2>&1 &
  local pid=$!
  local waited=0
  while [ ! -S "$sock" ] && [ "$waited" -lt 10 ]; do
    sleep 0.5
    waited=$((waited + 1))
  done
  echo "$pid"
}

stop_binary() {
  local pid="$1"
  kill "$pid" >/dev/null 2>&1 || true
  wait "$pid" 2>/dev/null || true
}

# $1 = socket path. Echoes "granted" or "denied" or "unknown".
screen_recording_status() {
  local sock="$1"
  local resp
  resp="$(query_socket "$sock" '{"cmd":"list_windows"}')"
  if printf '%s' "$resp" | grep -q '"ok":true'; then
    echo "granted"
  elif printf '%s' "$resp" | grep -q 'Screen Recording permission has not been granted'; then
    echo "denied"
  else
    echo "unknown"
  fi
}

# $1 = socket path. Echoes "granted" or "denied".
accessibility_status_of() {
  local sock="$1"
  local resp
  resp="$(query_socket "$sock" '{"cmd":"accessibility_status"}')"
  if printf '%s' "$resp" | grep -q '"trusted":true'; then
    echo "granted"
  else
    echo "denied"
  fi
}

open_privacy_pane() {
  local pane="$1"
  case "$pane" in
    screen-recording)
      open "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture" >/dev/null 2>&1 || true
      ;;
    accessibility)
      open "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility" >/dev/null 2>&1 || true
      ;;
  esac
}

# Walks the human through granting Screen Recording + Accessibility for one
# binary. $1 = binary path, $2 = human-readable label.
grant_and_verify() {
  local bin="$1" label="$2"
  local sock="/tmp/petal-cockpit-setup-$(basename "$(dirname "$bin")").sock"
  local log
  log="$(mktemp -t petal-cockpit-setup-log)"
  local loop=0
  local sr_status ax_status pid

  while [ "$loop" -le "$MAX_WAIT_LOOPS" ]; do
    pid="$(launch_binary "$bin" "$sock" "$log")"
    if [ ! -S "$sock" ]; then
      fail "$label: command socket never came up ($sock) -- see $log"
      stop_binary "$pid"
      return 1
    fi

    sr_status="$(screen_recording_status "$sock")"
    ax_status="$(accessibility_status_of "$sock")"
    stop_binary "$pid"

    if [ "$sr_status" = "granted" ] && [ "$ax_status" = "granted" ]; then
      ok "$label: Screen Recording GRANTED, Accessibility GRANTED ($bin)"
      return 0
    fi

    if [ "$loop" -eq "$MAX_WAIT_LOOPS" ]; then
      break
    fi

    warn "$label needs a grant (Screen Recording: $sr_status, Accessibility: $ax_status)."
    info "Binary path: $bin"
    if [ "$sr_status" != "granted" ]; then
      info "-> Open System Settings -> Privacy & Security -> Screen Recording,"
      info "   find this binary in the list (it registers once launched -- it just"
      info "   was), and enable it. Deep link: x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
      open_privacy_pane screen-recording
    fi
    if [ "$ax_status" != "granted" ]; then
      info "-> Open System Settings -> Privacy & Security -> Accessibility,"
      info "   find this binary in the list, and enable it. Deep link:"
      info "   x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
      open_privacy_pane accessibility
    fi
    read -r -t "$WAIT_TIMEOUT_S" -p "  Press Enter once granted to re-check (waits up to ${WAIT_TIMEOUT_S}s)... " _ans
    loop=$((loop + 1))
  done

  fail "$label: still missing a grant after $MAX_WAIT_LOOPS retries (Screen Recording: $sr_status, Accessibility: $ax_status). Binary: $bin. Log: $log"
  return 1
}

# ---------------------------------------------------------------------------
# Step 1: build the test-peer binary
# ---------------------------------------------------------------------------
step "1/6 Build the test-peer binary (target-peer/debug/desktop)"
if [ "${COCKPIT_SETUP_SKIP_PEER_BUILD:-0}" = "1" ] && [ -x "$PEER_BIN" ]; then
  if cockpit_runtime_assert_qa_artifact "$PEER_BIN"; then
    ok "Skipping rebuild (COCKPIT_SETUP_SKIP_PEER_BUILD=1); verified current-policy $PEER_BIN"
  else
    fail "test-peer binary exists but does not meet the #315 QA runtime policy"
  fi
else
  if "$ROOT/apps/desktop/scripts/build-test-peer.sh"; then
    :
  else
    fail "test-peer binary build failed -- see cargo output above"
  fi
fi
if [ -x "$PEER_BIN" ]; then
  ok "test-peer binary present: $PEER_BIN"
else
  fail "test-peer binary missing after build: $PEER_BIN"
fi

# ---------------------------------------------------------------------------
# Step 2: build the primary dev binary if missing
# ---------------------------------------------------------------------------
step "2/6 Build and verify the primary QA binary ($NATIVE_BIN)"
if "$ROOT/apps/desktop/scripts/build-cockpit-primary.sh"; then
  ok "Primary QA binary rebuilt with the #315 runtime policy: $NATIVE_BIN"
else
  fail "Primary QA binary build/runtime-policy verification failed"
fi

# ---------------------------------------------------------------------------
# Step 3: confirm the two binaries hold distinct identities (own
# app_data_dir()/socket, not just distinct file paths) -- counselors finding
# from the plan doc: don't just assume this, assert it.
# ---------------------------------------------------------------------------
step "3/6 Confirm primary + test-peer binaries hold DISTINCT identities"
if [ -x "$NATIVE_BIN" ] && [ -x "$PEER_BIN" ]; then
  peer_log="$(mktemp -t petal-cockpit-setup-peer-identity-log)"
  peer_sock="/tmp/petal-cockpit-setup-identity-check.sock"
  peer_pid="$(launch_binary "$PEER_BIN" "$peer_sock" "$peer_log")"
  sleep 1
  stop_binary "$peer_pid"
  if grep -q "rooms persistence loading from .*com\.petal\.app\.testpeer" "$peer_log"; then
    ok "test-peer binary resolves its OWN app_data_dir (com.petal.app.testpeer), distinct from the primary binary's com.petal.app"
  else
    fail "could not confirm test-peer binary uses a distinct app_data_dir -- check $peer_log"
  fi
else
  fail "cannot confirm distinct identities -- one or both binaries missing (see steps 1-2)"
fi

# ---------------------------------------------------------------------------
# Steps 4-5: walk the human through Screen Recording + Accessibility for
# BOTH binaries. TCC dialogs cannot be scripted -- this is the inherently
# interactive part of setup.
# ---------------------------------------------------------------------------
step "4/6 Grant Screen Recording + Accessibility: primary dev binary"
if [ -x "$NATIVE_BIN" ]; then
  grant_and_verify "$NATIVE_BIN" "Primary dev binary"
else
  fail "Primary dev binary missing -- cannot grant/verify"
fi

step "5/6 Grant Screen Recording + Accessibility: test-peer binary"
if [ -x "$PEER_BIN" ]; then
  grant_and_verify "$PEER_BIN" "Test-peer binary"
else
  fail "Test-peer binary missing -- cannot grant/verify"
fi

# ---------------------------------------------------------------------------
# Step 6: one-time Automation/AppleEvent consent for osascript -> TextEdit
# (dev-tier remote-control suite readback only). Running the AppleScript
# from this shell is what triggers macOS's one-time consent dialog for
# whatever process is running this script to control TextEdit; a
# non-prompting successful round-trip on a later attempt is exactly what a
# real grant looks like -- an ungranted attempt fails immediately with
# osascript error -1743, not a hang.
# ---------------------------------------------------------------------------
step "6/6 Automation/AppleEvent consent: osascript -> TextEdit"
osascript_probe() {
  osascript -e 'tell application "TextEdit" to make new document' \
            -e 'tell application "TextEdit" to close front document saving no' 2>&1
}
automation_result="$(osascript_probe)"
if printf '%s' "$automation_result" | grep -qi 'not authorized\|-1743'; then
  warn "Automation consent not yet granted for TextEdit."
  info "-> The OS should have just shown (or will show on the next attempt) an"
  info "   Automation permission dialog for this terminal/process to control"
  info "   TextEdit. Approve it, then re-run this script."
  read -r -t "$WAIT_TIMEOUT_S" -p "  Press Enter once approved to re-check (waits up to ${WAIT_TIMEOUT_S}s)... " _ans
  automation_result="$(osascript_probe)"
fi
if printf '%s' "$automation_result" | grep -qi 'not authorized\|-1743'; then
  fail "Automation consent for osascript -> TextEdit still not granted"
else
  ok "Automation consent for osascript -> TextEdit confirmed"
fi

# ---------------------------------------------------------------------------
# Sudoers snippet for scripts/net-impair.sh (#261's CHAOS-NET). PRINTED ONLY.
#
# Installing a persistent passwordless-sudo (NOPASSWD) rule is a
# system-level security change this script deliberately does NOT automate --
# it is handed back to the human running this script. Copy the snippet
# below and install it yourself with `sudo visudo -f ...`, which validates
# the syntax atomically before it takes effect (the same safety property an
# automated installer would otherwise have to reimplement with `visudo -c`).
# ---------------------------------------------------------------------------
step "ACTION REQUIRED (not automated by this script): net-impair.sh sudoers entry"
SUDOERS_USER="$(whoami)"
cat <<EOF

This step is intentionally NOT automated. Installing a NOPASSWD sudoers rule
is a system-level security change -- run these commands yourself:

  sudo visudo -f /etc/sudoers.d/petal-net-impair

Paste exactly this into the editor visudo opens, then save and exit
(visudo validates the syntax before installing; it will refuse to save an
invalid file, so there is no way to leave /etc/sudoers.d in a broken state):

  -----8<----- BEGIN /etc/sudoers.d/petal-net-impair -----8<-----
  # Petal Test Cockpit -- CHAOS-NET network impairment (#253 / #261).
  # Grants ${SUDOERS_USER} passwordless sudo ONLY to run this one script at
  # this exact path -- nothing else. #261 implements the real pfctl/dnctl
  # body; today the script is a documented no-op (scripts/net-impair.sh).
  ${SUDOERS_USER} ALL=(root) NOPASSWD: ${NET_IMPAIR_SCRIPT}
  -----8<------ END /etc/sudoers.d/petal-net-impair ------8<-----

Notes:
  - Use the exact absolute path above (${NET_IMPAIR_SCRIPT}). sudoers matches
    the literal command path; a relative path or a different checkout
    location will not match.
  - Do not make /etc/sudoers.d/petal-net-impair a symlink -- sudo's parser
    ignores symlinked/world-writable entries under /etc/sudoers.d for
    security. visudo -f creates a real file with the correct 0440
    root-owned permissions for you.
  - This grants sudo for exactly one script path, never a shell, and never
    any other command.
  - This script cannot verify this step (checking it would itself require
    invoking sudo, which this script does not do). Confirm success
    yourself with: sudo -l -U ${SUDOERS_USER} | grep net-impair.sh
    (run that check yourself outside this script, when you're ready).

EOF
info "This step is tracked as a known manual action on GitHub issue #253 --"
info "it does not block this script's own exit code."

# ---------------------------------------------------------------------------
# Final verification + marker file
# ---------------------------------------------------------------------------
step "Summary"
if [ "$OVERALL_OK" -eq 1 ]; then
  for identifier in "${MARKER_IDENTIFIERS[@]}"; do
    marker="$MARKER_ROOT/$identifier/.cockpit-setup-complete"
    mkdir -p "${marker%/*}"
    {
      echo "cockpit-setup completed: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
      echo "primary_bin=$NATIVE_BIN"
      echo "peer_bin=$PEER_BIN"
      echo "identity=$identifier"
      echo "note: this marker records completed setup; it is not a TCC grant"
      echo "note: sudoers entry for net-impair.sh is a separate manual step (see above), not covered by this marker"
    } >"$marker"
    ok "All automatable grants confirmed. Wrote marker for $identifier: $marker"
  done
  info "Remaining manual step: install the net-impair.sh sudoers entry printed above (#261 prerequisite)."
  exit 0
else
  printf '\n\033[1;31mSetup INCOMPLETE -- missing:\033[0m\n'
  for m in "${MISSING[@]}"; do
    printf '  - %s\n' "$m"
  done
  printf '\nRe-run this script after addressing the items above. No marker file was written -- INFRA-FAIL: run scripts/cockpit-setup.sh will keep firing until it is.\n'
  exit 1
fi

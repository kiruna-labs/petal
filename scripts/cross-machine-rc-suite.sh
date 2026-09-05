#!/usr/bin/env bash
# Run the remote-control 30-case live suite across two real Macs.
#
# Controller role: this Mac runs livekit-server, web-harness, and Chrome/CDP.
# Sharer role: PETAL_REMOTE_HOST runs a Developer-ID-signed Petal.app bundle
# plus the sacrificial TextEdit target. The Petal autotest Unix socket is
# forwarded back over SSH; the app itself never opens a network control port.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO"

REMOTE_HOST="${PETAL_REMOTE_HOST:-}"
REMOTE_APP_DIR="${PETAL_REMOTE_APP_DIR:-/tmp/petal-cross-machine-test}"
REMOTE_SOCKET="/tmp/petal-remote-rc.sock"
LOCAL_FORWARD_SOCKET="/tmp/petal-remote-rc-forwarded.sock"
RESULTS_JSON=""
EVIDENCE_DIR=""
EVIDENCE_ROOT="${PETAL_CROSS_MACHINE_EVIDENCE_DIR:-${TMPDIR:-/tmp}/petal-cross-machine-evidence}"
RAW_DIR=""
SANITIZED_RESULTS_JSON=""
LOCAL_MANIFEST=""
REMOTE_MANIFEST_LOCAL=""
REMOTE_MANIFEST_NAME="rc-cross-machine-remote-manifest.json"
RAW_SUITE_OUTPUT=""
LIVEKIT_API_KEY_VALUE="${LIVEKIT_API_KEY:-devkey}"
LIVEKIT_API_SECRET_VALUE="${LIVEKIT_API_SECRET:-secret}"
AUTOTEST_ROOM="${PETAL_AUTOTEST_ROOM:-rctest}"
AUTOTEST_IDENTITY="${PETAL_AUTOTEST_IDENTITY:-remote-sharer-autotest}"
SIGNING_IDENTITY="Developer ID Application: Kiruna Labs, Inc. (X83RP84J8Z)"
EXPECTED_TEAM_ID="X83RP84J8Z"
EXPECTED_BUNDLE_ID="com.petal.app"
EXPECTED_BUNDLE_VERSION=""
QA_APP_BUNDLE="$REPO/apps/desktop/src-tauri/target/universal-apple-darwin/release/bundle/macos/Petal.app"
QA_PREBUILT_ARCHIVE="${PETAL_QA_APP_ARCHIVE:-}"
QA_PREBUILT_BUNDLE="${PETAL_QA_APP_BUNDLE:-}"
QA_PREBUILT_ARCHIVE_HASH=""
QA_PREBUILT_MODE=0
PREBUILT_EXTRACT_DIR=""
PLIST_BUDDY="${PETAL_PLIST_BUDDY:-/usr/libexec/PlistBuddy}"
SSH_FORWARD_PID=""
REMOTE_COMMAND_WRAPPER_DIR=""
REMOTE_CLEANUP_ENABLED=0
SOURCE_COMMIT=""

log() {
  echo "== $* =="
}

warn() {
  echo "WARN: $*" >&2
}

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

remote_path_quote() {
  printf "%q" "$1"
}

remote_command() {
  local quoted=()
  local arg
  for arg in "$@"; do
    quoted+=("$(remote_path_quote "$arg")")
  done
  local IFS=" "
  printf "%s" "${quoted[*]}"
}

ssh_remote() {
  ssh -o BatchMode=yes "$REMOTE_HOST" "$@"
}

ssh_remote_command() {
  ssh -o BatchMode=yes "$REMOTE_HOST" "$(remote_command "$@")"
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM

  if [[ -n "$SSH_FORWARD_PID" ]] && kill -0 "$SSH_FORWARD_PID" 2>/dev/null; then
    kill "$SSH_FORWARD_PID" 2>/dev/null || true
    wait "$SSH_FORWARD_PID" 2>/dev/null || true
  fi
  rm -f "$LOCAL_FORWARD_SOCKET"
  rm -f "$RAW_SUITE_OUTPUT" "$RESULTS_JSON"
  [[ -z "$RAW_DIR" ]] || rm -rf "$RAW_DIR"
  [[ -z "$PREBUILT_EXTRACT_DIR" ]] || rm -rf "$PREBUILT_EXTRACT_DIR"

  if [[ -n "$REMOTE_COMMAND_WRAPPER_DIR" ]]; then
    rm -rf "$REMOTE_COMMAND_WRAPPER_DIR"
  fi

  if [[ "$REMOTE_CLEANUP_ENABLED" == "1" ]]; then
    ssh -o BatchMode=yes -o ConnectTimeout=5 "$REMOTE_HOST" "$(remote_command bash -s -- "$REMOTE_APP_DIR" "$REMOTE_SOCKET" "${PETAL_REMOTE_KEEP_APP:-0}")" <<'REMOTE_CLEANUP' || true
set -euo pipefail
app_dir="$1"
sock="$2"
keep_app="$3"

osascript -e 'tell application "TextEdit" to quit' >/dev/null 2>&1 || true
pkill -9 -x TextEdit >/dev/null 2>&1 || true

# #846: same fix as launch_remote_app -- the old `osascript quit "Petal"` +
# unanchored `pkill -f "Petal.app/Contents/MacOS/desktop"` here could resolve
# through LaunchServices or substring-match the user's installed
# /Applications/Petal.app and SIGKILL a live meeting. Refuse on a foreign
# instance; kill only PIDs anchored under this test bundle's own path.
assert_no_foreign_petal() {
  local expected="$1" pid cmd found=0
  while read -r pid; do
    [[ -z "$pid" ]] && continue
    cmd="$(ps -p "$pid" -o command= 2>/dev/null || true)"
    [[ -n "$cmd" ]] || continue
    case "$cmd" in
      "$expected"*) continue ;;
    esac
    echo "FATAL: a Petal instance is already running -- not mine. Refusing to proceed." >&2
    ps -p "$pid" -o pid=,etime=,command= 2>/dev/null | sed 's/^/       /' >&2
    found=1
  done < <(pgrep -f "Contents/MacOS/desktop" 2>/dev/null || true)
  [[ "$found" -eq 0 ]] || exit 97
}
assert_no_foreign_petal "$app_dir/Petal.app/Contents/MacOS/desktop"
pkill -f "$app_dir/Petal.app/Contents/MacOS/desktop" >/dev/null 2>&1 || true
rm -f "$sock"

launchctl unsetenv LIVEKIT_URL >/dev/null 2>&1 || true
launchctl unsetenv LIVEKIT_API_KEY >/dev/null 2>&1 || true
launchctl unsetenv LIVEKIT_API_SECRET >/dev/null 2>&1 || true
launchctl unsetenv PETAL_DISABLE_AUDIO >/dev/null 2>&1 || true
launchctl unsetenv PETAL_AUTOTEST_ROOM >/dev/null 2>&1 || true
launchctl unsetenv PETAL_AUTOTEST_IDENTITY >/dev/null 2>&1 || true
launchctl unsetenv PETAL_AUTOTEST_SOCK >/dev/null 2>&1 || true
launchctl unsetenv PETAL_REMOTE_CONTROL_DIRECT_SCROLL >/dev/null 2>&1 || true
launchctl unsetenv PETAL_REMOTE_CONTROL_DIRECT_DRAG >/dev/null 2>&1 || true
launchctl unsetenv PETAL_REMOTE_CONTROL_DIRECT_CLICK >/dev/null 2>&1 || true

if [[ "$keep_app" != "1" ]]; then
  rm -rf "$app_dir/Petal.app"
fi
REMOTE_CLEANUP
  fi

  # Raw output is always removed above, including reducer failures and
  # signals. The already allowlisted evidence directory is retained by
  # default under EVIDENCE_ROOT for classified Intel-run diagnosis.

  exit "$status"
}
trap cleanup EXIT INT TERM

require_remote_host() {
  if [[ -z "$REMOTE_HOST" ]]; then
    fail "PETAL_REMOTE_HOST is required. Configure key-based SSH access to a second real Mac before running this harness."
  fi
}

preflight_ssh() {
  log "preflight: SSH reachability"
  local output
  if ! output="$(ssh -o BatchMode=yes -o ConnectTimeout=5 "$REMOTE_HOST" true 2>&1)"; then
    if grep -Eiq "permission denied|publickey|authentication" <<<"$output"; then
      fail "SSH authentication failed. Configure key-based access and retry."
    fi
    fail "SSH reachability preflight failed. Verify Remote Login and key-based SSH, then retry."
  fi
}

local_translation_state() {
  local translated
  translated="$(sysctl -in sysctl.proc_translated 2>/dev/null || true)"
  printf '%s' "${translated:-0}"
}

local_arm_capability() {
  sysctl -in hw.optional.arm64 2>/dev/null || true
}

validate_physical_architecture() {
  local label="$1" arch="$2" translated="$3" arm_capability="$4"
  case "$translated" in 0) ;; 1) fail "$label is running translated/Rosetta code; physical architecture evidence is invalid." ;; *) fail "$label reported an invalid translation state." ;; esac
  case "$arch" in arm64|x86_64) ;; *) fail "$label reported unsupported architecture '$arch'; only physical arm64 and x86_64 are accepted." ;; esac
  case "$arm_capability" in 0|1) ;; *) fail "$label did not provide a valid physical Apple-Silicon capability." ;; esac
  if [[ "$arch" == "arm64" && "$arm_capability" != "1" ]]; then
    fail "$label architecture/capability disagreement (arm64 without Apple-Silicon capability)."
  fi
  if [[ "$arch" == "x86_64" && "$arm_capability" != "0" ]]; then
    fail "$label architecture/capability disagreement (x86_64 with Apple-Silicon capability)."
  fi
}

preflight_architecture() {
  log "preflight: physical architecture"
  local local_arch local_translated local_arm remote_probe remote_arch remote_translated remote_arm
  local_arch="$(uname -m)"
  local_translated="$(local_translation_state)"
  local_arm="$(local_arm_capability)"
  remote_probe="$(ssh_remote_command bash -s <<'REMOTE_ARCH' | tr -d '\r'
set -euo pipefail
translated="$(sysctl -in sysctl.proc_translated 2>/dev/null || true)"
arm_capability="$(sysctl -in hw.optional.arm64 2>/dev/null || true)"
printf '%s|%s|%s' "$(uname -m)" "${translated:-0}" "$arm_capability"
REMOTE_ARCH
)"
  IFS='|' read -r remote_arch remote_translated remote_arm <<<"$remote_probe"
  validate_physical_architecture "local peer" "$local_arch" "$local_translated" "$local_arm"
  validate_physical_architecture "remote peer" "$remote_arch" "$remote_translated" "$remote_arm"
  echo "local arch:  $local_arch"
  echo "remote arch: $remote_arch"
}

preflight_macos_versions() {
  log "preflight: macOS versions"
  local local_version remote_version
  local_version="$(sw_vers -productVersion)"
  remote_version="$(ssh_remote sw_vers -productVersion | tr -d '\r')"
  [[ "$local_version" =~ ^[0-9]+(\.[0-9]+)*$ ]] || fail "Local macOS version was malformed."
  [[ "$remote_version" =~ ^[0-9]+(\.[0-9]+)*$ ]] || fail "Remote macOS version was malformed."
  LOCAL_MACOS_MAJOR="${local_version%%.*}"
  REMOTE_MACOS_MAJOR="${remote_version%%.*}"
  echo "local macOS major:  $LOCAL_MACOS_MAJOR"
  echo "remote macOS major: $REMOTE_MACOS_MAJOR"
}

preflight_gui_session() {
  log "preflight: remote GUI session"
  if ssh_remote "who | grep -q '[[:space:]]console[[:space:]]'" >/dev/null 2>&1; then
    echo "remote console session: present"
  else
    fail "No active remote Aqua/console session was detected; do not deploy or launch a GUI app over SSH without it."
  fi
}

preflight_remote_osascript() {
  log "preflight: remote osascript over SSH"
  local output
  if ! output="$(ssh_remote_command osascript -e 'return 1' 2>&1)"; then
    fail "Remote AppleEvent preflight failed. Allow the SSH-controlled session to control apps if prompted, then retry."
  fi
  output="$(tr -d '\r' <<<"$output")"
  if [[ "$output" != "1" ]]; then
    fail "Remote AppleEvent preflight returned an unexpected value. Fix AppleEvents-over-SSH before running the suite."
  fi
}

print_human_checklist() {
  log "manual remote permission checklist"
  cat <<EOF
Before the live suite runs, the deployed test bundle on the remote Mac must
already have Screen Recording AND Accessibility permission.

This script signs Petal.app with the stable Developer ID identity:
  $SIGNING_IDENTITY

That stable signature is what lets those TCC grants survive redeploys. Do not
use an ad-hoc signed bundle for this cross-machine test path.
EOF
}

detect_lan_ip() {
  local iface ip
  iface="$(route get default 2>/dev/null | awk '/interface:/{print $2; exit}')"
  if [[ -n "$iface" ]]; then
    ip="$(ipconfig getifaddr "$iface" 2>/dev/null || true)"
    if [[ -n "$ip" ]]; then
      printf "%s\n" "$ip"
      return 0
    fi
  fi

  for iface in en0 en1 en2 bridge100; do
    ip="$(ipconfig getifaddr "$iface" 2>/dev/null || true)"
    if [[ -n "$ip" ]]; then
      printf "%s\n" "$ip"
      return 0
    fi
  done

  ifconfig | awk '/inet / && $2 !~ /^127\./ {print $2; exit}'
}

start_controller_services() {
  log "0. verify per-window capture is healthy"
  rm -f /tmp/_cap_probe.png
  if screencapture -x -D 1 /tmp/_cap_probe.png >/dev/null 2>&1 && [[ -s /tmp/_cap_probe.png ]]; then
    echo "per-window capture probe: ok"
  else
    warn "screencapture probe did not produce /tmp/_cap_probe.png; Screen Recording may need attention before the suite can pass."
  fi

  log "1. livekit-server"
  pgrep -f "livekit-server --dev" >/dev/null || (nohup livekit-server --dev >/tmp/livekit.log 2>&1 & sleep 2)

  log "2. web-harness :5185"
  lsof -iTCP:5185 -sTCP:LISTEN >/dev/null 2>&1 || (cd "$REPO/web-harness" && nohup npx vite --port 5185 --strictPort >/tmp/webharness.log 2>&1 & sleep 4)

  log "3. Chrome + CDP :9222"
  lsof -iTCP:9222 -sTCP:LISTEN >/dev/null 2>&1 || (rm -rf /tmp/petal-cdp-chrome; nohup "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --remote-debugging-port=9222 --user-data-dir=/tmp/petal-cdp-chrome --no-first-run --no-default-browser-check "http://localhost:5185/" >/tmp/chrome-cdp.log 2>&1 & sleep 5)
}

build_signed_app() {
  log "4. build universal Developer-ID-signed QA Petal.app with autotest feature"
  (
    cd "$REPO/apps/desktop"
    "$REPO/scripts/run-with-source-provenance.sh" --require-clean env \
      DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
      RUSTFLAGS="" \
      MACOSX_DEPLOYMENT_TARGET=13.0 \
      APPLE_SIGNING_IDENTITY="$SIGNING_IDENTITY" \
      bash -c \
      'npm ci && CARGO_TARGET_DIR="$PETAL_PROVENANCE_OUTPUT_ROOT/apps/desktop/src-tauri/target" npx tauri build --target universal-apple-darwin --bundles app --features autotest'
  )

  [[ -d "$QA_APP_BUNDLE" ]] || fail "Expected staged universal QA app bundle, but it was not found."
  LOCAL_BINARY_HASH="$(verify_app_bundle "$QA_APP_BUNDLE" 0)"
  write_manifest "$LOCAL_MANIFEST" "$(uname -m)" "$LOCAL_MACOS_MAJOR" "$LOCAL_BINARY_HASH" "$(lipo -archs "$QA_APP_BUNDLE/Contents/MacOS/desktop")"
}

verify_app_bundle() {
  local app_bundle="$1" require_notarization="$2" binary archs details bundle_id version hash
  binary="$app_bundle/Contents/MacOS/desktop"
  [[ -x "$binary" ]] || fail "QA bundle executable is missing."
  archs="$(lipo -archs "$binary" 2>/dev/null || true)"
  [[ "$archs" == "arm64 x86_64" || "$archs" == "x86_64 arm64" ]] || fail "QA bundle must contain exactly arm64 and x86_64 slices."
  codesign --verify --deep --strict "$app_bundle" >/dev/null 2>&1 || fail "QA bundle strict signature verification failed."
  details="$(codesign -dvv "$app_bundle" 2>&1 || true)"
  grep -Fq "Identifier=$EXPECTED_BUNDLE_ID" <<<"$details" || fail "QA bundle identifier did not match the expected identifier."
  grep -Fq "TeamIdentifier=$EXPECTED_TEAM_ID" <<<"$details" || fail "QA bundle TeamIdentifier did not match the expected Developer ID team."
  grep -Eq 'flags=.*runtime' <<<"$details" || fail "QA bundle is missing the hardened runtime."
  if [[ -n "$EXPECTED_BUNDLE_VERSION" ]]; then
    version="$("$PLIST_BUDDY" -c 'Print :CFBundleShortVersionString' "$app_bundle/Contents/Info.plist" 2>/dev/null || true)"
    [[ "$version" == "$EXPECTED_BUNDLE_VERSION" ]] || fail "QA bundle version did not match the immutable QA version."
  fi
  if [[ "$require_notarization" == "1" ]]; then
    xcrun stapler validate "$app_bundle" >/dev/null 2>&1 || fail "QA bundle stapler validation failed."
    spctl -a -vv -t exec "$app_bundle" >/dev/null 2>&1 || fail "QA bundle Gatekeeper assessment failed."
  fi
  hash="$(shasum -a 256 "$binary" | awk '{print $1}')"
  [[ "$hash" =~ ^[0-9a-fA-F]{64}$ ]] || fail "QA bundle SHA-256 was malformed."
  printf '%s' "$hash"
}

prepare_prebuilt_qa_bundle() {
  [[ -n "$QA_PREBUILT_BUNDLE" && -n "$QA_PREBUILT_ARCHIVE" ]] || fail "PETAL_QA_APP_BUNDLE and PETAL_QA_APP_ARCHIVE are both required for prebuilt QA mode."
  [[ -f "$QA_PREBUILT_ARCHIVE" ]] || fail "PETAL_QA_APP_ARCHIVE must name the supplied QA ZIP."
  [[ "$(basename "$QA_PREBUILT_BUNDLE")" == "Petal.app" && -d "$QA_PREBUILT_BUNDLE" ]] || fail "PETAL_QA_APP_BUNDLE must name an extracted Petal.app."
  QA_PREBUILT_ARCHIVE_HASH="$(shasum -a 256 "$QA_PREBUILT_ARCHIVE" | awk '{print $1}')"
  [[ "$QA_PREBUILT_ARCHIVE_HASH" =~ ^[0-9a-fA-F]{64}$ ]] || fail "QA archive SHA-256 was malformed."
  unzip -tqq "$QA_PREBUILT_ARCHIVE" >/dev/null || fail "QA archive integrity verification failed."
  PREBUILT_EXTRACT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/petal-qa-prebuilt.XXXXXX")"
  unzip -q "$QA_PREBUILT_ARCHIVE" -d "$PREBUILT_EXTRACT_DIR" || fail "QA archive controlled extraction failed."
  local extracted="$PREBUILT_EXTRACT_DIR/Petal.app"
  [[ -d "$extracted" ]] || fail "QA archive must extract one top-level Petal.app."
  [[ "$(shasum -a 256 "$QA_PREBUILT_BUNDLE/Contents/MacOS/desktop" | awk '{print $1}')" == "$(shasum -a 256 "$extracted/Contents/MacOS/desktop" | awk '{print $1}')" ]] || fail "PETAL_QA_APP_BUNDLE does not match the supplied QA archive."
  QA_APP_BUNDLE="$extracted"
  EXPECTED_BUNDLE_ID="com.petal.app.qa"
  EXPECTED_BUNDLE_VERSION="0.7.12"
  QA_PREBUILT_MODE=1
  LOCAL_BINARY_HASH="$(verify_app_bundle "$QA_APP_BUNDLE" 1)"
  write_manifest "$LOCAL_MANIFEST" "$(uname -m)" "$LOCAL_MACOS_MAJOR" "$LOCAL_BINARY_HASH" "$(lipo -archs "$QA_APP_BUNDLE/Contents/MacOS/desktop")"
}

write_manifest() {
  local destination="$1" architecture="$2" macos_major="$3" binary_hash="$4" lipo_archs="$5"
  [[ "$architecture" == "arm64" || "$architecture" == "x86_64" ]] || fail "Manifest architecture was not allowlisted."
  [[ "$macos_major" =~ ^[0-9]+$ ]] || fail "Manifest macOS version was not major-only."
  [[ "$binary_hash" =~ ^[0-9a-fA-F]{64}$ ]] || fail "Manifest hash was malformed."
  [[ "$lipo_archs" == "arm64 x86_64" || "$lipo_archs" == "x86_64 arm64" ]] || fail "Manifest lipo architecture set was not exact."
  local temporary="${destination}.tmp.$$"
  (umask 077 && printf '{"architecture":"%s","macosMajor":%s,"sourceCommit":"%s","lipoArchs":"%s","team":"%s","binarySha256":"%s","resultRef":"cross-machine-summary.json","inputRoute":"packaged-default"}\n' \
    "$architecture" "$macos_major" "$SOURCE_COMMIT" "$lipo_archs" "$EXPECTED_TEAM_ID" "$binary_hash" >"$temporary")
  mv -f "$temporary" "$destination"
}

deploy_app() {
  log "5. deploy verified universal QA Petal.app to remote"
  [[ "$REMOTE_APP_DIR" =~ ^/[-A-Za-z0-9_./]+$ ]] || fail "Remote app directory must be an absolute safe path."
  # All architecture, Aqua, and AppleEvent preflights have passed before this
  # first remote mutation. From here cleanup owns a partially deployed app.
  REMOTE_CLEANUP_ENABLED=1
  ssh_remote_command bash -s -- "$REMOTE_APP_DIR" <<'REMOTE_PREP'
set -euo pipefail
mkdir -p "$1"
rm -rf "$1/Petal.app"
REMOTE_PREP

  local quoted_remote_dir
  quoted_remote_dir="$(remote_path_quote "$REMOTE_APP_DIR")"
  if [[ "$QA_PREBUILT_MODE" == "1" ]]; then
    rsync -a --delete "$QA_PREBUILT_ARCHIVE" "$REMOTE_HOST:$quoted_remote_dir/Petal.qa.zip"
    ssh_remote_command bash -s -- "$REMOTE_APP_DIR" "$QA_PREBUILT_ARCHIVE_HASH" <<'REMOTE_EXTRACT'
set -euo pipefail
app_dir="$1"
archive_hash="$2"
archive="$app_dir/Petal.qa.zip"
[[ "$(shasum -a 256 "$archive" | awk '{print $1}')" == "$archive_hash" ]] || exit 80
unzip -tqq "$archive" >/dev/null || exit 81
stage="$app_dir/.qa-extract.$$"
rm -rf "$stage"
mkdir -p "$stage"
unzip -q "$archive" -d "$stage" || exit 82
[[ -d "$stage/Petal.app" ]] || exit 83
rm -rf "$app_dir/Petal.app"
mv "$stage/Petal.app" "$app_dir/Petal.app"
rm -rf "$stage"
REMOTE_EXTRACT
  else
  rsync -a --delete "$QA_APP_BUNDLE/" "$REMOTE_HOST:$quoted_remote_dir/Petal.app/"
  fi
  verify_remote_app "$LOCAL_BINARY_HASH"
}

verify_remote_app() {
  local expected_hash="$1" remote_hash remote_manifest
  remote_manifest="$REMOTE_APP_DIR/$REMOTE_MANIFEST_NAME"
  remote_hash="$(ssh_remote_command bash -s -- "$REMOTE_APP_DIR" "$expected_hash" "$EXPECTED_BUNDLE_ID" "$EXPECTED_BUNDLE_VERSION" "$EXPECTED_TEAM_ID" "$REMOTE_MACOS_MAJOR" "$REMOTE_MANIFEST_NAME" "$SOURCE_COMMIT" "$QA_PREBUILT_MODE" "$PLIST_BUDDY" <<'REMOTE_VERIFY' | tr -d '\r'
set -euo pipefail
app_dir="$1"
expected_hash="$2"
expected_bundle_id="$3"
expected_version="$4"
expected_team="$5"
macos_major="$6"
manifest_name="$7"
source_commit="$8"
prebuilt_mode="$9"
plist_buddy="${10}"
app="$app_dir/Petal.app"
bin="$app/Contents/MacOS/desktop"
[[ -x "$bin" ]] || exit 70
archs="$(lipo -archs "$bin" 2>/dev/null || true)"
if [[ "$archs" != "arm64 x86_64" && "$archs" != "x86_64 arm64" ]]; then exit 71; fi
codesign --verify --deep --strict "$app" >/dev/null 2>&1 || exit 72
details="$(codesign -dvv "$app" 2>&1 || true)"
grep -Fq "Identifier=$expected_bundle_id" <<<"$details" || exit 73
grep -Fq "TeamIdentifier=$expected_team" <<<"$details" || exit 74
grep -Eq 'flags=.*runtime' <<<"$details" || exit 75
if [[ -n "$expected_version" ]]; then
  [[ "$("$plist_buddy" -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist" 2>/dev/null || true)" == "$expected_version" ]] || exit 84
fi
if [[ "$prebuilt_mode" == "1" ]]; then
  xcrun stapler validate "$app" >/dev/null 2>&1 || exit 85
  spctl -a -vv -t exec "$app" >/dev/null 2>&1 || exit 86
fi
hash="$(shasum -a 256 "$bin" | awk '{print $1}')"
[[ "$hash" == "$expected_hash" ]] || exit 76
arch="$(uname -m)"
if [[ "$arch" != "arm64" && "$arch" != "x86_64" ]]; then exit 77; fi
[[ "$macos_major" =~ ^[0-9]+$ ]] || exit 78
tmp_manifest="$app_dir/.${manifest_name}.tmp.$$"
(umask 077 && printf '{"architecture":"%s","macosMajor":%s,"sourceCommit":"%s","lipoArchs":"%s","team":"%s","binarySha256":"%s","resultRef":"cross-machine-summary.json","inputRoute":"packaged-default"}\n' "$arch" "$macos_major" "$source_commit" "$archs" "$expected_team" "$hash" >"$tmp_manifest")
mv -f "$tmp_manifest" "$app_dir/$manifest_name"
printf '%s' "$hash"
REMOTE_VERIFY
)"
  [[ "$remote_hash" == "$expected_hash" ]] || fail "Remote QA bundle hash did not match the staged bundle."
  scp -q "$REMOTE_HOST:$(remote_path_quote "$remote_manifest")" "$REMOTE_MANIFEST_LOCAL" || fail "Could not retrieve the allowlisted remote manifest."
}

launch_remote_app() {
  local lan_ip="$1"
  local livekit_url="ws://$lan_ip:7880"

  log "6. launch remote Petal.app"
  ssh_remote_command bash -s -- "$REMOTE_APP_DIR" "$REMOTE_SOCKET" "$livekit_url" "$LIVEKIT_API_KEY_VALUE" "$LIVEKIT_API_SECRET_VALUE" "$AUTOTEST_ROOM" "$AUTOTEST_IDENTITY" <<'REMOTE_LAUNCH'
set -euo pipefail
app_dir="$1"
sock="$2"
livekit_url="$3"
api_key="$4"
api_secret="$5"
room="$6"
identity="$7"
app="$app_dir/Petal.app"

[[ -d "$app" ]] || exit 79

# #846: the old code here was `osascript -e 'tell application "Petal" to quit'`
# then a bare `pkill -f "Petal.app/Contents/MacOS/desktop"`. Both match ANY
# Petal.app -- the by-name quit resolves through LaunchServices (which may
# pick the installed /Applications/Petal.app, not this QA bundle), and the
# pkill pattern is an unanchored substring that /Applications/Petal.app's own
# command line also contains. A user's live installed app was SIGKILLed by
# exactly this (four times in 90 minutes, 2026-08-20). Refuse instead of
# clearing the way; kill only PIDs whose full command line is anchored under
# THIS test bundle's own path.
assert_no_foreign_petal() {
  local expected="$1" pid cmd found=0
  while read -r pid; do
    [[ -z "$pid" ]] && continue
    cmd="$(ps -p "$pid" -o command= 2>/dev/null || true)"
    [[ -n "$cmd" ]] || continue
    case "$cmd" in
      "$expected"*) continue ;;
    esac
    echo "FATAL: a Petal instance is already running -- not mine. Refusing to proceed." >&2
    ps -p "$pid" -o pid=,etime=,command= 2>/dev/null | sed 's/^/       /' >&2
    found=1
  done < <(pgrep -f "Contents/MacOS/desktop" 2>/dev/null || true)
  [[ "$found" -eq 0 ]] || exit 97
}
assert_no_foreign_petal "$app/Contents/MacOS/desktop"
pkill -f "$app/Contents/MacOS/desktop" >/dev/null 2>&1 || true
rm -f "$sock"

launchctl setenv LIVEKIT_URL "$livekit_url"
launchctl setenv LIVEKIT_API_KEY "$api_key"
launchctl setenv LIVEKIT_API_SECRET "$api_secret"
launchctl setenv PETAL_DISABLE_AUDIO "1"
launchctl setenv PETAL_AUTOTEST_ROOM "$room"
launchctl setenv PETAL_AUTOTEST_IDENTITY "$identity"
launchctl setenv PETAL_AUTOTEST_SOCK "$sock"
# The cross-machine result is packaged-default evidence. Never inherit a
# developer's direct SkyLight route from launchd into this QA app.
launchctl unsetenv PETAL_REMOTE_CONTROL_DIRECT_SCROLL >/dev/null 2>&1 || true
launchctl unsetenv PETAL_REMOTE_CONTROL_DIRECT_DRAG >/dev/null 2>&1 || true
launchctl unsetenv PETAL_REMOTE_CONTROL_DIRECT_CLICK >/dev/null 2>&1 || true

env -u PETAL_REMOTE_CONTROL_DIRECT_SCROLL \
  -u PETAL_REMOTE_CONTROL_DIRECT_DRAG \
  -u PETAL_REMOTE_CONTROL_DIRECT_CLICK \
  open -n "$app"
REMOTE_LAUNCH
}

wait_for_remote_socket() {
  log "7. wait for remote autotest socket"
  for _ in $(seq 1 60); do
    if ssh_remote_command test -S "$REMOTE_SOCKET" >/dev/null 2>&1; then
      echo "remote autotest socket: ready"
      return 0
    fi
    sleep 1
  done
  fail "Timed out waiting for the remote autotest socket. Confirm the manually granted Screen Recording and Accessibility prerequisites."
}

start_socket_forward() {
  log "8. forward remote autotest socket"
  rm -f "$LOCAL_FORWARD_SOCKET"
  ssh -o BatchMode=yes -o ExitOnForwardFailure=yes -L "$LOCAL_FORWARD_SOCKET:$REMOTE_SOCKET" "$REMOTE_HOST" -N &
  SSH_FORWARD_PID=$!

  for _ in $(seq 1 20); do
    if [[ -S "$LOCAL_FORWARD_SOCKET" ]]; then
      echo "local forwarded socket: ready"
      return 0
    fi
    if ! kill -0 "$SSH_FORWARD_PID" 2>/dev/null; then
      fail "SSH socket forward exited before $LOCAL_FORWARD_SOCKET appeared."
    fi
    sleep 0.5
  done
  fail "Timed out waiting for local forwarded socket $LOCAL_FORWARD_SOCKET."
}

create_remote_command_wrappers() {
  log "9. prepare remote host command wrappers"
  REMOTE_COMMAND_WRAPPER_DIR="$(mktemp -d /tmp/petal-remote-commands.XXXXXX)"

  cat >"$REMOTE_COMMAND_WRAPPER_DIR/open" <<'WRAP_OPEN'
#!/usr/bin/env bash
set -euo pipefail
host="${PETAL_REMOTE_COMMAND_HOST:?}"
if [[ "$#" -ge 3 && "$1" == "-a" && "$2" == "TextEdit" ]]; then
  target="${@: -1}"
  if [[ -f "$target" ]]; then
    remote_dir="$(dirname "$target")"
    ssh -o BatchMode=yes "$host" mkdir -p "$remote_dir"
    scp -q "$target" "$host:$target"
  fi
fi
exec ssh -o BatchMode=yes "$host" open "$@"
WRAP_OPEN

  cat >"$REMOTE_COMMAND_WRAPPER_DIR/pkill" <<'WRAP_PKILL'
#!/usr/bin/env bash
set -euo pipefail
host="${PETAL_REMOTE_COMMAND_HOST:?}"
exec ssh -o BatchMode=yes "$host" pkill "$@"
WRAP_PKILL

  cat >"$REMOTE_COMMAND_WRAPPER_DIR/pbcopy" <<'WRAP_PBCOPY'
#!/usr/bin/env bash
set -euo pipefail
host="${PETAL_REMOTE_COMMAND_HOST:?}"
exec ssh -o BatchMode=yes "$host" pbcopy
WRAP_PBCOPY

  cat >"$REMOTE_COMMAND_WRAPPER_DIR/pbpaste" <<'WRAP_PBPASTE'
#!/usr/bin/env bash
set -euo pipefail
host="${PETAL_REMOTE_COMMAND_HOST:?}"
exec ssh -o BatchMode=yes "$host" pbpaste
WRAP_PBPASTE

  cat >"$REMOTE_COMMAND_WRAPPER_DIR/defaults" <<'WRAP_DEFAULTS'
#!/usr/bin/env bash
set -euo pipefail
host="${PETAL_REMOTE_COMMAND_HOST:?}"
exec ssh -o BatchMode=yes "$host" defaults "$@"
WRAP_DEFAULTS

  cat >"$REMOTE_COMMAND_WRAPPER_DIR/sample" <<'WRAP_SAMPLE'
#!/usr/bin/env bash
set -euo pipefail
host="${PETAL_REMOTE_COMMAND_HOST:?}"
exec ssh -o BatchMode=yes "$host" sample "$@"
WRAP_SAMPLE

  cat >"$REMOTE_COMMAND_WRAPPER_DIR/screencapture" <<'WRAP_SCREENCAPTURE'
#!/usr/bin/env bash
set -euo pipefail
host="${PETAL_REMOTE_COMMAND_HOST:?}"
exec ssh -o BatchMode=yes "$host" screencapture "$@"
WRAP_SCREENCAPTURE

  chmod +x "$REMOTE_COMMAND_WRAPPER_DIR"/open "$REMOTE_COMMAND_WRAPPER_DIR"/pkill "$REMOTE_COMMAND_WRAPPER_DIR"/pbcopy "$REMOTE_COMMAND_WRAPPER_DIR"/pbpaste "$REMOTE_COMMAND_WRAPPER_DIR"/defaults "$REMOTE_COMMAND_WRAPPER_DIR"/sample "$REMOTE_COMMAND_WRAPPER_DIR"/screencapture
  echo "remote command wrappers: ready"
}

run_suite() {
  log "10. run the 30-case cross-machine suite"
  cd "$REPO/apps/desktop"
  local suite_status effective_status
  RAW_SUITE_OUTPUT="$(mktemp "$RAW_DIR/stdout.XXXXXX")"
  chmod 600 "$RAW_SUITE_OUTPUT"
  set +e
  env -u PETAL_REMOTE_CONTROL_DIRECT_SCROLL \
    -u PETAL_REMOTE_CONTROL_DIRECT_DRAG \
    -u PETAL_REMOTE_CONTROL_DIRECT_CLICK \
    PATH="$REMOTE_COMMAND_WRAPPER_DIR:$PATH" \
    PETAL_REMOTE_COMMAND_HOST="$REMOTE_HOST" \
    PETAL_AUTOTEST_SOCK="$LOCAL_FORWARD_SOCKET" \
    PETAL_REMOTE_OSASCRIPT_HOST="$REMOTE_HOST" \
    PETAL_REMOTE_CONTROL_TARGET_IDENTITY="$AUTOTEST_IDENTITY" \
    PETAL_WEB_HARNESS_URL_MATCH=localhost:5185 \
    node scripts/remote-control-local-loopback.mjs --live --json "$RESULTS_JSON" >"$RAW_SUITE_OUTPUT" 2>&1
  suite_status=$?
  set -e
  if ! reduce_suite_results "$RESULTS_JSON" "$SANITIZED_RESULTS_JSON" "$suite_status"; then
    write_classified_runner_failure "$SANITIZED_RESULTS_JSON" "malformed-results"
    effective_status=1
  else
    effective_status="$(node -e 'const r=require(process.argv[1]); process.stdout.write(String(r.suiteExit));' "$SANITIZED_RESULTS_JSON")"
  fi
  rm -f "$RAW_SUITE_OUTPUT" "$RESULTS_JSON"
  RAW_SUITE_OUTPUT=""
  if [[ "$effective_status" -ne 0 ]]; then
    echo "== cross-machine suite failed; privacy-safe evidence retained ==" >&2
    return "$effective_status"
  fi
  echo "== cross-machine suite passed; privacy-safe evidence retained =="
}

reduce_suite_results() {
  local raw_results="$1" summary_path="$2" suite_status="$3"
  [[ "$suite_status" =~ ^[0-9]+$ ]] || fail "Suite exit status was malformed."
  node - "$raw_results" "$summary_path" "$suite_status" <<'NODE'
const fs = require('node:fs');
const [rawPath, summaryPath, exitStatus] = process.argv.slice(2);
function fail() { process.exit(1); }
let report;
try { report = JSON.parse(fs.readFileSync(rawPath, 'utf8')); } catch { fail(); }
if (!report || typeof report !== 'object' || Array.isArray(report)) fail();
if (!report.summary || typeof report.summary !== 'object' || Array.isArray(report.summary)) fail();
// Pinned to SUITE_SUMMARY_KEYS in apps/desktop/scripts/remote-control-exit.mjs
// by scripts/test-cross-machine-rc-suite.sh. #580 added `tokenlessDrops` to the
// producer's SUMMARY and nowhere else, so every real run reduced to
// `malformed-results` while this file's hand-written fixtures stayed green.
const allowedSummary = new Set(['total', 'pass', 'fail', 'skip', 'recoveries', 'tokenlessDrops', 'mode', 'shareReadiness', 'targetObservationLatency']);
if (Object.keys(report.summary).some((key) => !allowedSummary.has(key))) fail();
const tokenlessDrops = report.summary.tokenlessDrops ?? 0;
if (!Number.isInteger(tokenlessDrops) || tokenlessDrops < 0) fail();
for (const key of ['total', 'pass', 'fail', 'skip']) {
  if (!Number.isInteger(report.summary[key]) || report.summary[key] < 0) fail();
}
if (report.summary.total !== report.summary.pass + report.summary.fail + report.summary.skip) fail();
if (!Array.isArray(report.results) || report.results.length !== report.summary.total || report.results.some((entry) => !entry || typeof entry !== 'object' || !['pass', 'fail', 'skip'].includes(entry.status))) fail();
const resultCounts = {
  pass: report.results.filter((entry) => entry.status === 'pass').length,
  fail: report.results.filter((entry) => entry.status === 'fail').length,
  skip: report.results.filter((entry) => entry.status === 'skip').length,
};
const resultCountsMatchSummary = resultCounts.pass === report.summary.pass
  && resultCounts.fail === report.summary.fail
  && resultCounts.skip === report.summary.skip;
if (!Array.isArray(report.terminalDeliveries)) fail();
const route = new Set(['admission', 'resolve', 'replay']);
const failureCode = new Set(['unauthorized', 'accessibilityDenied', 'grantExpired', 'targetOffScreen', 'targetUnavailable', 'resolveFailed', 'replayFailed', 'injectionTimeout', 'superseded', 'malformed', 'admissionOverloaded']);
const outcome = new Set(['applied', 'unauthorized', 'accessibilityDenied', 'grantExpired', 'targetOffScreen', 'targetUnavailable', 'resolveFailed', 'replayFailed', 'superseded', 'malformed', 'admissionOverloaded']);
function delivery(entry) {
  if (!entry || typeof entry !== 'object' || Array.isArray(entry)) fail();
  const allowed = new Set(['inputId', 'inputSeq', 'outcome', 'deliveryRoute', 'failureCode', 'windowId', 'receivedAt']);
  if (Object.keys(entry).some((key) => !allowed.has(key))) fail();
  if (typeof entry.inputId !== 'string' || !/^[A-Za-z0-9_-]{1,128}$/.test(entry.inputId)) fail();
  if (!Number.isInteger(entry.inputSeq) || entry.inputSeq < 0 || !outcome.has(entry.outcome)) fail();
  if (!Number.isInteger(entry.windowId) || entry.windowId < 0 || !Number.isInteger(entry.receivedAt) || entry.receivedAt < 0) fail();
  if (entry.deliveryRoute !== undefined && !route.has(entry.deliveryRoute)) fail();
  if (entry.failureCode !== undefined && !failureCode.has(entry.failureCode)) fail();
  if (entry.outcome === 'applied' && entry.failureCode !== undefined) fail();
  const safe = { inputId: entry.inputId, inputSeq: entry.inputSeq, outcome: entry.outcome, windowId: entry.windowId, receivedAt: entry.receivedAt };
  if (entry.deliveryRoute !== undefined) safe.deliveryRoute = entry.deliveryRoute;
  if (entry.failureCode !== undefined) safe.failureCode = entry.failureCode;
  return safe;
}
const terminalDeliveries = report.terminalDeliveries.map(delivery);
if (!report.terminalRecovery || typeof report.terminalRecovery !== 'object' || Array.isArray(report.terminalRecovery)) fail();
const allowedRecovery = new Set(['duplicateReplayObserved', 'sideEffectCount', 'terminalDeliveries']);
if (
  Object.keys(report.terminalRecovery).length !== allowedRecovery.size
  || Object.keys(report.terminalRecovery).some((key) => !allowedRecovery.has(key))
) fail();
if (typeof report.terminalRecovery.duplicateReplayObserved !== 'boolean') fail();
if (
  !Number.isInteger(report.terminalRecovery.sideEffectCount)
  || report.terminalRecovery.sideEffectCount < 0
  || report.terminalRecovery.sideEffectCount > 4
) fail();
if (!Array.isArray(report.terminalRecovery.terminalDeliveries) || report.terminalRecovery.terminalDeliveries.length > 3) fail();
const recoveryDeliveries = report.terminalRecovery.terminalDeliveries.map(delivery);
function sameOptional(left, right, key) {
  return Object.hasOwn(left, key) === Object.hasOwn(right, key)
    && (!Object.hasOwn(left, key) || left[key] === right[key]);
}
const matchingRecovery = recoveryDeliveries.length === 2
  && recoveryDeliveries[0].inputId === recoveryDeliveries[1].inputId
  && recoveryDeliveries[0].inputSeq === recoveryDeliveries[1].inputSeq
  && recoveryDeliveries[0].outcome === recoveryDeliveries[1].outcome
  && recoveryDeliveries[0].windowId === recoveryDeliveries[1].windowId
  && sameOptional(recoveryDeliveries[0], recoveryDeliveries[1], 'deliveryRoute')
  && sameOptional(recoveryDeliveries[0], recoveryDeliveries[1], 'failureCode')
  && recoveryDeliveries[1].receivedAt >= recoveryDeliveries[0].receivedAt;
function sameDelivery(left, right) {
  return left.inputId === right.inputId
    && left.inputSeq === right.inputSeq
    && left.outcome === right.outcome
    && left.windowId === right.windowId
    && left.receivedAt === right.receivedAt
    && sameOptional(left, right, 'deliveryRoute')
    && sameOptional(left, right, 'failureCode');
}
const consumedTerminalIndexes = new Set();
const recoveryOccursInTerminalDeliveries = recoveryDeliveries.every((recovery) => {
  const index = terminalDeliveries.findIndex(
    (terminal, candidateIndex) =>
      !consumedTerminalIndexes.has(candidateIndex)
      && sameDelivery(terminal, recovery)
  );
  if (index < 0) return false;
  consumedTerminalIndexes.add(index);
  return true;
});
const recoverySucceeded = report.terminalRecovery.duplicateReplayObserved
  && report.terminalRecovery.sideEffectCount === 1
  && matchingRecovery
  && recoveryOccursInTerminalDeliveries;
const recovery = {
  duplicateReplayObserved: report.terminalRecovery.duplicateReplayObserved,
  sideEffectCount: report.terminalRecovery.sideEffectCount,
  terminalDeliveries: recoveryDeliveries,
};
// #580: a host-side tokenless drop means the packet never reached any
// injection route, so the run did not inject what it claims to have injected.
// This is NOT a new pass/fail criterion -- remote-control-scenario.mjs has
// failed the run on a nonzero count since #580 (`process.exitCode =
// summary.fail > 0 || tokenlessDrops > 0 ? 1 : 0`). Zero is the correct floor:
// the only plausible benign path, case 24's deliberate post-release input, does
// NOT produce the line, because once control is released the host has no
// session and returns before the token check. This path was blind to it only
// because the broken allowlist ate the key, so enforcing it here restores the
// established criterion rather than adding one -- and stops a contradictory
// child exit laundering a drop into a pass.
const effectiveExit = Number(exitStatus) !== 0
  || report.summary.fail > 0
  || tokenlessDrops > 0
  || !resultCountsMatchSummary
  || !recoverySucceeded
  ? 1
  : 0;
const summary = {
  format: 'petal-cross-machine-summary-v1',
  inputRoute: 'packaged-default',
  suiteExit: effectiveExit,
  terminal: {
    total: report.summary.total,
    pass: report.summary.pass,
    fail: report.summary.fail,
    skip: report.summary.skip,
    tokenlessDrops,
    mode: report.summary.mode ?? 'numbered',
    shareReadiness: report.summary.shareReadiness ?? 'live-tile',
  },
  terminalDeliveries,
  terminalRecovery: recovery,
};
const temporary = `${summaryPath}.tmp-${process.pid}`;
fs.writeFileSync(temporary, `${JSON.stringify(summary)}\n`, { mode: 0o600 });
fs.renameSync(temporary, summaryPath);
NODE
}

write_classified_runner_failure() {
  local summary_path="$1" classification="$2"
  node - "$summary_path" "$classification" <<'NODE'
const fs = require('node:fs');
const [summaryPath, classification] = process.argv.slice(2);
if (classification !== 'malformed-results') process.exit(1);
const temporary = `${summaryPath}.tmp-${process.pid}`;
const summary = { format: 'petal-cross-machine-summary-v1', inputRoute: 'packaged-default', suiteExit: 1, runnerFailure: classification, terminal: { total: 0, pass: 0, fail: 1, skip: 0, tokenlessDrops: 0 }, terminalDeliveries: [], terminalRecovery: { duplicateReplayObserved: false, sideEffectCount: 0, terminalDeliveries: [] } };
fs.writeFileSync(temporary, `${JSON.stringify(summary)}\n`, { mode: 0o600 });
fs.renameSync(temporary, summaryPath);
NODE
}

main() {
  mkdir -p "$EVIDENCE_ROOT"
  chmod 700 "$EVIDENCE_ROOT"
  EVIDENCE_DIR="$(mktemp -d "$EVIDENCE_ROOT/run.XXXXXX")"
  chmod 700 "$EVIDENCE_DIR"
  RAW_DIR="$(mktemp -d "$EVIDENCE_DIR/raw.XXXXXX")"
  chmod 700 "$RAW_DIR"
  RESULTS_JSON="$(mktemp "$RAW_DIR/results.XXXXXX")"
  chmod 600 "$RESULTS_JSON"
  SANITIZED_RESULTS_JSON="$EVIDENCE_DIR/cross-machine-summary.json"
  LOCAL_MANIFEST="$EVIDENCE_DIR/local-manifest.json"
  REMOTE_MANIFEST_LOCAL="$EVIDENCE_DIR/remote-manifest.json"
  SOURCE_COMMIT="$(git rev-parse HEAD)"
  require_remote_host
  preflight_ssh
  preflight_architecture
  preflight_macos_versions
  preflight_gui_session
  preflight_remote_osascript
  print_human_checklist

  if [[ "${1:-}" == "--preflight-only" ]]; then
    return 0
  fi

  LAN_IP="$(detect_lan_ip)"
  [[ -n "$LAN_IP" ]] || fail "Could not detect a controller network address reachable by the remote peer."

  start_controller_services
  if [[ -n "$QA_PREBUILT_BUNDLE" || -n "$QA_PREBUILT_ARCHIVE" ]]; then
    prepare_prebuilt_qa_bundle
  else
    build_signed_app
  fi
  deploy_app
  launch_remote_app "$LAN_IP"
  wait_for_remote_socket
  start_socket_forward
  create_remote_command_wrappers
  run_suite
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi

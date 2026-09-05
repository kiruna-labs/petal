#!/usr/bin/env bash
#
# Signed-release clean-TCC smoke scaffold.
#
# This script intentionally does not click macOS TCC dialogs or drive a real
# meeting. It gives the release operator one repeatable command for static
# signed-artifact assertions, the human-only clean-TCC checklist, and post-run
# petal.log marker assertions.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

BUNDLE_ID="${PETAL_BUNDLE_ID:-com.petal.app}"
TEAM_ID="${PETAL_RELEASE_TEAM_ID:-X83RP84J8Z}"
BACKEND_URL="${PETAL_RELEASE_BACKEND_URL:-https://app.petal.live}"
APP_PATH="${PETAL_RELEASE_APP:-}"
DMG_PATH="${PETAL_RELEASE_DMG:-}"
# #905: logs are now per-day (`petal.log.<YYYY-MM-DD>`, rolled mid-session
# at the UTC date boundary); resolve the most-recently-written daily file,
# falling back to the pre-#905 bare `petal.log` on an install that hasn't
# rolled once yet. `PETAL_RELEASE_LOG` still overrides unconditionally.
_petal_default_log_dir="$HOME/Library/Logs/Petal"
# `|| true`: under `set -eo pipefail` a missing daily log made this pipeline
# fail and the assignment aborted the whole script before it printed a
# single line -- the release dry run's smoke gate died silently (#916).
_petal_default_log="$(ls -t "$_petal_default_log_dir"/petal.log.[0-9]*[0-9] 2>/dev/null | head -1 || true)"
if [ -z "$_petal_default_log" ]; then
  _petal_default_log="$_petal_default_log_dir/petal.log"
fi
LOG_PATH="${PETAL_RELEASE_LOG:-$_petal_default_log}"
ASSERT_LOG=0
GUIDE_ONLY=0
STATIC_ONLY=0
MARKERS=(
  "permissions: request_screen_recording()"
  "permissions: request_microphone()"
  "permissions: request_camera()"
  "permissions: request_accessibility()"
  "session: start_share(window "
  "frame pump heartbeat -- pushed"
  # #622: "moving frames" liveness. The heartbeat above fires on ANY push,
  # including idle_static_refresh re-pushes of one parked static frame; this
  # marker is only emitted by share.rs after MOVING_FRAME_LIVENESS_THRESHOLD
  # pushes that carried affirmative changed-content evidence (dirty rects or
  # a changed snapshot hash). A frozen share cannot satisfy it.
  "share liveness confirmed -- "
  "publish succeeded"
  "started control of local shared window"
  "remote-control: published host status"
  "remote-control-latency: host replay complete"
)
FORBIDDEN_MARKERS=(
  "missing environment variable PETAL_BACKEND_URL"
)

usage() {
  cat <<EOF
Usage:
  scripts/release-smoke.sh --app /Applications/Petal.app [--dmg path]
      verifies artifacts, prints the manual checklist, records the run boundary
  scripts/release-smoke.sh --app /Applications/Petal.app --assert-log
      after the manual pass: asserts markers logged since that boundary

Options:
  --app PATH          Signed Petal.app to verify. Env: PETAL_RELEASE_APP
  --dmg PATH          Optional stapled DMG to verify. Env: PETAL_RELEASE_DMG
  --log PATH          petal.log path. Env: PETAL_RELEASE_LOG
  --bundle-id ID      Bundle id for TCC reset guidance. Default: $BUNDLE_ID
  --team-id ID        Expected Developer ID team. Default: $TEAM_ID
  --backend-url URL   Expected baked backend URL. Default: $BACKEND_URL
  --assert-log        Second invocation only: assert all markers against log
                      output appended AFTER the run boundary recorded by the
                      first (checklist) invocation. Fails if no boundary was
                      recorded or nothing was logged since (#622)
  --marker TEXT       Add a required log marker. May be repeated
  --markers-file FILE Add required markers, one non-empty non-comment line each
  --guide-only        Print the clean-TCC checklist without artifact assertions
  --static-only       Verify signed artifacts only; skip the human TCC checklist
  -h, --help          Show this help

The default log markers are the current release-24 gate surface. As #45/#46/#48
add richer liveness markers, pass a markers file to assert them without changing
this scaffold.
EOF
}

die() {
  printf 'release-smoke: %s\n' "$*" >&2
  exit 1
}

step() {
  printf '\n\033[1;36m==> %s\033[0m\n' "$1"
}

add_markers_file() {
  local file="$1"
  [ -f "$file" ] || die "markers file not found: $file"
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      ''|\#*) continue ;;
      *) MARKERS+=("$line") ;;
    esac
  done < "$file"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --app)
      [ "$#" -ge 2 ] || die "--app needs a path"
      APP_PATH="$2"
      shift 2
      ;;
    --dmg)
      [ "$#" -ge 2 ] || die "--dmg needs a path"
      DMG_PATH="$2"
      shift 2
      ;;
    --log)
      [ "$#" -ge 2 ] || die "--log needs a path"
      LOG_PATH="$2"
      shift 2
      ;;
    --bundle-id)
      [ "$#" -ge 2 ] || die "--bundle-id needs a value"
      BUNDLE_ID="$2"
      shift 2
      ;;
    --team-id)
      [ "$#" -ge 2 ] || die "--team-id needs a value"
      TEAM_ID="$2"
      shift 2
      ;;
    --backend-url)
      [ "$#" -ge 2 ] || die "--backend-url needs a URL"
      BACKEND_URL="$2"
      shift 2
      ;;
    --assert-log)
      ASSERT_LOG=1
      shift
      ;;
    --marker)
      [ "$#" -ge 2 ] || die "--marker needs text"
      MARKERS+=("$2")
      shift 2
      ;;
    --markers-file)
      [ "$#" -ge 2 ] || die "--markers-file needs a path"
      add_markers_file "$2"
      shift 2
      ;;
    --guide-only)
      GUIDE_ONLY=1
      shift
      ;;
    --static-only)
      STATIC_ONLY=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

print_manual_gate() {
  step "Manual clean-TCC smoke gate"
  cat <<EOF
Run this on a clean or intentionally reset test Mac against the signed release
artifact, not the dev binary.

Manual setup:
  1. Quit Petal completely.
  2. Install the signed release app, typically in /Applications.
  3. Reset or revoke only the test Mac's grants:
       tccutil reset ScreenCapture $BUNDLE_ID
       tccutil reset Accessibility $BUNDLE_ID
  4. Launch the signed app normally from Finder or open.
  5. Complete onboarding and grant Screen Recording, Microphone, Camera, and
     Accessibility from their onboarding rows when prompted.
  6. Relaunch Petal after any onboarding grant that requests a relaunch.
  7. Join a real room with a second peer, share a real app window, and verify the
     remote peer receives moving frames.
  8. From the remote peer, request control; Accessibility should already be
     granted from onboarding. Verify first click/text lands in the shared app.
  9. Confirm there are no surprise Camera/Microphone/Screen Recording prompts
     outside the expected onboarding/share/control moments.
 10. Re-run this script with --assert-log to check petal.log markers.

Required operator notes for the release record:
  - macOS version and architecture of the test Mac
  - artifact path, version, and signing TeamIdentifier
  - whether Screen Recording, Microphone, Camera, and Accessibility prompts
    appeared at the expected onboarding steps
  - share liveness result and remote-control first-input result
  - this script's final assertion output
EOF
}

verify_dmg() {
  local dmg="$1"
  [ -f "$dmg" ] || die "DMG not found: $dmg"

  step "DMG notarization/staple checks"
  spctl -a -vvv -t open --context context:primary-signature "$dmg"
  xcrun stapler validate "$dmg"
}

verify_app() {
  local app="$1"
  [ -d "$app" ] || die "app bundle not found: $app"

  local bin="$app/Contents/MacOS/desktop"
  [ -x "$bin" ] || die "app binary not executable: $bin"

  step "Signed app checks"
  codesign --verify --deep --strict --verbose=2 "$app"

  local codesign_info
  codesign_info="$(codesign -dvv "$app" 2>&1 || true)"
  printf '%s\n' "$codesign_info" | grep -q "TeamIdentifier=$TEAM_ID" \
    || die "expected TeamIdentifier=$TEAM_ID in codesign output"
  printf '%s\n' "$codesign_info" | grep -q "runtime" \
    || die "expected hardened runtime flag in codesign output"

  # No `-q`/early-exit on these: grep -q closes its stdin as soon as it finds
  # a match, and if that happens before the upstream otool/strings process has
  # finished writing (routine on a large universal binary), the upstream gets
  # SIGPIPE (exit 141) -- which `pipefail` then misreports as THIS check
  # failing, even though grep genuinely found the match. Redirect to /dev/null
  # instead and let grep drain the whole stream before checking its own exit
  # status alone.
  # #622: fail CLOSED. The old form (`if otool ... | grep ...`) passed when
  # otool itself errored (empty pipe -> grep finds nothing -> "no CLT rpath"),
  # unlike the strings check below which fails closed. Capture otool's output
  # and exit status first, and require it to actually contain load commands
  # before treating "no CommandLineTools match" as evidence.
  local load_cmds
  if ! load_cmds="$(otool -l "$bin" 2>&1)"; then
    printf '%s\n' "$load_cmds" >&2
    die "otool -l failed on app binary; cannot verify absence of CLT rpath"
  fi
  # No pipe here: under `set -o pipefail`, `printf | grep -q` races -- grep
  # exits on the first match, printf takes EPIPE ("write error: Broken pipe")
  # and the pipeline fails even though the text was present. That false
  # INSUFFICIENT DATA killed the 0.9.6 release run (#916).
  [[ "$load_cmds" == *"Load command"* ]] \
    || die "otool -l produced no load commands; cannot verify absence of CLT rpath (INSUFFICIENT DATA)"
  if printf '%s\n' "$load_cmds" | grep "CommandLineTools" > /dev/null; then
    printf '%s\n' "$load_cmds" | grep -A2 LC_RPATH || true
    die "app binary carries a CommandLineTools rpath"
  fi
  if ! strings "$bin" | grep -F "$BACKEND_URL" > /dev/null; then
    die "app binary does not contain expected baked backend URL: $BACKEND_URL"
  fi
  bash "$ROOT/scripts/verify-universal-app.sh" --updater-config-only
  printf 'OK: signed app has TeamIdentifier=%s, hardened runtime, and no CLT rpath\n' "$TEAM_ID"
  printf 'OK: signed app contains expected backend URL\n'
}

# #622: run boundary for the log grep. The old assert grepped the CUMULATIVE
# petal.log, so markers from any previous session satisfied a new release and
# stale forbidden markers failed a good one. The checklist run records the
# current end-of-log byte offset; --assert-log only ever greps bytes appended
# after that baseline.
baseline_path_for_log() {
  printf '%s.release-smoke-baseline' "$1"
}

record_log_baseline() {
  local log="$1"
  local baseline
  baseline="$(baseline_path_for_log "$log")"
  local offset=0
  if [ -f "$log" ]; then
    offset="$(wc -c < "$log" | tr -d '[:space:]')"
  fi
  printf 'offset=%s\nrecorded_at=%s\n' "$offset" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$baseline"
  step "Run boundary recorded"
  printf 'Baseline: %s (log offset %s bytes). --assert-log will only accept\nmarkers logged AFTER this point.\n' "$baseline" "$offset"
}

assert_log_markers() {
  local log="$1"
  [ -f "$log" ] || die "log file not found: $log"

  local baseline offset
  baseline="$(baseline_path_for_log "$log")"
  [ -f "$baseline" ] || die "no run boundary recorded for $log -- run the checklist step first (this script without --assert-log records the baseline). Grepping the cumulative log is not evidence (#622)"
  offset="$(sed -n 's/^offset=//p' "$baseline" | head -n1)"
  case "$offset" in
    ''|*[!0-9]*) die "corrupt baseline file: $baseline" ;;
  esac

  local size
  size="$(wc -c < "$log" | tr -d '[:space:]')"
  if [ "$size" -lt "$offset" ]; then
    # Log rotated/truncated since the baseline: the whole file is new output.
    printf 'note: %s shrank below the recorded baseline (rotation); treating the whole file as this run\n' "$log"
    offset=0
  fi
  [ "$size" -gt "$offset" ] || die "INSUFFICIENT DATA: no log output appended to $log since the recorded run boundary; the manual gate has not produced evidence"

  step "petal.log marker assertions (bytes $offset..$size of this run only)"
  local slice
  slice="$(mktemp -t release-smoke-log-slice)"
  tail -c "+$((offset + 1))" "$log" > "$slice"
  local missing=0
  for marker in "${MARKERS[@]}"; do
    if grep -Fq "$marker" "$slice"; then
      printf 'ok marker: %s\n' "$marker"
    else
      printf 'missing marker: %s\n' "$marker" >&2
      missing=1
    fi
  done
  for marker in "${FORBIDDEN_MARKERS[@]}"; do
    if grep -Fq "$marker" "$slice"; then
      printf 'forbidden marker present: %s\n' "$marker" >&2
      missing=1
    else
      printf 'ok forbidden marker absent: %s\n' "$marker"
    fi
  done
  rm -f "$slice"

  [ "$missing" -eq 0 ] || die "one or more log marker assertions failed"
}

if [ "$GUIDE_ONLY" -eq 1 ]; then
  print_manual_gate
  exit 0
fi

[ -n "$APP_PATH" ] || die "provide --app /path/to/Petal.app or set PETAL_RELEASE_APP; use --guide-only for checklist-only mode"

if [ -n "$DMG_PATH" ]; then
  verify_dmg "$DMG_PATH"
fi
verify_app "$APP_PATH"

if [ "$STATIC_ONLY" -eq 1 ]; then
  if [ "$ASSERT_LOG" -eq 1 ]; then
    assert_log_markers "$LOG_PATH"
  fi
  printf '\nrelease-smoke static checks completed\n'
  exit 0
fi

# #622: --assert-log used to run IMMEDIATELY after printing the checklist,
# i.e. before the human had done any of the steps it validates. The assert is
# now only ever the second invocation: the first (no --assert-log) prints the
# checklist and records the run boundary; the operator completes the manual
# gate; the re-run with --assert-log validates only what was logged after
# that boundary.
if [ "$ASSERT_LOG" -eq 1 ]; then
  assert_log_markers "$LOG_PATH"
else
  print_manual_gate
  record_log_baseline "$LOG_PATH"
  step "petal.log marker assertions"
  cat <<EOF
Deferred until the manual gate above is complete. Then run:
  $ROOT/scripts/release-smoke.sh --app "$APP_PATH" --log "$LOG_PATH" --assert-log
Only log output produced after the baseline just recorded will count.
EOF
fi

printf '\nrelease-smoke scaffold completed\n'

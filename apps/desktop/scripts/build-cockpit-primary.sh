#!/usr/bin/env bash
# Build and inspect the direct QA primary used by the native-to-native cockpit.
# Full Xcode is a link-time input only; the executable resolves Swift from the
# OS at launch. This script never accepts a DYLD wrapper (#315).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DESKTOP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
CRATE_DIR="$DESKTOP_DIR/src-tauri"
MANIFEST="$CRATE_DIR/Cargo.toml"
BIN="$CRATE_DIR/target/debug/desktop"
VERIFY_DIRECT_LAUNCH=0
ASSERT_ARTIFACT=""
ASSERT_QA_ARTIFACT=""

# shellcheck source=cockpit-runtime-policy.sh
source "$SCRIPT_DIR/cockpit-runtime-policy.sh"

cockpit_frontend_provenance() {
  local source="$DESKTOP_DIR/build/dev/test-pattern.html"
  local status="$DESKTOP_DIR/build/dev/test-pattern-status.html"
  [[ -f "$source" && -f "$status" ]] || {
    echo "build-cockpit-primary: missing generated cockpit assets; run npm run build in $DESKTOP_DIR" >&2
    return 1
  }
  local commit source_sum status_sum
  commit="$(git -C "$DESKTOP_DIR/../.." rev-parse HEAD 2>/dev/null || printf unknown)"
  source_sum="$(cksum "$source" | awk '{print $1 ":" $2}')"
  status_sum="$(cksum "$status" | awk '{print $1 ":" $2}')"
  printf 'git=%s;dev/test-pattern.html=%s;dev/test-pattern-status.html=%s' "$commit" "$source_sum" "$status_sum"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --verify-direct-launch) VERIFY_DIRECT_LAUNCH=1 ;;
    --assert-artifact) ASSERT_ARTIFACT="${2:?--assert-artifact requires a binary path}"; shift ;;
    --assert-qa-artifact) ASSERT_QA_ARTIFACT="${2:?--assert-qa-artifact requires a binary path}"; shift ;;
    *) echo "usage: $0 [--verify-direct-launch] [--assert-artifact <binary>] [--assert-qa-artifact <binary>]" >&2; exit 64 ;;
  esac
  shift
done

if [[ -n "$ASSERT_ARTIFACT" ]]; then
  cockpit_runtime_assert_non_qa_artifact "$ASSERT_ARTIFACT"
  echo "build-cockpit-primary: OK — non-QA artifact has no toolchain runtime path: $ASSERT_ARTIFACT"
  exit 0
fi
if [[ -n "$ASSERT_QA_ARTIFACT" ]]; then
  cockpit_runtime_assert_qa_artifact "$ASSERT_QA_ARTIFACT"
  echo "build-cockpit-primary: OK — QA artifact uses the system Swift runtime: $ASSERT_QA_ARTIFACT"
  exit 0
fi

cockpit_runtime_configure_build
# Release builds intentionally omit /dev routes. The QA cockpit is the one
# supported exception: build its own static frontend with the deterministic
# source and operator-status pages included, then fingerprint those exact
# assets below. This prevents a caller from accidentally proving a stale or
# release-only frontend.
(
  cd "$DESKTOP_DIR"
  PETAL_INCLUDE_DEV_ROUTES=1 npm run build
)
export PETAL_COCKPIT_FRONTEND_PROVENANCE="$(cockpit_frontend_provenance)"
[[ "$PETAL_COCKPIT_FRONTEND_PROVENANCE" == *'dev/test-pattern-status.html='* ]] || {
  echo "build-cockpit-primary: invalid cockpit frontend provenance" >&2
  exit 1
}
echo "build-cockpit-primary: full-Xcode link policy=$COCKPIT_XCODE_SWIFT_LINK_DIR; dynamic runtime=$COCKPIT_SYSTEM_SWIFT_RPATH"
echo "build-cockpit-primary: cockpit frontend provenance=$PETAL_COCKPIT_FRONTEND_PROVENANCE"
cargo build --manifest-path "$MANIFEST" --features cockpit-privileged
[[ -x "$BIN" ]] || { echo "build-cockpit-primary: expected binary missing: $BIN" >&2; exit 1; }
cockpit_runtime_assert_qa_artifact "$BIN"
echo "build-cockpit-primary: OK — direct QA binary=$BIN; do not add DYLD_* variables."

if [[ "$VERIFY_DIRECT_LAUNCH" == "1" ]]; then
  cockpit_runtime_verify_direct_launch "$BIN" "primary"
fi

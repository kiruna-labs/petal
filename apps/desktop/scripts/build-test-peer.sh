#!/usr/bin/env bash
#
# Build the Native Test Client (test-peer) binary for SHARE-01 / SHARE-N2N.
#
# Why this exists: SHARE-01 (the project history, feature A, P0) is the one
# journey that validates Petal's defining feature -- a shared window rendering
# on the receiver as a real, borderless, independently movable NATIVE window,
# not a web DOM tile. Proving that on a single Mac needs a SECOND native
# instance as the receiver (the sharer is the normal `target/debug/desktop`).
#
# `tauri-plugin-single-instance` locks on `/tmp/<identifier>_si.sock`, where
# `<identifier>` is compiled in from tauri.conf.json -- it is NOT
# LaunchServices/PID based, so launching `target/debug/desktop` twice does NOT
# dodge it. The fix (zero Rust source changes) is a wholly separate binary built
# with a DIFFERENT identifier + its own CARGO_TARGET_DIR:
#
#   CARGO_TARGET_DIR=<crate>/target-peer \
#     TAURI_CONFIG='{"identifier":"com.petal.app.testpeer"}' \
#     cargo build --manifest-path <crate>/Cargo.toml
#
# -> `target-peer/debug/desktop`: its own single-instance socket, its own
# app_data_dir(), and its own TCC identity. Screen Recording (+ Accessibility if
# it drives remote control) is granted per-binary-path+signature, ONE TIME, via
# scripts/cockpit-setup.sh -- and stable across `cargo build` rebuilds (only a
# full `tauri build` re-sign churns it; see CLAUDE.md).
#
# This script ONLY builds the binary. `scripts/cockpit-setup.sh` gives the
# operator the one-time manual TCC instructions; the live cockpit protocol
# verifies the distinct peer identity and authenticated socket at run time.
#
# Env vars:
#   PETAL_TEST_PEER_IDENTIFIER   -- override the peer bundle identifier
#                                   (default com.petal.app.testpeer).
#   PETAL_TEST_PEER_FEATURES     -- extra cargo --features (space/comma list).
#                                   `cockpit-privileged` is always included:
#                                   without it the receiver refuses to run the
#                                   authenticated cockpit protocol.
#   Extra args after `--` are forwarded verbatim to `cargo build`.
#
# NOTE: do NOT launch the peer via `npm run dev:clean` / scripts/dev.sh -- those
# pkill `target/debug/desktop` and would kill the OTHER instance. Launch the two
# binaries directly.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DESKTOP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"          # apps/desktop
CRATE_DIR="$DESKTOP_DIR/src-tauri"                   # apps/desktop/src-tauri
MANIFEST="$CRATE_DIR/Cargo.toml"
TARGET_PEER_DIR="$CRATE_DIR/target-peer"
PEER_BIN="$TARGET_PEER_DIR/debug/desktop"
VERIFY_DIRECT_LAUNCH=0

# shellcheck source=cockpit-runtime-policy.sh
source "$SCRIPT_DIR/cockpit-runtime-policy.sh"

IDENTIFIER="${PETAL_TEST_PEER_IDENTIFIER:-com.petal.app.testpeer}"

FEATURES="cockpit-privileged"
if [ -n "${PETAL_TEST_PEER_FEATURES:-}" ]; then
  FEATURES="$FEATURES ${PETAL_TEST_PEER_FEATURES//,/ }"
fi
if ! grep -Eq '^cockpit-privileged[[:space:]]*=' "$MANIFEST"; then
  echo "build-test-peer: manifest does not expose required cockpit-privileged feature" >&2
  exit 1
fi
FEATURE_ARGS=(--features "$FEATURES")

# Anything after `--` is forwarded to cargo build.
EXTRA_ARGS=()
seen_dashes=0
for arg in "$@"; do
  if [ "$arg" = "--verify-direct-launch" ] && [ "$seen_dashes" = "0" ]; then
    VERIFY_DIRECT_LAUNCH=1
  elif [ "$seen_dashes" = "1" ]; then
    EXTRA_ARGS+=("$arg")
  elif [ "$arg" = "--" ]; then
    seen_dashes=1
  fi
done

echo "build-test-peer: identifier=$IDENTIFIER"
echo "build-test-peer: CARGO_TARGET_DIR=$TARGET_PEER_DIR"
echo "build-test-peer: manifest=$MANIFEST"
echo "build-test-peer: required features=$FEATURES"
echo "build-test-peer: NOTE first (cold) build of a fresh target-peer/ tree is large (~9GB) and slow."

cockpit_runtime_configure_build
echo "build-test-peer: full-Xcode link policy=$COCKPIT_XCODE_SWIFT_LINK_DIR; dynamic runtime=$COCKPIT_SYSTEM_SWIFT_RPATH"

if [ "${#EXTRA_ARGS[@]}" -gt 0 ]; then
  CARGO_TARGET_DIR="$TARGET_PEER_DIR" \
    TAURI_CONFIG="{\"identifier\":\"$IDENTIFIER\"}" \
    cargo build --manifest-path "$MANIFEST" "${FEATURE_ARGS[@]}" "${EXTRA_ARGS[@]}"
else
  CARGO_TARGET_DIR="$TARGET_PEER_DIR" \
    TAURI_CONFIG="{\"identifier\":\"$IDENTIFIER\"}" \
    cargo build --manifest-path "$MANIFEST" "${FEATURE_ARGS[@]}"
fi

if [ "$?" -eq 0 ]; then
  :
else
  echo "build-test-peer: FAILED -- see cargo output above" >&2
  exit 1
fi

if [ -x "$PEER_BIN" ]; then
  cockpit_runtime_assert_qa_artifact "$PEER_BIN"
  echo "build-test-peer: OK -- test-peer binary at $PEER_BIN"
  if [ "$VERIFY_DIRECT_LAUNCH" = "1" ]; then
    cockpit_runtime_verify_direct_launch "$PEER_BIN" "peer"
  fi
  echo "build-test-peer: next, run scripts/cockpit-setup.sh for the required manual Screen Recording/Accessibility setup."
else
  echo "build-test-peer: build reported success but binary missing at $PEER_BIN" >&2
  exit 1
fi

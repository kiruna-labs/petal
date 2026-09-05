#!/usr/bin/env bash
#
# Clean dev launcher for Petal.
#
# Why this exists: `tauri dev` runs the binary directly at
# `src-tauri/target/debug/desktop`, but a previous `tauri build` leaves a real
# `Petal.app` bundle under `target/{debug,release}/bundle/macos/`. macOS
# LaunchServices keeps that bundle registered for `com.petal.app`, so ANY
# `open`/`open -b com.petal.app`/Dock/Spotlight launch (and some tooling) fires
# up the STALE bundle instead of -- or alongside -- the running dev build. You
# then end up with two "Petal" processes, and clicks/tests land on the wrong
# (usually stale, unfixed) one. The in-app `tauri-plugin-single-instance` guard
# is the primary defense; this script is the belt-and-suspenders that makes a
# dev session start from a clean slate.
#
# What it does, then execs `tauri dev`:
#   1. Kills any running Petal/desktop processes (dev binary AND bundle).
#   2. Renames any built `.app` bundle out of the way (Petal.app -> Petal.app.disabled)
#      and unregisters it from LaunchServices.
#   3. Detaches any mounted Petal*.dmg volume.
#
# Usage:  npm run dev        (from apps/desktop -- see package.json)
#     or:  bash scripts/dev.sh

# NOTE: intentionally NOT using `set -e`. Every cleanup step below is
# best-effort (killing a process that isn't running, disabling a bundle that
# doesn't exist, grepping for a Petal DMG that isn't mounted) and legitimately
# exits non-zero on a clean machine. With `set -e` + `pipefail`, the very first
# such "failure" (e.g. `grep` finding no mounted Petal volume) would abort the
# script BEFORE it reached `exec npm run tauri dev` — which is exactly the bug
# where `dev:clean` printed "cleaning up…" and then silently quit. `set -u`
# (catch typos in var names) is safe to keep.
set -uo pipefail

# Resolve apps/desktop dir (this script lives in apps/desktop/scripts/).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$APP_DIR" || { echo "petal dev: could not cd to $APP_DIR" >&2; exit 1; }

LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

echo "petal dev: cleaning up any stale Petal instances/bundles…"

# 1. Kill running instances (both the raw dev binary and any bundle).
#    `|| true` so a clean machine (nothing running) doesn't abort under `set -e`.
pkill -f "target/debug/desktop"            2>/dev/null || true
pkill -f "target/release/desktop"          2>/dev/null || true
pkill -f "Petal.app/Contents/MacOS/desktop" 2>/dev/null || true

# 1b. Kill any orphaned Vite dev server still holding port 1420. `tauri dev`
# launches it as a `beforeDevCommand` child (`npm run dev` -> `vite dev`), so
# killing only the top-level `tauri dev`/cargo process (e.g. a prior session
# ending abruptly, or a manual `pkill -f "tauri dev"`) leaves this orphaned
# and still bound to the port -- the next `dev:clean` launch then fails with
# "Port 1420 is already in use" even though no Petal/tauri process appears to
# be running. Confirmed live: this happened twice in one session (2026-07-07).
VITE_PID="$(lsof -tiTCP:1420 -sTCP:LISTEN 2>/dev/null || true)"
if [ -n "$VITE_PID" ]; then
  echo "petal dev: killing orphaned process on port 1420 (pid $VITE_PID)"
  kill $VITE_PID 2>/dev/null || true
fi

# 2. Disable + unregister any built bundles so LaunchServices can't launch them.
for variant in debug release; do
  BUNDLE="src-tauri/target/$variant/bundle/macos/Petal.app"
  if [ -d "$BUNDLE" ]; then
    echo "petal dev: disabling stale bundle $BUNDLE"
    [ -x "$LSREGISTER" ] && "$LSREGISTER" -u "$BUNDLE" 2>/dev/null || true
    rm -rf "${BUNDLE}.disabled" 2>/dev/null || true
    mv "$BUNDLE" "${BUNDLE}.disabled"
  fi
done

# Also neutralize a stale copy that may have been installed to /Applications.
if [ -d "/Applications/Petal.app" ]; then
  echo "petal dev: unregistering /Applications/Petal.app"
  [ -x "$LSREGISTER" ] && "$LSREGISTER" -u "/Applications/Petal.app" 2>/dev/null || true
fi

# 3. Detach any mounted Petal DMG volume.
hdiutil info 2>/dev/null | grep -i "petal" | grep "/Volumes" | awk '{print $1}' | while read -r dev; do
  echo "petal dev: detaching $dev"
  hdiutil detach "$dev" 2>/dev/null || true
done

echo "petal dev: launching tauri dev…"
# CLT-only toolchain (see root CLAUDE.md "CLT-only build gotcha").
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Library/Developer/CommandLineTools}"
exec npm run tauri dev

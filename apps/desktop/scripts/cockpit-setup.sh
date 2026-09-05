#!/usr/bin/env bash
#
# One-time, deliberately non-destructive setup for the internal Test Cockpit.
# macOS does not permit a script to grant Screen Recording or Accessibility;
# this helper opens the right pane, records that the operator completed setup,
# and leaves the Rust preflight to verify the grants before every real run.

set -euo pipefail

# These are deliberately fixed identities, not caller-provided paths. The
# native-to-native cockpit starts two independently identified executables, so
# each one needs its own preflight marker. Do not add a generic marker-path
# override: this helper records an operator acknowledgement, never grants TCC.
COCKPIT_IDENTIFIERS=(
  "com.petal.app"
  "com.petal.app.testpeer"
)
APP_SUPPORT_ROOT="${HOME:-/tmp}/Library/Application Support"

cat <<'EOF'
Petal Test Cockpit setup

Grant Screen Recording and Accessibility to BOTH development executables:
  1. apps/desktop/src-tauri/target/debug/desktop
  2. apps/desktop/src-tauri/target-peer/debug/desktop

Build the second executable first with:
  apps/desktop/scripts/build-test-peer.sh

This script cannot grant TCC permissions itself. The cockpit checks those
permissions again before every run, so this marker is never proof of access.
EOF

if [[ "${PETAL_COCKPIT_SETUP_CONFIRMED:-}" != "1" ]]; then
  if command -v open >/dev/null 2>&1; then
    open 'x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture' || true
  fi
  cat <<'EOF'

After granting both permissions, run this command again with:
  PETAL_COCKPIT_SETUP_CONFIRMED=1 apps/desktop/scripts/cockpit-setup.sh
EOF
  exit 2
fi

for identifier in "${COCKPIT_IDENTIFIERS[@]}"; do
  marker="$APP_SUPPORT_ROOT/$identifier/.cockpit-setup-complete"
  mkdir -p "${marker%/*}"
  touch "$marker"
  echo "cockpit-setup: marker written for $identifier: $marker"
done
echo "cockpit-setup: markers record confirmation only; Rust still refuses safely if either executable lacks TCC access."

#!/usr/bin/env bash
# Register THIS Mac as a self-hosted GitHub Actions runner for your fork or
# deployment, and install it as a LaunchAgent in the current GUI session.
#
# Set PETAL_RUNNER_REPO=<owner>/<repo> -- there is deliberately no default, so
# this can never register a machine against somebody else's repository.
#
# Usage:
#   PETAL_RUNNER_REPO=you/petal scripts/runner/register.sh              # mints the token via gh (needs repo admin)
#   PETAL_RUNNER_REPO=you/petal scripts/runner/register.sh <reg-token>  # token minted by an admin, valid 1 hour:
#       gh api -X POST repos/<owner>/<repo>/actions/runners/registration-token --jq .token
#
# Idempotent: re-running with an already-configured runner just (re)installs
# the LaunchAgent. Remove with: scripts/runner/register.sh --remove
set -euo pipefail

REPO="${PETAL_RUNNER_REPO:-}"
if [[ -z "$REPO" ]]; then
  echo "PETAL_RUNNER_REPO is required, e.g. PETAL_RUNNER_REPO=you/petal $0" >&2
  exit 2
fi
RUNNER_DIR="${PETAL_RUNNER_DIR:-$HOME/actions-runner}"
LABELS="${PETAL_RUNNER_LABELS:-self-hosted,macOS}"
NAME="${PETAL_RUNNER_NAME:-$(scutil --get ComputerName 2>/dev/null || hostname -s)}"
PLIST_SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/com.petal.actions-runner.plist"
PLIST_DST="$HOME/Library/LaunchAgents/com.petal.actions-runner.plist"
UID_NUM="$(id -u)"

if [ "${1:-}" = "--remove" ]; then
  launchctl bootout "gui/$UID_NUM/com.petal.actions-runner" 2>/dev/null || true
  rm -f "$PLIST_DST"
  if [ -f "$RUNNER_DIR/.runner" ]; then
    tok="$(gh api -X POST "repos/$REPO/actions/runners/remove-token" --jq .token)"
    (cd "$RUNNER_DIR" && ./config.sh remove --token "$tok")
  fi
  echo "runner removed"
  exit 0
fi

[ -x "$RUNNER_DIR/config.sh" ] || { echo "no runner at $RUNNER_DIR -- download it from https://github.com/actions/runner/releases (osx-arm64) first" >&2; exit 1; }

if [ ! -f "$RUNNER_DIR/.runner" ]; then
  TOKEN="${1:-}"
  if [ -z "$TOKEN" ]; then
    echo "==> minting registration token via gh (requires admin on $REPO)"
    TOKEN="$(gh api -X POST "repos/$REPO/actions/runners/registration-token" --jq .token)" || {
      echo "could not mint a registration token -- this account lacks admin on $REPO." >&2
      echo "Ask a repo admin to run:  gh api -X POST repos/$REPO/actions/runners/registration-token --jq .token" >&2
      echo "and re-run:  $0 <token>   within one hour." >&2
      exit 2
    }
  fi
  echo "==> configuring runner '$NAME' labels=$LABELS"
  (cd "$RUNNER_DIR" && ./config.sh --unattended --replace \
      --url "https://github.com/$REPO" --token "$TOKEN" \
      --name "$NAME" --labels "$LABELS" --work _work)
else
  echo "==> runner already configured ($(grep -o '"agentName": *"[^"]*"' "$RUNNER_DIR/.runner" || echo "$RUNNER_DIR/.runner")); skipping config.sh"
fi

echo "==> installing LaunchAgent $PLIST_DST"
mkdir -p "$HOME/Library/LaunchAgents"
sed -e "s#__RUNNER_DIR__#$RUNNER_DIR#g" -e "s#__HOME__#$HOME#g" "$PLIST_SRC" > "$PLIST_DST"
launchctl bootout "gui/$UID_NUM/com.petal.actions-runner" 2>/dev/null || true
launchctl bootstrap "gui/$UID_NUM" "$PLIST_DST"
launchctl kickstart -k "gui/$UID_NUM/com.petal.actions-runner"
sleep 2
launchctl print "gui/$UID_NUM/com.petal.actions-runner" | grep -E 'state|pid' | head -3

cat <<MSG

Runner installed. Remaining one-time manual steps on this Mac:
  1. System Settings > Privacy & Security > Screen & System Audio Recording:
     grant the runner's process (it appears after the first capture attempt,
     or add $RUNNER_DIR/bin/Runner.Listener by hand) AND
     apps/desktop/src-tauri/target/debug/desktop. Same for Accessibility.
  2. Verify:  $(dirname "$PLIST_SRC")/preflight.sh
  3. Dispatch .github/workflows/nightly-loopback.yml once
     (gh workflow run nightly-loopback.yml) and ci-selfhosted.yml
     (gh workflow run ci-selfhosted.yml); when both are green, restore
     nightly's schedule trigger and ci-selfhosted's push/pull_request
     triggers in one commit (see each file's TRIGGER NOTE).
Logs: ~/Library/Logs/petal-actions-runner.{log,err}
MSG

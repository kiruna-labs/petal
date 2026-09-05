#!/usr/bin/env bash
# Install (or remove) the HOST LaunchAgent that keeps ephemeral-runner.sh
# running for one repo. The agent runs as the current user (it only calls
# tart, gh and launchctl); the jobs themselves run inside the guest.
#
#   PETAL_RUNNER_REPO=owner/repo scripts/runner/tart/install-host-agent.sh [--softnet] [--always]
#   scripts/runner/tart/install-host-agent.sh --remove
set -euo pipefail
LABEL="com.petal.tart-ephemeral-runner"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UID_NUM="$(id -u)"
if [ "${1:-}" = "--remove" ]; then
  launchctl bootout "gui/$UID_NUM/$LABEL" 2>/dev/null || true
  rm -f "$PLIST"
  echo "removed $LABEL (any running guest is cleaned up by the loop's trap; check 'tart list')"
  exit 0
fi
REPO="${PETAL_RUNNER_REPO:?PETAL_RUNNER_REPO=owner/repo is required}"
# Pass-through flags for the loop: --softnet (LAN isolation), --always (keep an idle guest).
EXTRA=""; for a in "$@"; do case "$a" in --softnet|--always) EXTRA="$EXTRA<string>$a</string>" ;; esac; done
STATE="$HOME/Library/Application Support/petal-tart-runner"
mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs/petal-tart-runner" "$STATE"
# The agent runs a fresh COPY of ephemeral-runner.sh each (re)launch, so the
# checked-in file can be edited while a loop is running; the running copy
# notices the original's mtime change and exits for KeepAlive to relaunch.
cat > "$STATE/launch.sh" <<EOF
#!/bin/bash
cp "$HERE/ephemeral-runner.sh" "$STATE/ephemeral-runner.running.sh"
export PETAL_RUNNER_SELF_SOURCE="$HERE/ephemeral-runner.sh"
exec bash "$STATE/ephemeral-runner.running.sh" "\$@"
EOF
chmod +x "$STATE/launch.sh"
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array><string>$STATE/launch.sh</string>$EXTRA</array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    <key>PETAL_RUNNER_REPO</key><string>$REPO</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>StandardOutPath</key><string>$HOME/Library/Logs/petal-tart-runner/loop.log</string>
  <key>StandardErrorPath</key><string>$HOME/Library/Logs/petal-tart-runner/loop.log</string>
</dict>
</plist>
EOF
# bootout is asynchronous: bootstrapping the new plist while the old instance
# is still being torn down fails with "Bootstrap failed: 5: Input/output
# error". Wait for the label to disappear, then retry the bootstrap a few times.
launchctl bootout "gui/$UID_NUM/$LABEL" 2>/dev/null || true
for _ in $(seq 1 20); do launchctl print "gui/$UID_NUM/$LABEL" >/dev/null 2>&1 || break; sleep 0.5; done
ok=0
for _ in 1 2 3 4 5; do
  if launchctl bootstrap "gui/$UID_NUM" "$PLIST" 2>/tmp/petal-agent-bootstrap.err; then ok=1; break; fi
  sleep 2
done
[ "$ok" -eq 1 ] || { echo "bootstrap failed: $(cat /tmp/petal-agent-bootstrap.err)"; echo "run this from a Terminal window logged in at the console (not ssh)"; exit 1; }
launchctl kickstart -k "gui/$UID_NUM/$LABEL"
echo "installed $LABEL for $REPO ($EXTRA); log: ~/Library/Logs/petal-tart-runner/loop.log"
echo "first run builds the golden guest (~15 min: brew, Chrome, runner) before the first ephemeral runner registers"

#!/usr/bin/env bash
# Run a command as a one-shot LaunchAgent in the current user's GUI (Aqua)
# launchd domain and stream its log.
#
# Why: `tart run` fails with VZErrorDomain -9 "The virtual machine
# encountered a security error ... Failed to create new HostKey" when the
# calling process is not in the Aqua session (ssh, agent harnesses, plain
# background launchd contexts) -- Virtualization.framework keys the guest's
# host key to the logged-in session. Bootstrapping into gui/<uid> works from
# any of those contexts as the same user, without sudo.
#
#   scripts/runner/tart/run-in-gui-session.sh <label> <logfile> <command> [args...]
#
# Blocks until the agent exits; prints the log as it grows; exits with the
# command's status (read from the log's trailing EXIT= marker).
set -euo pipefail
LABEL="${1:?label}"; LOG="${2:?logfile}"; shift 2
[ $# -gt 0 ] || { echo "command required" >&2; exit 2; }
UID_NUM="$(id -u)"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
WRAP="$(mktemp -t "$LABEL").sh"
mkdir -p "$(dirname "$LOG")" "$HOME/Library/LaunchAgents"
: > "$LOG"
{
  echo '#!/bin/bash'
  printf 'export PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin\n'
  printf 'cd %q\n' "$PWD"
  printf '%q ' "$@"; printf '\nEXIT=$?\necho "EXIT=$EXIT"\nexit $EXIT\n'
} > "$WRAP"
chmod +x "$WRAP"
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key><array><string>$WRAP</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>StandardOutPath</key><string>$LOG</string>
  <key>StandardErrorPath</key><string>$LOG</string>
</dict></plist>
EOF
launchctl bootout "gui/$UID_NUM/$LABEL" 2>/dev/null || true
if ! launchctl bootstrap "gui/$UID_NUM" "$PLIST" 2>/dev/null; then
  # A Background session (ssh, agent harness) gets "Domain does not support
  # specified action" here. LaunchServices still delivers `open` into the
  # console user's GUI session, so a throwaway windowless app bundle does the
  # bootstrap from inside that session and exits.
  APP="$(mktemp -d -t gui-bootstrap)/PetalGuiBootstrap.app"
  mkdir -p "$APP/Contents/MacOS"
  cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>com.petal.gui-bootstrap</string>
  <key>CFBundleName</key><string>PetalGuiBootstrap</string>
  <key>CFBundleExecutable</key><string>bootstrap</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSUIElement</key><true/>
</dict></plist>
EOF
  printf '#!/bin/bash\nlaunchctl bootstrap "gui/%s" %q\n' "$UID_NUM" "$PLIST" > "$APP/Contents/MacOS/bootstrap"
  chmod +x "$APP/Contents/MacOS/bootstrap"
  open -W "$APP"
  launchctl print "gui/$UID_NUM/$LABEL" >/dev/null 2>&1 || who | grep -q console || { echo "no GUI session for $(id -un); log in at the console first" >&2; exit 1; }
  rm -rf "$(dirname "$APP")"
fi
tail -n +1 -f "$LOG" &
TAILPID=$!
while ! grep -q '^EXIT=' "$LOG"; do sleep 5; done
sleep 1; kill "$TAILPID" 2>/dev/null || true
launchctl bootout "gui/$UID_NUM/$LABEL" 2>/dev/null || true
rm -f "$PLIST" "$WRAP"
RC="$(grep '^EXIT=' "$LOG" | tail -1 | cut -d= -f2)"
exit "${RC:-1}"

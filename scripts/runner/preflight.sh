#!/usr/bin/env bash
# Self-hosted runner preflight for Petal's display-requiring CI tier.
#
# Runs at the top of every job on the [self-hosted, macOS] runner (and by hand
# via scripts/runner/register.sh). Exits non-zero when the machine cannot give
# a trustworthy result, so a job fails loudly instead of producing a green
# that means nothing (CLAUDE.md: "test a gate in BOTH directions").
#
# Checks, in order:
#   1. Tools the live harnesses need: livekit-server, Google Chrome, Xcode,
#      cargo, node.
#   2. The CLAUDE.md multi-agent gate: refuse to run if a Petal dev binary is
#      already running that this job did not start. scripts/dev.sh's cleanup
#      step would otherwise kill another agent's live measurement mid-run.
#   3. Screen Recording access for the runner's process tree, using the same
#      non-prompting probe the app uses (CGPreflightScreenCaptureAccess). A
#      denied capture returns black, which is indistinguishable from the
#      no-black-frame failure being measured.
#   4. A real WindowServer session (the runner must be a LaunchAgent in the
#      logged-in GUI session, not a LaunchDaemon).
set -uo pipefail

fail=0
say()  { printf '%s\n' "$*"; }
ok()   { say "ok    $*"; }
bad()  { say "FAIL  $*"; fail=1; }

say "== Petal self-hosted runner preflight =="

for tool in livekit-server cargo node timeout; do   # timeout: coreutils, the workflows wrap every long step in it
  if command -v "$tool" >/dev/null 2>&1; then ok "$tool: $(command -v "$tool")"; else bad "$tool not on PATH"; fi
done
[ -x "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" ] && ok "Google Chrome present" || bad "Google Chrome missing"
[ -d /Applications/Xcode.app ] && ok "Xcode present" || bad "/Applications/Xcode.app missing (full Xcode required, not CLT)"

# 2. Multi-agent gate. Exclude our own shell from the match (CLAUDE.md rule 5).
running="$(pgrep -f 'target/(debug|release)/desktop( |$)' | grep -vw "$$" || true)"
if [ -n "$running" ]; then
  bad "a Petal dev binary is already running (pids: $(echo "$running" | tr '\n' ' ')) -- not mine; refusing to start (scripts/dev.sh would kill it)"
else
  ok "no foreign Petal dev instance running"
fi

# 3. Screen Recording (non-prompting). Swift one-liner so we don't need a
#    compiled helper; CGPreflightScreenCaptureAccess never shows the TCC sheet.
if command -v swift >/dev/null 2>&1; then
  probe="$(mktemp -d)/sr.swift"
  cat > "$probe" <<'SWIFT'
import CoreGraphics
print(CGPreflightScreenCaptureAccess() ? "granted" : "denied")
SWIFT
  sr="$(DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer swift "$probe" 2>/dev/null | tail -1)"
  rm -rf "$(dirname "$probe")"
  case "$sr" in
    granted) ok "Screen Recording: GRANTED for this process tree" ;;
    denied)  bad "Screen Recording: DENIED for this process tree -- grant it to the runner's launching app (System Settings > Privacy & Security > Screen & System Audio Recording) and restart the LaunchAgent" ;;
    *)       bad "Screen Recording probe could not run (output: '$sr')" ;;
  esac
else
  bad "swift not available; cannot probe Screen Recording"
fi

# 4. GUI session.
if launchctl managername 2>/dev/null | grep -q Aqua; then
  ok "running inside an Aqua (GUI) launchd session"
else
  bad "not an Aqua session (managername=$(launchctl managername 2>/dev/null)) -- install the runner as a LaunchAgent, not a LaunchDaemon"
fi

if [ "$fail" -ne 0 ]; then
  say "== preflight FAILED =="
  exit 1
fi
say "== preflight passed =="

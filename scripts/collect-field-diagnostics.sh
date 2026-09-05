#!/bin/bash
# Petal field-diagnostics collector (#878).
#
# Collects the artifacts that decide what tore down a login session /
# window server during a Petal meeting: every recent crash/jetsam/spin/hang
# report, the session-teardown unified-log windows, WindowServer
# PID-continuity windows for the known incidents, GPU restart counter,
# reboot/boot history, RAM size, a top-memory snapshot, and the Petal logs.
#
# Read-only except for its own output folder. No sudo. Safe to re-run
# (each run writes a fresh timestamped folder).
#
# Usage (either):
#   bash -c "$(curl -fsSL <hosted-url>)"
#   bash scripts/collect-field-diagnostics.sh
#
# Override the output root for testing: PETAL_DIAG_OUT=/some/dir
#
# The entire script body lives inside main() and the LAST line calls it, so
# a truncated download executes nothing.

set -u

main() {
  local STAMP ROOT OUT ZIP
  STAMP="$(date +%Y%m%d-%H%M%S)"
  ROOT="${PETAL_DIAG_OUT:-$HOME/Desktop}"
  OUT="$ROOT/petal-diag-$STAMP"

  if ! mkdir -p "$OUT" 2>/dev/null || ! cd "$OUT"; then
    echo "ERROR: cannot create $OUT -- is the Desktop writable?" >&2
    return 1
  fi

  step() { printf '\n==> %s\n' "$*"; }
  note() { printf '    %s\n' "$*"; }

  echo "Petal diagnostics collector"
  echo "This takes roughly 3-8 minutes (system log queries are slow)."
  echo "Please leave the window open until you see DONE."
  echo "Output: $OUT"

  step "1/7 System basics"
  sw_vers > os-version.txt 2>&1
  sysctl hw.memsize hw.model kern.boottime > hardware.txt 2>&1
  last reboot 2>/dev/null | head -10 > reboots.txt
  # Top memory consumers right now (whether Petal is currently bloated).
  ps axo rss,pid,etime,comm -m 2>/dev/null | head -25 > top-memory-now.txt

  step "2/7 Crash-report inventories"
  ls -lt /Library/Logs/DiagnosticReports/ > system-reports-list.txt 2>&1
  ls -lt "$HOME/Library/Logs/DiagnosticReports/" > user-reports-list.txt 2>&1
  if ! ls /Library/Logs/DiagnosticReports/ >/dev/null 2>&1; then
    {
      echo "WARNING: /Library/Logs/DiagnosticReports is NOT readable."
      echo "That folder holds the jetsam reports -- the most important artifact."
      echo "Fix: System Settings -> Privacy & Security -> Full Disk Access ->"
      echo "enable Terminal, then run this script again."
    } | tee fda-warning.txt
  fi

  step "3/7 Copying ALL diagnostic reports since 2026-08-15 (both locations)"
  mkdir -p reports-system reports-user
  find /Library/Logs/DiagnosticReports -maxdepth 1 -type f -newermt "2026-08-15" \
    \( -name '*.ips' -o -name '*.panic' -o -name '*.diag' -o -name '*.spin' -o -name '*.hang' -o -name '*.shutdownStall' \) \
    -exec cp {} reports-system/ \; 2>/dev/null
  find "$HOME/Library/Logs/DiagnosticReports" -maxdepth 1 -type f -newermt "2026-08-15" \
    \( -name '*.ips' -o -name '*.panic' -o -name '*.diag' -o -name '*.spin' -o -name '*.hang' \) \
    -exec cp {} reports-user/ \; 2>/dev/null
  note "system reports copied: $(ls reports-system 2>/dev/null | wc -l | tr -d ' ')"
  note "user reports copied:   $(ls reports-user 2>/dev/null | wc -l | tr -d ' ')"

  step "4/7 Session-teardown log windows (the slow part -- please wait)"
  note "Aug 24 teardown causes (loginwindow / jetsam / memorystatus / runningboard)..."
  log show \
    --predicate 'process == "loginwindow" OR process == "runningboardd" OR eventMessage CONTAINS[c] "memorystatus" OR eventMessage CONTAINS[c] "jetsam"' \
    --start "2026-08-24 20:35:00" --end "2026-08-24 20:50:00" --style compact \
    > aug24-session-teardown.txt 2>&1
  note "Aug 24 WindowServer + GPU window..."
  log show \
    --predicate 'process == "WindowServer" OR senderImagePath CONTAINS "AGX" OR eventMessage CONTAINS[c] "gpu restart"' \
    --start "2026-08-24 20:35:00" --end "2026-08-24 20:50:00" --style compact \
    > aug24-windowserver.txt 2>&1

  step "5/7 WindowServer continuity for the earlier incidents (may be purged; empty files are EXPECTED then)"
  note "Aug 17 window..."
  log show --predicate 'process == "WindowServer" OR eventMessage CONTAINS[c] "jetsam" OR eventMessage CONTAINS[c] "memorystatus"' \
    --start "2026-08-17 15:10:00" --end "2026-08-17 15:28:00" --style compact \
    > aug17-windowserver.txt 2>&1
  note "Aug 18 window..."
  log show --predicate 'process == "WindowServer" OR eventMessage CONTAINS[c] "jetsam" OR eventMessage CONTAINS[c] "memorystatus"' \
    --start "2026-08-18 14:36:00" --end "2026-08-18 14:46:00" --style compact \
    > aug18-windowserver.txt 2>&1

  step "6/7 GPU restart counter + Petal logs"
  ioreg -r -c IOAccelerator -d 1 2>/dev/null | grep -o '"recoveryCount"=[0-9]*' > gpu-restarts.txt
  [ -s gpu-restarts.txt ] || echo "no IOAccelerator recoveryCount readable" > gpu-restarts.txt
  if [ -d "$HOME/Library/Logs/Petal" ]; then
    cp -R "$HOME/Library/Logs/Petal" petal-logs 2>/dev/null
    note "petal logs copied: $(ls petal-logs 2>/dev/null | wc -l | tr -d ' ') file(s)"
  else
    echo "no ~/Library/Logs/Petal directory found" > petal-logs-missing.txt
  fi

  step "7/7 Manifest + zip"
  {
    echo "collected $STAMP on $(sw_vers -productVersion 2>/dev/null) $(sysctl -n hw.model 2>/dev/null)"
    echo
    # du is space-in-filename safe (tab-separated size<TAB>path).
    du -ak . 2>/dev/null | sort -k2
  } > MANIFEST.txt
  echo "----- collected files -----"
  cat MANIFEST.txt
  echo "---------------------------"

  cd "$ROOT" || return 1
  ZIP="petal-diag-$STAMP.zip"
  if ! zip -qr "$ZIP" "petal-diag-$STAMP" 2>/dev/null; then
    # ditto ships on every macOS; zip should too, but leave no failure mode.
    ditto -c -k --sequesterRsrc "petal-diag-$STAMP" "$ZIP" || {
      echo "ERROR: could not create zip; please send the folder $OUT instead." >&2
      return 1
    }
  fi
  echo
  echo "DONE. Please send this file back: $ROOT/$ZIP"
  open -R "$ROOT/$ZIP" 2>/dev/null || true
}

main "$@"

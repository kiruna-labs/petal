#!/bin/bash
# Preflight for the notarization leg of a release.
#
# WHY THIS EXISTS: the login keychain auto-locks on an inactivity timer
# (default here: 7200s / 2h). A universal build plus notarization routinely
# straddles that window, so the keychain relocks MID-RELEASE. notarytool then
# fails with "No Keychain password item" -- which reads like broken or missing
# credentials and has repeatedly sent people to re-authenticate, recreate the
# notarization profile, or switch git to SSH. All three are wrong and none of
# them fix it. The credentials are fine; a timer expired.
#
# Run this BEFORE starting a release, and again immediately before the
# notarization step if the build took a while.
#
# Detection is verified in BOTH directions (per CLAUDE.md): against a throwaway
# keychain, `security show-keychain-info` exits 0 and prints the settings when
# unlocked, and exits non-zero with "User canceled the operation." when locked.

set -u

KEYCHAIN="${NOTARY_KEYCHAIN:-$HOME/Library/Keychains/login.keychain-db}"
PROFILE="${NOTARY_PROFILE:-trickle}"
# A release leg (build + zip + submit + staple) comfortably exceeds this.
MIN_SAFE_TIMEOUT=10800

fail=0

# If an App Store Connect API key is configured, notarization reads it from a
# file and the keychain lock state is irrelevant to it. Say so plainly rather
# than emitting a warning that no longer applies.
AUTH=$("$(dirname "$0")/notary-auth.sh" 2>/dev/null) || AUTH=""
case "$AUTH" in
  --key\ *) AUTH_MODE="apikey" ;;
  *)        AUTH_MODE="keychain" ;;
esac

echo "== notarization auth: $AUTH_MODE"
if [ "$AUTH_MODE" = "apikey" ]; then
  echo "   App Store Connect API key (file-based) -- immune to keychain relock."
  echo "   Note: codesign still needs the login keychain for the Developer ID"
  echo "   certificate, so the state below still matters for SIGNING."
  echo
fi

echo "== keychain: $KEYCHAIN"

if info=$(security show-keychain-info "$KEYCHAIN" 2>&1); then
  echo "   state: UNLOCKED"
  # "timeout=7200s" -> 7200 ; absent means no inactivity timeout at all.
  timeout=$(printf '%s' "$info" | sed -n 's/.*timeout=\([0-9]*\)s.*/\1/p')
  if [ -z "$timeout" ]; then
    echo "   auto-lock: no inactivity timeout -- will not relock mid-release"
  else
    echo "   auto-lock: ${timeout}s"
    if [ "$timeout" -lt "$MIN_SAFE_TIMEOUT" ] && [ "$AUTH_MODE" = "apikey" ]; then
      echo "   (Only affects codesign now -- notarization uses the API key.)"
    elif [ "$timeout" -lt "$MIN_SAFE_TIMEOUT" ]; then
      echo
      echo "   WARNING: ${timeout}s is shorter than a full release leg."
      echo "   The keychain can relock between the build finishing and"
      echo "   notarization submitting. If that happens you will see"
      echo "   \"No Keychain password item\" -- that is THIS timer, not a"
      echo "   credential problem."
      echo
      echo "   Durable options (both need you to run them -- they change a"
      echo "   security setting or add a credential, so tooling must not):"
      echo "     1) Stop the login keychain auto-locking on idle:"
      echo "          security set-keychain-settings \"$KEYCHAIN\""
      echo "        (re-arm later with: security set-keychain-settings -u -t 7200 \"$KEYCHAIN\")"
      echo "     2) Better: drop the keychain from this path entirely by using"
      echo "        an App Store Connect API key, which notarytool reads from a"
      echo "        FILE and never from the keychain:"
      echo "          xcrun notarytool submit <artifact> --wait \\"
      echo "            --key ~/.appstoreconnect/private_keys/AuthKey_<KEYID>.p8 \\"
      echo "            --key-id <KEYID> --issuer <ISSUER-UUID>"
      echo "        Generate the key once at appstoreconnect.apple.com"
      echo "        (Users and Access -> Integrations -> App Store Connect API)."
    fi
  fi
elif [ ! -f "$KEYCHAIN" ]; then
  # Distinct from LOCKED on purpose: a non-zero exit alone cannot tell these
  # apart, and reporting "locked" for a path typo would send you to unlock a
  # keychain that was never the problem.
  echo "   state: NOT FOUND (no such file)"
  echo "   $info"
  echo
  echo "   FIX: this is a path problem, not a lock. Check NOTARY_KEYCHAIN."
  echo "   Available keychains:"
  security list-keychains | sed 's/^/     /'
  fail=1
else
  echo "   state: LOCKED"
  echo "   $info"
  echo
  echo "   FIX: unlock it, then re-run. Do NOT recreate the notarization"
  echo "   profile, do NOT run 'gh auth login', do NOT switch git to SSH --"
  echo "   a locked keychain makes all three look broken and none of them are."
  echo "     security unlock-keychain \"$KEYCHAIN\""
  fail=1
fi

echo
if [ "$AUTH_MODE" = "apikey" ]; then
  echo "== notarization credentials: App Store Connect API key"
else
  echo "== notarization credentials: keychain profile '$PROFILE'"
fi
if [ "$fail" -eq 0 ] || [ "$AUTH_MODE" = "apikey" ]; then
  # Live call through the SAME wrapper a release uses, so this exercises the
  # real auth path rather than a parallel one that could diverge from it.
  # No pipe: a pipeline would report the last stage's exit code, not this one.
  if out=$(timeout 90 "$(dirname "$0")/notarize.sh" history 2>&1); then
    echo "   USABLE (notarytool history succeeded)"
  else
    echo "   NOT USABLE"
    printf '%s\n' "$out" | sed 's/^/   /'
    echo
    echo "   If this mentions a missing Keychain password item while the"
    echo "   keychain reported UNLOCKED above, THEN it is a genuine profile"
    echo "   problem and re-running notarytool store-credentials is correct."
    echo "   Otherwise it is the lock. Check the state line above first."
    fail=1
  fi
else
  echo "   SKIPPED -- keychain is locked; unlock first or this result is meaningless."
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "PREFLIGHT: PASS"
else
  echo "PREFLIGHT: FAIL"
fi
exit "$fail"

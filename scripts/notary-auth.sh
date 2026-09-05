#!/bin/bash
# Emits the notarytool authentication flags to use, on stdout.
#
# Prefers an App Store Connect API key (read from a FILE, so it is immune to
# the login keychain's idle auto-lock -- see docs/RELEASING.md) and falls back
# to the legacy keychain profile.
#
# Usage:
#   eval "xcrun notarytool submit \"$ART\" --wait $(scripts/notary-auth.sh)"
# or:
#   AUTH=$(scripts/notary-auth.sh) || exit 1
#   xcrun notarytool submit "$ART" --wait $AUTH
#
# Config (create this file; it is never committed):
#   ~/.claude/secrets/petal-notary-api.env
#     NOTARY_API_KEY_ID=ABCD123456
#     NOTARY_API_ISSUER=aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee
#     NOTARY_API_KEY_PATH=$HOME/.appstoreconnect/private_keys/AuthKey_ABCD123456.p8
#
# This script prints ONLY flag names and the key PATH -- never key contents.

set -u

CONF="${NOTARY_API_ENV:-$HOME/.claude/secrets/petal-notary-api.env}"
PROFILE="${NOTARY_PROFILE:-trickle}"

if [ -f "$CONF" ]; then
  # shellcheck disable=SC1090
  . "$CONF"
  missing=""
  [ -z "${NOTARY_API_KEY_ID:-}" ] && missing="$missing NOTARY_API_KEY_ID"
  [ -z "${NOTARY_API_ISSUER:-}" ] && missing="$missing NOTARY_API_ISSUER"
  [ -z "${NOTARY_API_KEY_PATH:-}" ] && missing="$missing NOTARY_API_KEY_PATH"
  if [ -n "$missing" ]; then
    echo "notary-auth: $CONF exists but is missing:$missing" >&2
    exit 1
  fi
  # Reject un-substituted placeholders. Without this the flags are emitted
  # verbatim and notarytool fails much later with an opaque auth error --
  # exactly the "misleading symptom, distant cause" failure this whole script
  # exists to prevent.
  for ph in KEYID ISSUER-UUID "<KEYID>" "<ISSUER-UUID>"; do
    for var in NOTARY_API_KEY_ID NOTARY_API_ISSUER NOTARY_API_KEY_PATH; do
      eval "val=\$$var"
      case "$val" in *"$ph"*)
        echo "notary-auth: $CONF still contains the placeholder '$ph' in $var." >&2
        echo "notary-auth: substitute the real value. Key ID is the filename suffix" >&2
        echo "notary-auth: of your .p8; Issuer ID is the UUID at the top of the" >&2
        echo "notary-auth: App Store Connect API page (shared across all keys)." >&2
        exit 1;;
      esac
    done
  done

  # Expand a leading ~ so the config can be written either way.
  case "$NOTARY_API_KEY_PATH" in "~/"*) NOTARY_API_KEY_PATH="$HOME/${NOTARY_API_KEY_PATH#~/}";; esac
  if [ ! -f "$NOTARY_API_KEY_PATH" ]; then
    echo "notary-auth: key file not found: $NOTARY_API_KEY_PATH" >&2
    echo "notary-auth: generate one at appstoreconnect.apple.com -> Users and Access" >&2
    echo "notary-auth: -> Integrations -> App Store Connect API, then save it there." >&2
    exit 1
  fi
  # Apple requires the key file be readable only by you.
  perms=$(stat -f "%OLp" "$NOTARY_API_KEY_PATH")
  if [ "$perms" != "600" ] && [ "$perms" != "400" ]; then
    echo "notary-auth: $NOTARY_API_KEY_PATH is mode $perms; tighten it:" >&2
    echo "  chmod 600 \"$NOTARY_API_KEY_PATH\"" >&2
    exit 1
  fi
  printf -- '--key %s --key-id %s --issuer %s\n' \
    "$NOTARY_API_KEY_PATH" "$NOTARY_API_KEY_ID" "$NOTARY_API_ISSUER"
  exit 0
fi

# Fallback: keychain profile. Works, but is subject to the idle relock that
# docs/RELEASING.md describes -- run scripts/preflight-notarization.sh first.
printf -- '--keychain-profile %s\n' "$PROFILE"

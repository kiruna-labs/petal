#!/bin/bash
# Wrapper around `xcrun notarytool` that supplies authentication.
#
# USE THIS instead of interpolating flags yourself:
#   scripts/notarize.sh submit "$DMG" --wait
#   scripts/notarize.sh history
#
# WHY A WRAPPER, not `notarytool $(scripts/notary-auth.sh)`:
# that idiom relies on the shell word-splitting an unquoted expansion. bash
# does; **zsh does not** -- it passes the whole flag string as ONE argument and
# notarytool rejects it with "Unknown option '--key ... --key-id ...'". This
# Mac's interactive shell is zsh, so the documented command would have failed
# at the next release. Passing argv through a wrapper removes the dependency on
# shell word-splitting entirely.
#
# Auth comes from scripts/notary-auth.sh: App Store Connect API key when
# configured (immune to the login keychain's idle relock -- see
# docs/RELEASING.md), keychain profile otherwise.

set -u

DIR="$(cd "$(dirname "$0")" && pwd)"

# Build the auth flags into an ARRAY so each is its own argv entry.
AUTH_LINE=$("$DIR/notary-auth.sh") || exit 1
# shellcheck disable=SC2206
AUTH_ARGS=($AUTH_LINE)

if [ "$#" -eq 0 ]; then
  echo "usage: $(basename "$0") <notarytool-subcommand> [args...]" >&2
  echo "e.g.   $(basename "$0") submit /path/to/Petal.dmg --wait" >&2
  exit 2
fi

exec xcrun notarytool "$@" "${AUTH_ARGS[@]}"

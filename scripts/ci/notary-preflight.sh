#!/usr/bin/env bash
# Pick and PROVE notarization credentials before anything expensive runs.
#
# Dry run 4 of #916 (run 33828956308) spent 25 minutes building and signing a
# universal bundle, then died at `notarytool submit` with an Apple 401
# ("Invalid credentials"). The credentials can only be checked by asking
# Apple, so ask Apple first: `notarytool history` is a cheap authenticated
# call that fails in seconds with the same 401 a submit would.
#
# Inputs (env, from GitHub secrets):
#   APPLE_API_ISSUER / APPLE_API_KEY / APPLE_API_KEY_P8_BASE64   preferred
#   APPLE_ID / APPLE_PASSWORD / APPLE_TEAM_ID                     fallback
# Outputs (appended to $GITHUB_OUTPUT when set, always echoed):
#   mode=apikey|appleid      key-path=<decoded .p8 path or empty>
# Exit 1 with a precise ::error:: if neither set authenticates.
#
# No `timeout` wrapper: coreutils is not on the hosted macOS runner (dry run 5
# failed on "timeout: command not found"); the job's timeout-minutes is the
# watchdog.
set -euo pipefail

out() { echo "$1"; [ -n "${GITHUB_OUTPUT:-}" ] && echo "$1" >> "$GITHUB_OUTPUT" || true; }
tmp="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
apikey_reason=""; appleid_reason=""

if [ -n "${APPLE_API_KEY_P8_BASE64:-}" ] && [ -n "${APPLE_API_KEY:-}" ] && [ -n "${APPLE_API_ISSUER:-}" ]; then
  key_path="$tmp/AuthKey_${APPLE_API_KEY}.p8"
  printf '%s' "$APPLE_API_KEY_P8_BASE64" | base64 --decode > "$key_path"
  chmod 600 "$key_path"
  if ! head -n1 "$key_path" | grep -q 'BEGIN PRIVATE KEY'; then
    apikey_reason="APPLE_API_KEY_P8_BASE64 did not decode to a PKCS#8 private key"
  elif xcrun notarytool history --key "$key_path" --key-id "$APPLE_API_KEY" --issuer "$APPLE_API_ISSUER" > "$tmp/notary-apikey.txt" 2>&1; then
    echo "notarization auth: apikey (verified with notarytool history)"
    out "mode=apikey"; out "key-path=$key_path"
    exit 0
  else
    apikey_reason="Apple rejected the App Store Connect key: $(tail -n1 "$tmp/notary-apikey.txt" | cut -c1-200)"
  fi
else
  apikey_reason="APPLE_API_ISSUER / APPLE_API_KEY / APPLE_API_KEY_P8_BASE64 not all set"
fi
echo "::warning::API-key notarization unavailable -- $apikey_reason"

if [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
  if xcrun notarytool history --apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID" > "$tmp/notary-appleid.txt" 2>&1; then
    echo "notarization auth: apple-id (verified with notarytool history)"
    out "mode=appleid"; out "key-path="
    exit 0
  else
    appleid_reason="Apple rejected the Apple ID credentials: $(tail -n1 "$tmp/notary-appleid.txt" | cut -c1-200)"
  fi
else
  appleid_reason="APPLE_ID / APPLE_PASSWORD / APPLE_TEAM_ID not all set"
fi

echo "::error::No working notarization credentials. apikey: $apikey_reason. apple-id: $appleid_reason. For the API key: it must be a TEAM key (App Store Connect > Users and Access > Integrations > App Store Connect API > Team Keys) with role Admin, App Manager or Developer; APPLE_API_KEY is the Key ID, APPLE_API_ISSUER the Issuer ID shown on that same page, APPLE_API_KEY_P8_BASE64 the base64 of that key's AuthKey_<KEYID>.p8."
exit 1

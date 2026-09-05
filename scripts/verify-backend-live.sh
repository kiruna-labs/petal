#!/usr/bin/env bash
#
# Fundamental gap this closes (2026-07-05): a route/page can be fully committed
# to `main` and still 404 in production because the Vercel backend project
# deploys SEPARATELY from git (no auto-deploy hook wired up here). This bit
# twice in one session: the invite-link route (`api/j.ts`) and copy edits to
# the join/download pages were correct in source but stale live. Unit tests
# against the handler functions (see backend/test/distribution.ts) catch a
# regression IN SOURCE; they cannot catch "I forgot to redeploy." This script
# is the one check that actually hits the LIVE production backend and would
# have caught both incidents automatically.
#
# Run this after every `vercel --prod` deploy of `backend/` (see
# docs/RELEASING.md's "Deploying the backend" section), or any time a route
# added to backend/api/ seems to 404 for no reason in the app.
set -uo pipefail

BASE_URL="${PETAL_BACKEND_URL:-https://app.petal.live}"
FAILURES=0

# Vercel deployment protection (SSO) covers every non-custom-domain URL, so a
# staged `--skip-domain` deployment 302s to a login page for plain curl. When
# VERCEL_AUTOMATION_BYPASS_SECRET is set (release.yml's deploy-web job), send
# the documented bypass header on every request. Bash 3.2 + `set -u` needs the
# `${arr[@]+"${arr[@]}"}` form to expand an EMPTY array without erroring.
CURL_EXTRA=()
if [ -n "${VERCEL_AUTOMATION_BYPASS_SECRET:-}" ]; then
  CURL_EXTRA=(-H "x-vercel-protection-bypass: $VERCEL_AUTOMATION_BYPASS_SECRET")
fi

check() {
  local desc="$1" url="$2" expect_status="$3" expect_pattern="${4:-}"
  local out status
  out="$(curl -s ${CURL_EXTRA[@]+"${CURL_EXTRA[@]}"} -w '\n%{http_code}' "$url" 2>/dev/null)"
  status="${out##*$'\n'}"
  local body="${out%$'\n'*}"
  if [ "$status" != "$expect_status" ]; then
    echo "FAIL: $desc -- GET $url expected $expect_status, got $status" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  if [ -n "$expect_pattern" ] && ! echo "$body" | grep -qE "$expect_pattern"; then
    echo "FAIL: $desc -- GET $url returned $status but body didn't match /$expect_pattern/" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  echo "ok   $desc"
}

echo "verify-backend-live: checking $BASE_URL"

# This project is a pure API host now -- the marketing page lives at
# petal.live (petal-website), and join links (/:code, /:label/:code) moved to
# meet.petal.live (see verify-web-harness-live.sh). "/" here just redirects.
root_headers="$(curl -s ${CURL_EXTRA[@]+"${CURL_EXTRA[@]}"} -D - -o /dev/null "$BASE_URL/" 2>/dev/null)"
root_status="$(echo "$root_headers" | head -1 | grep -oE '[0-9]{3}')"
if [ "$root_status" != "302" ] || ! echo "$root_headers" | grep -qi '^location:[[:space:]]*https://petal\.live/'; then
  echo "FAIL: root redirect -- GET $BASE_URL/ expected 302 to https://petal.live/, got status=$root_status" >&2
  FAILURES=$((FAILURES + 1))
else
  echo "ok   root redirects to the marketing site (petal.live)"
fi

check "updater manifest is reachable" \
  "$BASE_URL/api/updater" 200 \
  '"platforms"'

# The public room directory is gone (enumeration leak): GET must be 410, and
# the proof-of-possession replacement must return nothing to a caller holding
# no credentials.
check "public room directory is removed (GET /api/rooms is 410)" \
  "$BASE_URL/api/rooms" 410 \
  'rooms/status'
STATUS_BODY="$(curl -s ${CURL_EXTRA[@]+"${CURL_EXTRA[@]}"} -X POST -H 'content-type: application/json' -d '{"rooms":[]}' "$BASE_URL/api/rooms/status" 2>/dev/null || true)"
if [ "$(printf '%s' "$STATUS_BODY" | tr -d '[:space:]')" = '{"rooms":[]}' ]; then
  echo "ok   /api/rooms/status returns an empty list to a caller with no credentials"
else
  echo "FAIL: /api/rooms/status with no credentials returned: ${STATUS_BODY:-<empty>}" >&2
  FAILURES=$((FAILURES + 1))
fi

echo
if [ "$FAILURES" -gt 0 ]; then
  echo "verify-backend-live: $FAILURES check(s) FAILED -- the deployed backend is stale or broken" >&2
  exit 1
fi
echo "verify-backend-live: all checks passed against the LIVE deployment"

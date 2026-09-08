#!/usr/bin/env bash
#
# Can the release gate's WEB peers actually talk to the backend? (#42 follow-up)
#
# The gate loads the Test Cockpit's web peers from the STAGED web-harness
# deployment, and their first act is a cross-origin token mint against the
# backend baked into that build. `backend/lib/http.ts` answers a browser Origin
# it does not recognise with a bare 403 -- no `Access-Control-Allow-Origin` --
# so the peer's join fails, and a failed join has no room to report over: the
# native engine sees total silence and can only say INFRA-FAIL.
#
# That is exactly how v0.9.10 failed: six web-peer scenarios reported
# INFRA-FAIL, twelve minutes into the gate, with no evidence beyond an empty
# peer list. Nothing in the run named the cause. This check names it in
# `deploy-web`, before the gate is ever queued.
#
# It also catches the deploy ordering that makes the allowance real: the origin
# allowlist lives in backend code, and `backend/` deploys SEPARATELY from git,
# so a merged fix that was never `vercel --prod`-ed leaves the live backend
# refusing the staged origin exactly as before.
#
#   PETAL_WEB_HARNESS_URL   staged web-harness origin the peers load (required)
#   PETAL_WEB_PEER_BACKEND  backend they will call; defaults to the
#                           VITE_PETAL_BACKEND_URL baked into
#                           web-harness/vercel.json, which is what the staged
#                           build actually calls.

set -uo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"

ORIGIN="${PETAL_WEB_HARNESS_URL:-}"
if [ -z "$ORIGIN" ]; then
  echo "verify-web-peer-origin: PETAL_WEB_HARNESS_URL is required" >&2
  exit 2
fi
ORIGIN="${ORIGIN%/}"

BACKEND="${PETAL_WEB_PEER_BACKEND:-}"
if [ -z "$BACKEND" ]; then
  # Single source of truth for what the staged harness build calls.
  BACKEND="$(sed -n 's/.*VITE_PETAL_BACKEND_URL=\([^ "]*\).*/\1/p' \
    "$REPO_ROOT/web-harness/vercel.json" | head -n1)"
fi
if [ -z "$BACKEND" ]; then
  echo "verify-web-peer-origin: could not determine the web peers' backend URL" >&2
  exit 2
fi
BACKEND="${BACKEND%/}"

# A STAGED backend sits behind Vercel deployment protection, which 302s a plain
# curl to an SSO page and would read as "origin refused". curl -- unlike the
# browser this check stands in for -- can send the documented bypass header.
# Bash 3.2 + `set -u` needs the `${arr[@]+"${arr[@]}"}` form for an empty array.
CURL_EXTRA=()
if [ -n "${VERCEL_AUTOMATION_BYPASS_SECRET:-}" ]; then
  CURL_EXTRA=(-H "x-vercel-protection-bypass: $VERCEL_AUTOMATION_BYPASS_SECRET")
fi

echo "verify-web-peer-origin: preflight $BACKEND/api/token as Origin: $ORIGIN"

RESPONSE="$(curl -s -o /dev/null -D - -X OPTIONS "$BACKEND/api/token" \
  ${CURL_EXTRA[@]+"${CURL_EXTRA[@]}"} \
  -H "Origin: $ORIGIN" \
  -H "Access-Control-Request-Method: POST" \
  -H "Access-Control-Request-Headers: content-type" \
  -w '\nHTTP_STATUS:%{http_code}\n' 2>/dev/null)"

STATUS="$(printf '%s\n' "$RESPONSE" | sed -n 's/^HTTP_STATUS:\([0-9]*\)$/\1/p' | tail -n1)"
ALLOW="$(printf '%s\n' "$RESPONSE" \
  | tr -d '\r' \
  | sed -n 's/^[Aa]ccess-[Cc]ontrol-[Aa]llow-[Oo]rigin:[[:space:]]*//p' | tail -n1)"

if [ "$STATUS" != "204" ] || [ "$ALLOW" != "$ORIGIN" ]; then
  echo "::error::$BACKEND refuses the staged web-harness origin $ORIGIN (preflight HTTP ${STATUS:-<none>}, Access-Control-Allow-Origin '${ALLOW:-<none>}')."
  echo "::error::Every web peer in the e2e gate would fail its token mint and report INFRA-FAIL with no explanation."
  echo "::error::Fix: land the origin allowance in backend/lib/http.ts AND deploy it -- 'cd backend && vercel --prod'. Merging alone does not deploy the backend."
  exit 1
fi

echo "verify-web-peer-origin: OK -- preflight 204, Access-Control-Allow-Origin: $ALLOW"

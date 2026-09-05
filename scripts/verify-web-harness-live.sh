#!/usr/bin/env bash
#
# Fundamental gap this closes (2026-07-08, issue #198): the web-harness
# Vercel project deploys SEPARATELY from git, same as backend/ (no
# auto-deploy hook wired up here either -- see verify-backend-live.sh's
# header for the backend incident this mirrors). PR #225 landed a full
# native-parity rebuild of the remote-window header on `main`, but the LIVE
# site kept showing the old floating-card design for days because nothing
# ever ran `vercel --prod` for web-harness -- a user screenshot caught it,
# not CI. Unit tests against the source (web-harness/tests/) catch a
# regression IN SOURCE; they cannot catch "I forgot to redeploy." This
# script hits the LIVE production site and checks for specific markers from
# landed parity work, so a missed deploy fails loudly instead of silently.
#
# Run this after every `vercel --prod` deploy of `web-harness/` (mirrors
# docs/RELEASING.md's "Deploying the backend" section -- web-harness needs
# the same manual step).

set -uo pipefail

BASE_URL="${PETAL_WEB_HARNESS_URL:-https://meet.petal.live}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
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

# Pre-publish mode (release.yml's deploy-web job): the staged web-harness is
# verified BEFORE latest.json for this release exists, so every check that
# compares against the published manifest (updater version, download redirect
# target, bundle version parity) is skipped; the structural checks still run.
# The same run re-executes this script WITHOUT the flag after publish.
PREPUBLISH="${PETAL_PREPUBLISH:-}"

check() {
  local desc="$1" url="$2" expect_status="$3" expect_pattern="${4:-}"
  local out status body
  out="$(curl -s ${CURL_EXTRA[@]+"${CURL_EXTRA[@]}"} -w '\n%{http_code}' "$url" 2>/dev/null)"
  status="${out##*$'\n'}"
  body="${out%$'\n'*}"
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

check_body_contains() {
  local desc="$1" url="$2" expect_pattern="$3"
  local body status
  body="$(curl -s ${CURL_EXTRA[@]+"${CURL_EXTRA[@]}"} -w '\n%{http_code}' "$url" 2>/dev/null)"
  status="${body##*$'\n'}"
  body="${body%$'\n'*}"
  if [ "$status" != "200" ]; then
    echo "FAIL: $desc -- GET $url expected 200, got $status" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  if ! echo "$body" | grep -qE "$expect_pattern"; then
    echo "FAIL: $desc -- GET $url returned 200 but body didn't match /$expect_pattern/" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  echo "ok   $desc"
}

echo "verify-web-harness-live: checking $BASE_URL"

ROOT_RESPONSE="$(curl -s ${CURL_EXTRA[@]+"${CURL_EXTRA[@]}"} -w '\n%{http_code}' "$BASE_URL/" 2>/dev/null)"
ROOT_STATUS="${ROOT_RESPONSE##*$'\n'}"
ROOT_HTML="${ROOT_RESPONSE%$'\n'*}"
if [ "$ROOT_STATUS" != "200" ]; then
  echo "FAIL: index page -- GET $BASE_URL/ expected 200, got $ROOT_STATUS" >&2
  FAILURES=$((FAILURES + 1))
else
  echo "ok   index page loads"
  if printf '%s' "$ROOT_HTML" | node -e '
    const fs = require("node:fs");
    const compact = fs.readFileSync(0, "utf8").replace(/\s+/g, " ");
    const footer = /<footer\b[^>]*\bid="build-version"[^>]*>(.*?)<\/footer>/is.exec(compact)?.[1] ?? "";
    const anchor = /<a\b[^>]*>Download Petal for macOS<\/a>/i.exec(footer)?.[0] ?? "";
    // `?platform=macos` was added with the Windows lane (1394ddf0); a fresh
    // deploy of main failed this check until the verifier caught up (#916).
    if (!footer || !anchor || !/\bhref="https:\/\/app\.petal\.live\/api\/download(\?platform=macos)?"/i.test(anchor)) process.exit(1);
  ' 2>/dev/null; then
    echo "ok   root footer exposes the exact desktop download link"
  else
    echo "FAIL: root footer did not contain the exact desktop download link" >&2
    FAILURES=$((FAILURES + 1))
  fi
fi

asset_path() {
  local extension="$1"
  printf '%s' "$ROOT_HTML" | node -e '
    const fs = require("node:fs");
    const extension = process.argv[1];
    const match = new RegExp(`(?:src|href)="(\\/?assets/meeting-[^"\\s]+\\.${extension})"`).exec(fs.readFileSync(0, "utf8"));
    if (match) process.stdout.write(match[1].replace(/^\/+/, ""));
  ' "$extension" 2>/dev/null || true
}

JS_PATH="$(asset_path js)"
CSS_PATH="$(asset_path css)"
if [ -z "$JS_PATH" ] || [ -z "$CSS_PATH" ]; then
  echo "FAIL: root page did not reference both the deployed SPA JS and CSS assets" >&2
  FAILURES=$((FAILURES + 1))
else
  echo "ok   root page references SPA assets: $JS_PATH and $CSS_PATH"
fi

UPDATER_URL="${PETAL_UPDATER_URL:-https://app.petal.live/api/updater}"
DOWNLOAD_URL="${PETAL_DOWNLOAD_URL:-https://app.petal.live/api/download}"
UPDATER_VERSION=""
EXPECTED_VERSION=""
if [ -n "$PREPUBLISH" ]; then
  echo "skip updater manifest / download redirect checks (PETAL_PREPUBLISH: manifest for this release is not published yet)"
else
UPDATER_RESPONSE="$(curl -s ${CURL_EXTRA[@]+"${CURL_EXTRA[@]}"} -w '\n%{http_code}' "$UPDATER_URL" 2>/dev/null)"
UPDATER_STATUS="${UPDATER_RESPONSE##*$'\n'}"
UPDATER_BODY="${UPDATER_RESPONSE%$'\n'*}"
if [ "$UPDATER_STATUS" != "200" ]; then
  echo "FAIL: updater manifest -- GET $UPDATER_URL expected 200, got $UPDATER_STATUS" >&2
  FAILURES=$((FAILURES + 1))
  UPDATER_VERSION=""
else
  UPDATER_VERSION="$(printf '%s' "$UPDATER_BODY" | STRICT_SEMVER_MODULE="$SCRIPT_DIR/../web-harness/src/strictSemver.mjs" node --input-type=module -e '
    import { readFileSync } from "node:fs";
    import { pathToFileURL } from "node:url";
    const { isStrictSemVer } = await import(pathToFileURL(process.env.STRICT_SEMVER_MODULE).href);
    const value = JSON.parse(readFileSync(0, "utf8")).version;
    if (typeof value !== "string" || value === "0.0.0" || !isStrictSemVer(value)) process.exit(1);
    process.stdout.write(value);
  ' 2>/dev/null || true)"
  if [ -z "$UPDATER_VERSION" ]; then
    echo "FAIL: updater manifest -- response did not contain a valid nonzero version" >&2
    FAILURES=$((FAILURES + 1))
  else
    echo "ok   updater manifest reports version $UPDATER_VERSION"
  fi
fi

EXPECTED_VERSION_OVERRIDE="${PETAL_EXPECTED_VERSION:-}"
EXPECTED_VERSION="${EXPECTED_VERSION_OVERRIDE:-$UPDATER_VERSION}"
if [ -n "$EXPECTED_VERSION_OVERRIDE" ] && [ "$EXPECTED_VERSION_OVERRIDE" != "$UPDATER_VERSION" ]; then
  echo "FAIL: expected release version $EXPECTED_VERSION_OVERRIDE does not match updater version $UPDATER_VERSION" >&2
  FAILURES=$((FAILURES + 1))
fi

DOWNLOAD_HEADERS="$(curl -s ${CURL_EXTRA[@]+"${CURL_EXTRA[@]}"} -D - -o /dev/null "$DOWNLOAD_URL" 2>/dev/null)"
DOWNLOAD_STATUS="$(printf '%s\n' "$DOWNLOAD_HEADERS" | awk 'tolower($1) ~ /^http/ { status=$2 } END { print status }')"
DOWNLOAD_LOCATION="$(printf '%s\n' "$DOWNLOAD_HEADERS" | awk 'tolower($1) == "location:" { sub(/^[^:]*:[[:space:]]*/, ""); print; exit }')"
if [ "$DOWNLOAD_STATUS" != "302" ]; then
  echo "FAIL: desktop download endpoint -- GET $DOWNLOAD_URL expected 302, got ${DOWNLOAD_STATUS:-unknown}" >&2
  FAILURES=$((FAILURES + 1))
elif [ -z "$EXPECTED_VERSION" ] || ! printf '%s' "$DOWNLOAD_LOCATION" | grep -Fq -- "Petal_${EXPECTED_VERSION}_universal.dmg"; then
  echo "FAIL: desktop download endpoint -- redirect target did not contain Petal_${EXPECTED_VERSION}_universal.dmg (got ${DOWNLOAD_LOCATION:-missing})" >&2
  FAILURES=$((FAILURES + 1))
else
  echo "ok   desktop download redirects to $DOWNLOAD_LOCATION"
fi
fi

# Join-link interstitial (api/j.ts) moved here from the backend project (see
# verify-backend-live.sh's header) so short links live at meet.petal.live.
check "invite route resolves via the bare-code rewrite (/:code -> /api/j)" \
  "$BASE_URL/abc-defg-hjk" 200 \
  'Join room|brand-mark|code-copy'

check "invite route resolves via the label+code rewrite (/:label/:code -> /api/j)" \
  "$BASE_URL/eng-sync/abc-defg-hjk" 200 \
  'Opening the desktop app'

check "invite route rejects a malformed code (proves /api/j itself is live, not just 200-by-accident)" \
  "$BASE_URL/not-a-code" 400 \
  ''

# Pull the referenced CSS bundle and check it for markers of landed
# remote-window-header parity work -- these are specific enough that a stale
# (pre-#225 or pre-today) deploy will NOT contain them.
if [ -z "$CSS_PATH" ]; then
  :
else
  check_body_contains "deployed CSS has the #225 native-parity header (sliding indicator, not the old floating card)" \
    "$BASE_URL/$CSS_PATH" \
    'remote-window-header__active-indicator'
  check_body_contains "deployed CSS reserves space for the header above the video (not a bare z-index overlay)" \
    "$BASE_URL/$CSS_PATH" \
    'has-remote-window-header video'
  check_body_contains "deployed CSS has the debug-overlay FPS sparkline" \
    "$BASE_URL/$CSS_PATH" \
    'remote-window-stats__spark'
fi

if [ -z "$JS_PATH" ]; then
  :
elif [ -n "$PREPUBLISH" ]; then
  echo "skip bundle-version parity check (PETAL_PREPUBLISH)"
elif [ -z "$EXPECTED_VERSION" ]; then
  echo "FAIL: cannot verify the deployed build version without an updater version" >&2
  FAILURES=$((FAILURES + 1))
else
  JS_ESCAPED_VERSION="$(printf '%s' "$EXPECTED_VERSION" | node -e '
    const fs = require("node:fs");
    process.stdout.write(fs.readFileSync(0, "utf8").replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  ')"
  JS_VERSION_PATTERN="version[[:space:]]*:[[:space:]]*\"$JS_ESCAPED_VERSION\""
  check_body_contains "deployed JS bundle contains the updater release version (browser rendering is validated separately)" \
    "$BASE_URL/$JS_PATH" \
    "$JS_VERSION_PATTERN"
fi

echo
if [ "$FAILURES" -gt 0 ]; then
  echo "verify-web-harness-live: $FAILURES check(s) FAILED -- the deployed web-harness is stale or broken" >&2
  exit 1
fi
echo "verify-web-harness-live: all checks passed against the LIVE deployment"

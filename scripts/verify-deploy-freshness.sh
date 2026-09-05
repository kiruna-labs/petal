#!/usr/bin/env bash
#
# Fundamental gap this closes: backend/ and web-harness/ deploy SEPARATELY
# from `git push` -- a plain `vercel --prod` from a local checkout (see
# verify-backend-live.sh / verify-web-harness-live.sh headers for the two
# incidents this project has already had from a forgotten redeploy). Those
# two scripts catch a stale deploy only for specific features someone
# thought to hand-write a marker check for. This script is general: it
# reads the commit each live deployment was actually built from and fails
# if origin/main has newer commits under that service's own subtree which
# were never deployed. It cannot fail on drift elsewhere in the repo (e.g.
# apps/desktop/ alone moving on does not make a web-harness deploy stale).
#
# Requires each deployment to expose its build commit:
#   - web-harness: vite.config.ts's `buildInfoFile` plugin emits
#     /build-info.json into the static build automatically -- no extra step.
#   - backend: api/version.ts reads PETAL_DEPLOY_COMMIT from the runtime
#     environment, which is NOT automatic. Pass it explicitly on every
#     backend deploy:
#       cd backend && vercel --prod -e PETAL_DEPLOY_COMMIT=$(git rev-parse HEAD)
#     (see docs/RELEASING.md).
#
# Run this as part of the pre-release checklist, or any time a backend/ or
# web-harness/ change is expected to be live and you want to confirm it
# actually is, rather than assuming the last person to touch it deployed.
set -uo pipefail

WEB_HARNESS_URL="${PETAL_WEB_HARNESS_URL:-https://meet.petal.live}"
BACKEND_URL="${PETAL_BACKEND_URL:-https://app.petal.live}"
REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
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

extract_commit() {
  node -e '
    const fs = require("node:fs");
    let data;
    try { data = JSON.parse(fs.readFileSync(0, "utf8")); } catch { process.exit(1); }
    if (typeof data.commit !== "string" || !/^[0-9a-f]{40}$/.test(data.commit)) process.exit(1);
    process.stdout.write(data.commit);
  ' 2>/dev/null
}

check_freshness() {
  local name="$1" url="$2"
  shift 2
  # Remaining args are every subtree whose changes make this deploy stale --
  # not just the service's own directory. web-harness compiles the monorepo
  # `shared/` package into its bundle (via the web-harness/shared symlink), so
  # a shared/-only commit changes the web build without touching web-harness/.
  local subtrees=("$@")
  local body commit

  body="$(curl -s ${CURL_EXTRA[@]+"${CURL_EXTRA[@]}"} "$url" 2>/dev/null)"
  commit="$(printf '%s' "$body" | extract_commit)"
  if [ -z "$commit" ]; then
    echo "FAIL: $name -- GET $url did not return a valid full commit SHA (got: $body)" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi

  if ! git -C "$REPO_ROOT" cat-file -e "${commit}^{commit}" 2>/dev/null; then
    echo "FAIL: $name -- deployed commit $commit is unknown to this checkout; run 'git fetch origin main' first" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi

  if ! git -C "$REPO_ROOT" merge-base --is-ancestor "$commit" origin/main 2>/dev/null; then
    echo "FAIL: $name -- deployed commit $commit is NOT an ancestor of origin/main (deployed from a stray branch?)" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi

  if ! git -C "$REPO_ROOT" diff --quiet "$commit" origin/main -- "${subtrees[@]}"; then
    local behind
    behind="$(git -C "$REPO_ROOT" rev-list --count "$commit"..origin/main -- "${subtrees[@]}")"
    echo "FAIL: $name -- deployed commit $commit is STALE: {${subtrees[*]}} has $behind newer commit(s) on origin/main not yet live" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi

  echo "ok   $name is fresh (deployed $commit, no undeployed {${subtrees[*]}} changes on origin/main)"
}

echo "verify-deploy-freshness: comparing live deployments against origin/main"
git -C "$REPO_ROOT" fetch origin main --quiet 2>/dev/null || echo "warn: could not fetch origin/main; comparing against the local ref" >&2

check_freshness "web-harness (meet.petal.live)" "$WEB_HARNESS_URL/build-info.json" web-harness shared contracts
check_freshness "backend (app.petal.live)" "$BACKEND_URL/api/version" backend contracts

echo
if [ "$FAILURES" -gt 0 ]; then
  echo "verify-deploy-freshness: $FAILURES check(s) FAILED -- redeploy the stale service(s) before cutting a release" >&2
  exit 1
fi
echo "verify-deploy-freshness: all live deployments match their latest committed source"

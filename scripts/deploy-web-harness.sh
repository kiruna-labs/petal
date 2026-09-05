#!/usr/bin/env bash
#
# #662: `web-harness/vite.config.ts` resolves `@petal/shared` through
# `web-harness/shared`, a symlink to the monorepo-root `shared/` package.
# That works for local dev (real filesystem, symlink follows fine) but
# `vercel deploy` uploads a snapshot of the invocation cwd, and a symlink
# pointing outside that cwd has nothing to resolve to on Vercel's remote
# build machine -- every deploy since shared/ was introduced (96f1bfd3,
# unifying desktop + web-harness UI/logic) failed with "Cannot find module
# '@petal/shared/...'". The live site had been silently serving a 5-day-old
# build the whole time (Vercel never promotes a failed build, so the
# failure was invisible unless you went looking).
#
# This script stages a throwaway copy of web-harness/ in a system temp dir
# and dereferences the symlink (`rsync -L`) so `shared/` materializes as a
# real, self-contained copy INSIDE that staged copy only -- never inside the
# actual repo, so `shared/` stays the single source of truth on disk
# (CLAUDE.md: never duplicate it into a per-client copy). Everything else
# about the deploy (web-harness/vercel.json, its Vercel project link, the
# api/ directory's position) is unchanged from the pre-#662 working deploy.
#
# Deploying straight from the repo root was tried and rejected: this repo's
# top level also contains .claude/ (cross-session data, tens of GB, can
# contain real secrets pasted into past conversations), a multi-GB .git/,
# and a 40GB+ apps/ -- uploading the literal repo root to a third party is
# not safe, and a hand-built .vercelignore allowlist was judged too fragile
# for that much blast radius.
# --build-only: build the staged, self-contained copy locally and stop --
# no Vercel CLI, auth, or network needed. Used by scripts/ci-local.sh so a
# future shared/ change that breaks the ISOLATED build (the thing that broke
# for #662) fails a local gate instead of only showing up as a silent
# production deploy failure. Real deploys should still use this script for
# every actual push to Vercel, since only a real `vercel` invocation proves
# the function/rewrite config Vercel's remote build applies on top of this.
BUILD_ONLY=0
if [ "${1:-}" = "--build-only" ]; then
  BUILD_ONLY=1
  shift
fi

set -euo pipefail

# #788 (owner-decided posture, priority rubric 2026-08-24: "always have the
# data to catch crashes"): web-harness crash reporting defaults ON for every
# deploy path rather than requiring an operator to remember to set
# VITE_SENTRY_DSN. The same default is baked by web-harness/vite.config.ts's
# define fallback (the single source for Vercel's remote build -- an inline
# vercel.json buildCommand default exceeded Vercel's 256-char limit and
# failed the deploy API). If you change this value, change vite.config.ts's
# DEFAULT_SENTRY_DSN in the same commit; verify_sentry_dsn_baked below
# guards the invariant either way.
DEFAULT_SENTRY_DSN="https://0e3aed022eea70d6e9c68b1804253e69@o4510882392899584.ingest.us.sentry.io/4511711774375937"

# #788 fail-closed gate: a default DSN is only as good as proof it actually
# landed in the shipped bundle. Grep the built JS for the Sentry ingest
# hostname (present verbatim in the minified Sentry.init() call) and refuse
# to deploy -- exit 1 before any `vercel` invocation -- if it's missing.
# This is what makes "default-on" fail-closed instead of fail-silent: a
# typo'd or emptied VITE_SENTRY_DSN would otherwise ship a build with web
# crash reporting silently off, exactly like the vercel.json gap this issue
# closed.
verify_sentry_dsn_baked() {
  local dist_dir="$1"
  if ! grep -rl "ingest.us.sentry.io" "$dist_dir"/assets/*.js >/dev/null 2>&1; then
    echo "FATAL: built web-harness bundle in $dist_dir does not contain a Sentry ingest URL -- this build would ship with web crash reporting OFF. Refusing to deploy. Check VITE_SENTRY_DSN and see the DEFAULT_SENTRY_DSN note in this script (#788)." >&2
    exit 1
  fi
  echo "Verified: web-harness bundle carries a Sentry DSN (crash reporting armed)."
}

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# Portable template form: GNU mktemp (ubuntu runners, #916) rejects the
# BSD/macOS `-t prefix` shorthand with "too few X's in template".
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/petal-web-harness-deploy.XXXXXX")"
cleanup() { rm -rf "$STAGE"; }
trap cleanup EXIT

# In CI (release.yml deploy-web) the project is identified by VERCEL_ORG_ID +
# VERCEL_PROJECT_ID, which the Vercel CLI honors instead of a .vercel/project.json
# link file; only a human deploy from a laptop needs the link file.
LINKED_BY_ENV=0
if [ -n "${VERCEL_ORG_ID:-}" ] && [ -n "${VERCEL_PROJECT_ID:-}" ]; then LINKED_BY_ENV=1; fi
if [ "$BUILD_ONLY" -eq 0 ] && [ "$LINKED_BY_ENV" -eq 0 ] && [ ! -f "$REPO_ROOT/web-harness/.vercel/project.json" ]; then
  echo "FATAL: $REPO_ROOT/web-harness/.vercel/project.json missing -- run 'vercel link' inside web-harness/ once first" >&2
  exit 1
fi

echo "Staging deploy copy at $STAGE"
# -L dereferences the shared symlink into a real copy; excludes keep the
# upload small and never carry a stale local .vercel link into the copy.
rsync -aL --exclude 'node_modules' --exclude 'dist' --exclude '.vercel' --exclude '.turbo' --exclude 'coverage' \
  "$REPO_ROOT/web-harness/" "$STAGE/"

echo "Verifying the staged copy is genuinely self-contained (no ../shared reference survives)..."
if [ ! -d "$STAGE/shared" ] || [ -L "$STAGE/shared" ]; then
  echo "FATAL: $STAGE/shared is missing or still a symlink -- rsync -L did not dereference it" >&2
  exit 1
fi

COMMIT="$(git -C "$REPO_ROOT" rev-parse HEAD)"

cd "$STAGE"
npm install

if [ "$BUILD_ONLY" -eq 1 ]; then
  echo "Building isolated copy (no deploy) for commit $COMMIT"
  # VERCEL=1 matches what Vercel's real remote build machine sets
  # automatically -- vite.config.ts's buildInfo() tolerates missing
  # apps/desktop/ metadata only under this flag (see readDesktopMetadata's
  # allowMissingDesktopMetadata caller).
  VERCEL=1 PETAL_DEPLOY_COMMIT="$COMMIT" VITE_PETAL_BACKEND_URL="${VITE_PETAL_BACKEND_URL:-https://app.petal.live}" VITE_USERDISPATCH_PUBLIC_KEY="${VITE_USERDISPATCH_PUBLIC_KEY:-pk_2692a1152395a821c2e571ba38b92b3edca7d16329e485fa}" VITE_SENTRY_DSN="${VITE_SENTRY_DSN:-$DEFAULT_SENTRY_DSN}" npm run build
  verify_sentry_dsn_baked "$STAGE/dist"
  exit 0
fi

# #788: build the same staged copy locally first, purely to run the
# fail-closed Sentry DSN gate below BEFORE we ever hand off to `vercel`.
# Vercel's own remote build (vercel.json's buildCommand) rebuilds this same
# source again with the same env defaults -- this local pass is a
# pre-flight proof, not a substitute for it, since only the real `vercel`
# invocation exercises the function/rewrite config layered on top.
echo "Building isolated copy locally to verify Sentry DSN is baked before deploying (#788 fail-closed gate)..."
VERCEL=1 PETAL_DEPLOY_COMMIT="$COMMIT" VITE_PETAL_BACKEND_URL="${VITE_PETAL_BACKEND_URL:-https://app.petal.live}" VITE_USERDISPATCH_PUBLIC_KEY="${VITE_USERDISPATCH_PUBLIC_KEY:-pk_2692a1152395a821c2e571ba38b92b3edca7d16329e485fa}" VITE_SENTRY_DSN="${VITE_SENTRY_DSN:-$DEFAULT_SENTRY_DSN}" npm run build
verify_sentry_dsn_baked "$STAGE/dist"

if [ "$LINKED_BY_ENV" -eq 0 ]; then
  mkdir -p "$STAGE/.vercel"
  cp "$REPO_ROOT/web-harness/.vercel/project.json" "$STAGE/.vercel/project.json"
fi

echo "Deploying commit $COMMIT"
# Forward all CLI args (e.g. --prod --yes, or nothing for a preview deploy).
# -b is load-bearing: this staging copy has no .git, so vite.config.ts's own
# `git rev-parse HEAD` fallback can't find one either -- see fullCommit()'s
# comment there and docs/RELEASING.md's "Deploying web-harness" section.
# VITE_PETAL_POSTHOG_KEY is optional: production should set it as a Vercel
# project env so the remote build bakes it. Never default the token here.
VERCEL_CMD=(vercel "$@" -b "PETAL_DEPLOY_COMMIT=$COMMIT")
if [ -n "${VITE_PETAL_POSTHOG_KEY:-}" ]; then
  VERCEL_CMD+=(-b "VITE_PETAL_POSTHOG_KEY=${VITE_PETAL_POSTHOG_KEY}")
fi
# The CLI prints the deployment URL (and nothing else) on stdout. release.yml
# needs it to verify + promote a `--skip-domain` staged deployment, so tee it
# to PETAL_DEPLOY_URL_FILE when asked.
if [ -n "${PETAL_DEPLOY_URL_FILE:-}" ]; then
  "${VERCEL_CMD[@]}" | tee "$PETAL_DEPLOY_URL_FILE"
  test "${PIPESTATUS[0]}" -eq 0
else
  "${VERCEL_CMD[@]}"
fi

#!/usr/bin/env bash
# Run scripts/publish-blob.mjs from a scratch npm dir that provides
# @vercel/blob (a library, not a CLI). ESM ignores NODE_PATH, so the
# publisher and its local import publish-blob-lib.mjs are copied INTO the
# scratch dir. The whole environment is inherited (this `exec`s node and
# filters nothing) -- so a gate's expected value reaches publish-blob.mjs iff
# release.yml's publish step sets it, whatever the variable is named. An
# earlier version of this comment listed PETAL_*/BLOB_*/VERSION/TAG as though
# that were a filter; it never was, and reading it as one sends you looking
# for the wrong bug. Set PETAL_PUBLISH_DRY_RUN=1 to run every gate and stop
# before uploading.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRATCH="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/blobpub"
mkdir -p "$SCRATCH"
( cd "$SCRATCH" && [ -d node_modules/@vercel/blob ] || { npm init -y >/dev/null 2>&1 && npm i @vercel/blob@^2 >/dev/null 2>&1; } )
cp "$ROOT/scripts/publish-blob.mjs" "$SCRATCH/publish-blob.mjs"
cp "$ROOT/scripts/publish-blob-lib.mjs" "$SCRATCH/publish-blob-lib.mjs"
cd "$SCRATCH" && exec node publish-blob.mjs

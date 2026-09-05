#!/usr/bin/env bash
# Run scripts/publish-blob.mjs from a scratch npm dir that provides
# @vercel/blob (a library, not a CLI). ESM ignores NODE_PATH, so the
# publisher and its local import publish-blob-lib.mjs are copied INTO the
# scratch dir. All PETAL_*/BLOB_*/VERSION/TAG env is inherited; set
# PETAL_PUBLISH_DRY_RUN=1 to run every gate and stop before uploading.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRATCH="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/blobpub"
mkdir -p "$SCRATCH"
( cd "$SCRATCH" && [ -d node_modules/@vercel/blob ] || { npm init -y >/dev/null 2>&1 && npm i @vercel/blob@^2 >/dev/null 2>&1; } )
cp "$ROOT/scripts/publish-blob.mjs" "$SCRATCH/publish-blob.mjs"
cp "$ROOT/scripts/publish-blob-lib.mjs" "$SCRATCH/publish-blob-lib.mjs"
cd "$SCRATCH" && exec node publish-blob.mjs

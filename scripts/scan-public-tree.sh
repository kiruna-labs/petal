#!/usr/bin/env bash
# Scan everything that would ship in the PUBLIC repository for PII and
# secret-shaped strings.
#
# The docs-site scanner (site/scripts/scan-for-pii.mjs) only ever looked at
# built site output, so personal detail elsewhere in the tree sailed past it.
# This runs the same deny-list over the real export set: every tracked file
# except internal/ and the agent-facing files the export drops anyway.
#
# Project-specific literals (a maintainer's name, a keychain-profile name) come
# from the untracked site/.pii-patterns.local.json — see loadLocalPatterns() in
# the scanner. Without that file only the generic patterns run, which is the
# right default for an outside contributor.
#
# Usage: scripts/scan-public-tree.sh [--staged]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

STAGE_DIR="$(mktemp -d)"
trap 'rm -rf "$STAGE_DIR"' EXIT

# Mirror the export denylist in scripts/export-public-tree.sh. Keep the two in
# sync: a path excluded there but scanned here is noise, and a path shipped
# there but skipped here is an unscanned publication.
EXCLUDES=(
  ':!internal/**'
  ':!CLAUDE.md'
  ':!AGENTS.md'
  ':!.claude/**'
  # Third-party content we redistribute but did not author. Upstream changelogs
  # and lockfiles legitimately carry other people's contact details; they are
  # not this project's PII, and rewriting them would corrupt the vendored copy.
  ':!apps/desktop/vendor/**'
  ':!**/package-lock.json'
)

FILES=()
if [[ "${1:-}" == "--staged" ]]; then
  while IFS= read -r line; do FILES+=("$line"); done \
    < <(git diff --cached --name-only --diff-filter=ACMR -- . "${EXCLUDES[@]}")
else
  while IFS= read -r line; do FILES+=("$line"); done \
    < <(git ls-files -- . "${EXCLUDES[@]}")
fi

if [[ ${#FILES[@]} -eq 0 ]]; then
  echo "[scan-public-tree] no files to scan"
  exit 0
fi

for f in "${FILES[@]}"; do
  [[ -f "$f" ]] || continue
  mkdir -p "$STAGE_DIR/$(dirname "$f")"
  cp "$f" "$STAGE_DIR/$f"
done

# The scanner only reads text-ish extensions, so binaries in the staged copy
# are ignored rather than false-positived.
echo "[scan-public-tree] scanning ${#FILES[@]} exportable file(s)…"
node site/scripts/scan-for-pii.mjs "$STAGE_DIR"

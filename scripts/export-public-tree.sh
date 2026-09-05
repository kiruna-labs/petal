#!/usr/bin/env bash
# Export a clean, publishable snapshot of the repository.
#
# Produces a directory containing ONLY the files that belong in the public
# repository — no history, no internal/, no agent-facing files, no working-tree
# strays. This is deliberately built from `git archive` (tracked files at a
# committed revision) rather than a directory copy: a copy would sweep up
# .env.local, build output, editor state, and anything else sitting untracked
# in the working tree.
#
# It does NOT create or push a repository. Review the output, then follow
# internal/OPEN_SOURCING.md.
#
# Usage:
#   scripts/export-public-tree.sh [--rev <commit-ish>] [--out <dir>]
#
# Defaults: --rev HEAD, --out ../petal-public-export
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

REV="HEAD"
OUT="$REPO_ROOT/../petal-public-export"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --rev) REV="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

# Paths that must never reach the public repository. Keep in sync with the
# EXCLUDES list in scripts/scan-public-tree.sh.
#
# internal/  — history, planning docs, the design bundle, operator scripts
# CLAUDE.md / AGENTS.md / .claude/ — agent-facing operating instructions
EXCLUDE_PATHS=(
  'internal'
  'CLAUDE.md'
  'AGENTS.md'
  '.claude'
)

fail() { printf '\033[1;31m%s\033[0m\n' "$1" >&2; exit 1; }
ok()   { printf '\033[1;32m%s\033[0m\n' "$1"; }

if [[ -e "$OUT" ]]; then
  fail "Output path already exists: $OUT
Remove it yourself, or pass a different --out. This script will not delete anything."
fi

RESOLVED="$(git rev-parse --verify "$REV^{commit}")"
echo "Exporting $REV ($RESOLVED) -> $OUT"

mkdir -p "$OUT"
git archive --format=tar "$RESOLVED" | tar -x -C "$OUT"

for path in "${EXCLUDE_PATHS[@]}"; do
  if [[ -e "$OUT/$path" ]]; then
    rm -rf "${OUT:?}/$path"
    echo "  removed $path"
  fi
done

echo
echo "=== Verification ==="

# 1. Nothing excluded survived.
for path in "${EXCLUDE_PATHS[@]}"; do
  [[ -e "$OUT/$path" ]] && fail "FAIL: $path is still present in the export"
done
ok "  no excluded paths present"

# 2. Required legal files are present.
for required in LICENSE NOTICE THIRD_PARTY_NOTICES.md TRADEMARKS.md \
                SECURITY.md PRIVACY.md CONTRIBUTING.md CODE_OF_CONDUCT.md README.md; do
  [[ -f "$OUT/$required" ]] || fail "FAIL: required file missing from export: $required"
done
ok "  license, governance and community files present"

# 3. Font licenses travel with the font binaries (SIL OFL 1.1 §2).
while IFS= read -r fontdir; do
  [[ -f "$fontdir/OFL.txt" ]] || fail "FAIL: fonts in $fontdir ship without OFL.txt"
done < <(find "$OUT" -name '*.woff2' -exec dirname {} \; | sort -u)
ok "  every font directory carries OFL.txt"

# 4. Vendored Apache-2.0 copies carry their license text (Apache-2.0 §4a).
for vendored in "$OUT"/apps/desktop/vendor/*/; do
  [[ -d "$vendored" ]] || continue
  if ! compgen -G "$vendored/LICENSE*" >/dev/null; then
    fail "FAIL: vendored $(basename "$vendored") ships without a LICENSE"
  fi
done
ok "  every vendored dependency carries a LICENSE"

# 5. No environment or provider state.
if find "$OUT" -name '.env' -o -name '.env.*' ! -name '.env.example' \
     -o -name '.vercel' -o -name '.npmrc' | grep -q .; then
  find "$OUT" -name '.env' -o -name '.env.*' ! -name '.env.example' \
    -o -name '.vercel' -o -name '.npmrc' >&2
  fail "FAIL: environment/provider state present in export"
fi
ok "  no .env / .vercel / .npmrc state"

# 6. No build output or large files.
if find "$OUT" -type d \( -name target -o -name target-peer -o -name node_modules \) | grep -q .; then
  fail "FAIL: build output present in export"
fi
LARGE="$(find "$OUT" -type f -size +1M -not -path '*/.git/*' | head -20)"
if [[ -n "$LARGE" ]]; then
  echo "WARNING: files over 1MB in the export:" >&2
  echo "$LARGE" >&2
fi
ok "  no build output"

# 7. PII / secret scan over the exported tree, not the repo.
echo
echo "=== PII / secret scan of the export ==="
if ! node "$REPO_ROOT/site/scripts/scan-for-pii.mjs" "$OUT"; then
  fail "FAIL: PII/secret scan found hits in the exported tree"
fi

echo
ok "Export ready: $OUT"
cat <<EOF

It is a plain directory with no git history. Next steps are in
internal/OPEN_SOURCING.md — review the contents by hand before initializing a
repository, and remember the commit must be authored with the pseudonymous
noreply identity.
EOF

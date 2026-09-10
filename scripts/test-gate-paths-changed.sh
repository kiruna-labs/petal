#!/usr/bin/env bash
#
# Both-directions test for scripts/gate-paths-changed.sh (#133).
#
# The gate this script feeds decides whether a branch-protection-required check
# does real work or passes as not-applicable. CLAUDE.md's rule is that a gate is
# tested in BOTH directions before it is relied on -- a check whose negative
# result has never been observed is worth nothing. So: it must say `true` for a
# matching file, `false` for a non-matching one, `true` for a MIXED set (the
# exact case that made `paths-ignore` double-report), and it must refuse a
# pattern it cannot honour rather than matching nothing.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SUT="$ROOT/scripts/gate-paths-changed.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

PATTERNS='# comment line
apps/desktop/src-tauri/**
contracts/**
scripts/ci-local.sh

.github/workflows/rust-gate.yml'

fails=0
run_case() {
  local name="$1" want="$2"; shift 2
  printf '%s\n' "$@" > "$TMP/files"
  local got
  got="$(printf '%s\n' "$PATTERNS" | "$SUT" --files-from "$TMP/files" 2>/dev/null)" || got="EXIT=$?"
  if [[ "$got" == "changed=$want" ]]; then
    printf '  ok   %-52s -> %s\n' "$name" "$got"
  else
    printf '  FAIL %-52s -> %s (wanted changed=%s)\n' "$name" "$got" "$want"
    fails=$((fails + 1))
  fi
}

echo "gate-paths-changed.sh: positive direction"
run_case "file under a dir pattern"            true  "apps/desktop/src-tauri/src/lib.rs"
run_case "deeply nested file under a dir"      true  "apps/desktop/src-tauri/src/session/room.rs"
run_case "exact-path pattern"                  true  "scripts/ci-local.sh"
run_case "workflow file listed exactly"        true  ".github/workflows/rust-gate.yml"

echo "gate-paths-changed.sh: negative direction"
run_case "docs-only change"                    false "docs/TESTING.md"
run_case "sibling dir with a shared prefix"    false "apps/desktop/src/routes/+page.svelte"
run_case "prefix is not a path boundary"       false "apps/desktop/src-tauri-notes.md"
run_case "exact pattern is not a prefix"       false "scripts/ci-local.sh.bak"
run_case "another gate's paths"                false "web-harness/src/main.ts"

echo "gate-paths-changed.sh: the mixed case that broke paths-ignore (#133)"
run_case "rust + docs in one PR"               true  "apps/desktop/src-tauri/src/lib.rs" "docs/TESTING.md"
run_case "docs + rust, other order"            true  "README.md" "contracts/petal-contracts.json"

echo "gate-paths-changed.sh: an unhonourable pattern fails loudly"
for bad in 'apps/**/src' 'apps/desktop/*.rs' '/absolute/path' 'trailing/slash/'; do
  printf '%s\n' "docs/TESTING.md" > "$TMP/files"
  if printf '%s\n' "$bad" | "$SUT" --files-from "$TMP/files" >/dev/null 2>&1; then
    printf '  FAIL %-52s -> accepted, must be rejected\n' "$bad"
    fails=$((fails + 1))
  else
    printf '  ok   %-52s -> rejected\n' "$bad"
  fi
done

echo "gate-paths-changed.sh: no PR context fails SAFE (runs the gate)"
got="$(printf '%s\n' "$PATTERNS" | env -u GH_REPO -u GITHUB_REPOSITORY -u PR_NUMBER "$SUT" 2>/dev/null)"
if [[ "$got" == "changed=true" ]]; then
  printf '  ok   %-52s -> %s\n' "missing PR context" "$got"
else
  printf '  FAIL %-52s -> %s (wanted changed=true)\n' "missing PR context" "$got"
  fails=$((fails + 1))
fi

if [[ $fails -ne 0 ]]; then
  printf '\n\033[1;31m%d gate-paths-changed.sh case(s) failed\033[0m\n' "$fails" >&2
  exit 1
fi
printf '\n\033[1;32mgate-paths-changed.sh: all cases passed (both directions)\033[0m\n'

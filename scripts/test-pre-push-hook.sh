#!/usr/bin/env bash
# Contract tests for scripts/git-hooks/pre-push's root + base resolution. No
# cargo, no network: every repo is a throwaway under mktemp, and the hook runs
# in PETAL_PRE_PUSH_DRY_RUN=1 mode (prints root/base/decision, never builds).
#
# What it pins down:
#   - the gate targets the worktree being pushed, even when the hook file is
#     invoked from a DIFFERENT checkout (absolute core.hooksPath);
#   - a new branch's base comes from the remote being pushed to, not a
#     hard-coded origin/main (which here has unrelated history, so the old
#     hook found no merge base and silently skipped the gate);
#   - no usable base fails CLOSED (gate runs), never open.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOOK="$ROOT/scripts/git-hooks/pre-push"
Z=0000000000000000000000000000000000000000

TMP="$(mktemp -d /tmp/petal-pre-push-test.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT INT TERM
fail() { echo "FAIL: $*" >&2; exit 1; }

g() { git -c user.email=test@example.invalid -c user.name=test -c core.hooksPath=/dev/null "$@"; }

# "public": the real upstream. "origin": a mirror with unrelated history.
g init -q -b main "$TMP/seed"
mkdir -p "$TMP/seed/contracts" "$TMP/seed/docs"
echo '{}' >"$TMP/seed/contracts/c.json"
echo doc >"$TMP/seed/docs/d.md"
g -C "$TMP/seed" add -A && g -C "$TMP/seed" commit -qm seed
g clone -q --bare "$TMP/seed" "$TMP/public.git"
g init -q -b main "$TMP/mirror" && g -C "$TMP/mirror" commit -q --allow-empty -m unrelated
g clone -q --bare "$TMP/mirror" "$TMP/origin.git"

g clone -q -o public "$TMP/public.git" "$TMP/main"
g -C "$TMP/main" remote add origin "$TMP/origin.git"
g -C "$TMP/main" fetch -q origin
g -C "$TMP/main" worktree add -q -b feature "$TMP/wt" public/main
echo '{"x":1}' >"$TMP/wt/contracts/c.json"
g -C "$TMP/wt" commit -qam contracts
RUST_SHA="$(git -C "$TMP/wt" rev-parse HEAD)"
g -C "$TMP/wt" checkout -q -b docs-only public/main
echo doc2 >"$TMP/wt/docs/d.md"
g -C "$TMP/wt" commit -qam docs
DOCS_SHA="$(git -C "$TMP/wt" rev-parse HEAD)"
WT_REAL="$(cd "$TMP/wt" && pwd -P)"

# Run the hook the way git does (cwd = pushing worktree), from a path OUTSIDE
# that worktree -- the absolute-hooksPath case.
cp "$HOOK" "$TMP/pre-push"
run_hook() { # <sha> <remote_sha> <remote-arg> <url>
  (cd "$TMP/wt" && printf 'refs/heads/f %s refs/heads/f %s\n' "$1" "$2" |
    PETAL_PRE_PUSH_DRY_RUN=1 PETAL_FORBIDDEN_AUTHOR_EMAILS=/nonexistent "$TMP/pre-push" "$3" "$4" 2>&1)
}
expect() { grep -qF -- "$2" <<<"$1" || fail "expected '$2' in: $1"; }

# 1. New branch with a contracts/ change, pushed to public: worktree root,
#    base = merge-base with public's default branch, gate runs.
OUT="$(run_hook "$RUST_SHA" "$Z" public "$TMP/public.git")"
expect "$OUT" "root=$WT_REAL "
expect "$OUT" "merge-base with public/"
expect "$OUT" "decision=gate"

# 2. Same, docs-only: skipped (proves the base is real, not "gate everything").
OUT="$(run_hook "$DOCS_SHA" "$Z" public "$TMP/public.git")"
expect "$OUT" "merge-base with public/"
expect "$OUT" "decision=skip"

# 3. `git push <url>`: the URL is mapped back to the public remote.
OUT="$(run_hook "$RUST_SHA" "$Z" "$TMP/public.git" "$TMP/public.git")"
expect "$OUT" "remote=public"
expect "$OUT" "decision=gate"

# 4. Pushed to the unrelated-history mirror: no merge base anywhere -> fail closed.
OUT="$(run_hook "$DOCS_SHA" "$Z" origin "$TMP/origin.git")"
expect "$OUT" "base=<none>"
expect "$OUT" "decision=gate"

# 5. Update whose remote tip was never fetched: falls back to the merge-base
#    instead of a failing `git diff` that used to skip the gate.
OUT="$(run_hook "$RUST_SHA" 1234567890123456789012345678901234567890 public "$TMP/public.git")"
expect "$OUT" "merge-base with public/"
expect "$OUT" "decision=gate"

# 6. Force-push of a rebased docs-only branch: main gained a contracts/ change
#    that the old tip lacks. Diffing old-tip..new would wrongly gate; the base
#    must be the merge-base with public/main instead -> skip.
OLD_DOCS_SHA="$DOCS_SHA"
g -C "$TMP/main" checkout -q main
echo '{"main":1}' >"$TMP/main/contracts/c.json"
g -C "$TMP/main" commit -qam "main contracts" && g -C "$TMP/main" push -q public main
g -C "$TMP/wt" fetch -q public
g -C "$TMP/wt" rebase -q public/main
REBASED_SHA="$(git -C "$TMP/wt" rev-parse HEAD)"
OUT="$(run_hook "$REBASED_SHA" "$OLD_DOCS_SHA" public "$TMP/public.git")"
expect "$OUT" "(non-fast-forward)"
expect "$OUT" "decision=skip"

# 7. Fast-forward docs-only follow-up on top of an already-pushed tip: base is
#    the remote tip -> skip.
echo doc3 >"$TMP/wt/docs/d.md"
g -C "$TMP/wt" commit -qam docs3
OUT="$(run_hook "$(git -C "$TMP/wt" rev-parse HEAD)" "$REBASED_SHA" public "$TMP/public.git")"
expect "$OUT" "(remote tip)"
expect "$OUT" "decision=skip"

# 8. Rebased branch that itself changes contracts/: still gated.
echo '{"main":1,"y":2}' >"$TMP/wt/contracts/c.json"
g -C "$TMP/wt" commit -qam "branch contracts"
OUT="$(run_hook "$(git -C "$TMP/wt" rev-parse HEAD)" "$OLD_DOCS_SHA" public "$TMP/public.git")"
expect "$OUT" "(non-fast-forward)"
expect "$OUT" "decision=gate"

echo "test result: pre-push hook root/base resolution tests passed"

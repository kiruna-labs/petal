#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WRAPPER="$SCRIPT_DIR/run-with-source-provenance.sh"
TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/petal provenance wrapper.XXXXXX")"
SYNC_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/petal provenance sync.XXXXXX")"
TEST_ROOT="$(cd "$TEST_ROOT" && pwd -P)"
SYNC_ROOT="$(cd "$SYNC_ROOT" && pwd -P)"
MANIFEST_ROOT="$SYNC_ROOT/manifests"
mutation_pid=""
cleanup() {
  if [[ -n "$mutation_pid" ]] && kill -0 "$mutation_pid" 2>/dev/null; then
    kill "$mutation_pid" 2>/dev/null || true
    wait "$mutation_pid" 2>/dev/null || true
  fi
  rm -rf "$TEST_ROOT"
  rm -rf "$SYNC_ROOT"
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

git -C "$TEST_ROOT" init -q
git -C "$TEST_ROOT" config user.email test@petal.invalid
git -C "$TEST_ROOT" config user.name "Petal Test"
mkdir -p "$TEST_ROOT/apps/desktop/src-tauri" "$MANIFEST_ROOT"
printf 'tracked\n' >"$TEST_ROOT/tracked.txt"
printf '{"lockfileVersion":3}\n' >"$TEST_ROOT/apps/desktop/package-lock.json"
cat >"$TEST_ROOT/.gitignore" <<'EOF'
apps/desktop/node_modules/
apps/desktop/src-tauri/target/
EOF
git -C "$TEST_ROOT" add .gitignore apps/desktop/package-lock.json tracked.txt
git -C "$TEST_ROOT" commit -qm first

read_state() {
  (
    cd "$1"
    "$WRAPPER" sh -c \
      'printf "%s|%s|%s\n" "$PETAL_OFFICIAL_SOURCE_SHA_FULL" "$PETAL_OFFICIAL_SOURCE_STATE" "$PETAL_SOURCE_PROVENANCE_WRAPPED"'
  )
}

read_trusted_state() {
  (
    cd "$1"
    "$WRAPPER" --require-clean sh -c \
      'printf "%s|%s|%s\n" "$PETAL_OFFICIAL_SOURCE_SHA_FULL" "$PETAL_OFFICIAL_SOURCE_STATE" "$PETAL_SOURCE_PROVENANCE_WRAPPED"'
  )
}

head_sha="$(git -C "$TEST_ROOT" rev-parse HEAD)"
clean="$(read_state "$TEST_ROOT")"
IFS='|' read -r clean_sha clean_state clean_guard <<<"$clean"
[[ "$clean_sha" == "unverified" ]] || fail "raw clean tree was trusted"
[[ "$clean_state" =~ ^[0-9a-f]{64}$ ]] || fail "clean state is not SHA-256"
[[ "$clean_guard" == "$clean_state" ]] || fail "wrapper guard was not bound to clean state"

trusted="$(read_trusted_state "$TEST_ROOT")"
IFS='|' read -r trusted_sha trusted_state trusted_guard <<<"$trusted"
[[ "$trusted_sha" == "$head_sha" ]] || fail "trusted clean SHA mismatch"
[[ "$trusted_state" == "$clean_state" ]] || fail "isolated trusted state differs from caller HEAD"
[[ "$trusted_guard" == "$trusted_state" ]] || fail "trusted guard was not state-bound"

printf 'untracked\n' >"$TEST_ROOT/untracked.txt"
untracked="$(read_state "$TEST_ROOT")"
IFS='|' read -r untracked_sha untracked_state _ <<<"$untracked"
[[ "$untracked_sha" == "unverified" ]] || fail "untracked tree was trusted"
[[ "$untracked_state" != "$clean_state" ]] || fail "untracked state did not change"
printf 'edited in place\n' >"$TEST_ROOT/untracked.txt"
edited_untracked="$(read_state "$TEST_ROOT")"
IFS='|' read -r edited_untracked_sha edited_untracked_state _ <<<"$edited_untracked"
[[ "$edited_untracked_sha" == "unverified" ]] || fail "edited untracked tree was trusted"
[[ "$edited_untracked_state" != "$untracked_state" ]] ||
  fail "editing untracked content did not change source state"
if (cd "$TEST_ROOT" && "$WRAPPER" --require-clean true); then
  fail "--require-clean accepted an untracked file"
fi
rm -f "$TEST_ROOT/untracked.txt"

printf 'dirty\n' >"$TEST_ROOT/tracked.txt"
dirty="$(read_state "$TEST_ROOT")"
IFS='|' read -r dirty_sha dirty_state _ <<<"$dirty"
[[ "$dirty_sha" == "unverified" ]] || fail "dirty tracked tree was trusted"
[[ "$dirty_state" != "$clean_state" ]] || fail "dirty state did not change"
git -C "$TEST_ROOT" checkout -q -- tracked.txt
[[ "$(read_state "$TEST_ROOT")" == "$clean" ]] || fail "restored clean state mismatch"

rm -f "$TEST_ROOT/tracked.txt"
deleted="$(
  TMPDIR="$MANIFEST_ROOT" read_state "$TEST_ROOT"
)"
IFS='|' read -r deleted_sha deleted_state deleted_guard <<<"$deleted"
[[ "$deleted_sha" == "unverified" ]] || fail "raw deleted tracked path was trusted"
[[ "$deleted_state" != "$clean_state" ]] ||
  fail "deleted tracked path did not change source state"
[[ "$deleted_guard" == "$deleted_state" ]] ||
  fail "deleted tracked path guard was not state-bound"
if find "$MANIFEST_ROOT" -name 'petal-source-manifest.*' -print -quit |
  grep -q .; then
  fail "source manifest leaked after ordinary tracked deletion"
fi
git -C "$TEST_ROOT" checkout -q -- tracked.txt
[[ "$(read_state "$TEST_ROOT")" == "$clean" ]] ||
  fail "ordinary deleted tracked path did not restore cleanly"

LINKED_ROOT="$TEST_ROOT linked"
git -C "$TEST_ROOT" worktree add -q -b linked-test "$LINKED_ROOT"
linked="$(read_state "$LINKED_ROOT")"
IFS='|' read -r linked_sha linked_state linked_guard <<<"$linked"
[[ "$linked_sha" == "unverified" ]] || fail "raw linked tree was trusted"
[[ "$linked_state" =~ ^[0-9a-f]{64}$ ]] || fail "linked state is not SHA-256"
[[ "$linked_guard" == "$linked_state" ]] || fail "linked guard was not bound to linked state"
linked_trusted="$(read_trusted_state "$LINKED_ROOT")"
IFS='|' read -r linked_trusted_sha linked_trusted_state linked_trusted_guard <<<"$linked_trusted"
[[ "$linked_trusted_sha" == "$(git -C "$LINKED_ROOT" rev-parse HEAD)" ]] ||
  fail "trusted linked SHA mismatch"
[[ "$linked_trusted_state" == "$linked_state" ]] ||
  fail "trusted linked materialization differs from linked caller"
[[ "$linked_trusted_guard" == "$linked_trusted_state" ]] ||
  fail "trusted linked guard was not state-bound"

rm -f "$LINKED_ROOT/tracked.txt"
linked_deleted="$(
  TMPDIR="$MANIFEST_ROOT" read_state "$LINKED_ROOT"
)"
IFS='|' read -r linked_deleted_sha linked_deleted_state linked_deleted_guard \
  <<<"$linked_deleted"
[[ "$linked_deleted_sha" == "unverified" ]] ||
  fail "raw linked deleted tracked path was trusted"
[[ "$linked_deleted_state" != "$linked_state" ]] ||
  fail "linked deleted tracked path did not change source state"
[[ "$linked_deleted_guard" == "$linked_deleted_state" ]] ||
  fail "linked deleted tracked path guard was not state-bound"
if find "$MANIFEST_ROOT" -name 'petal-source-manifest.*' -print -quit |
  grep -q .; then
  fail "source manifest leaked after linked tracked deletion"
fi
git -C "$LINKED_ROOT" checkout -q -- tracked.txt
[[ "$(read_state "$LINKED_ROOT")" == "$linked" ]] ||
  fail "linked deleted tracked path did not restore cleanly"

nested="$(
  cd "$LINKED_ROOT"
  "$WRAPPER" "$WRAPPER" sh -c \
    'printf "%s|%s\n" "$PETAL_OFFICIAL_SOURCE_SHA_FULL" "$PETAL_SOURCE_PROVENANCE_WRAPPED"'
)"
[[ "$nested" == "unverified|$linked_state" ]] || fail "nested raw wrapper was not idempotent"

git -C "$TEST_ROOT" worktree remove "$LINKED_ROOT"

ready="$SYNC_ROOT/mutation-ready"
release="$SYNC_ROOT/mutation-release"
mutation_stderr="$SYNC_ROOT/mutation-stderr"
mutation_artifact="$SYNC_ROOT/mutation-artifact"
mutation_published="$SYNC_ROOT/mutation-published"
mkfifo "$release"
(
  cd "$TEST_ROOT"
  "$WRAPPER" --require-clean sh -c \
    'touch "$1"; cat "$2" >/dev/null; cp tracked.txt "$3"' \
    mutation-child "$ready" "$release" "$mutation_artifact" &&
    touch "$mutation_published"
) 2>"$mutation_stderr" &
mutation_pid=$!
for _ in {1..500}; do
  [[ -e "$ready" ]] && break
  sleep 0.01
done
[[ -e "$ready" ]] || fail "mutation child did not reach deterministic barrier"
printf 'changed during command\n' >"$TEST_ROOT/tracked.txt"
printf 'release\n' >"$release"
if wait "$mutation_pid"; then
  fail "wrapper accepted source mutation during command"
else
  mutation_status=$?
fi
mutation_pid=""
[[ "$mutation_status" -eq 4 ]] || fail "mutation rejection used unexpected status $mutation_status"
grep -Fq 'caller source changed while command was running; refusing success' "$mutation_stderr" ||
  fail "mutation rejection diagnostic missing"
grep -Fq 'tracked' "$mutation_artifact" ||
  fail "isolated artifact did not retain canonical tracked content"
[[ ! -e "$mutation_published" ]] ||
  fail "downstream publication ran after provenance mutation rejection"
git -C "$TEST_ROOT" checkout -q -- tracked.txt

aba_ready="$SYNC_ROOT/aba-ready"
aba_release="$SYNC_ROOT/aba-release"
aba_copied="$SYNC_ROOT/aba-copied"
aba_finish="$SYNC_ROOT/aba-finish"
aba_artifact="$SYNC_ROOT/aba-artifact"
aba_published="$SYNC_ROOT/aba-published"
mkfifo "$aba_release" "$aba_finish"
(
  cd "$TEST_ROOT"
  "$WRAPPER" --require-clean sh -c \
    'touch "$1"; cat "$2" >/dev/null; cp tracked.txt "$3"; touch "$4"; cat "$5" >/dev/null' \
    aba-child "$aba_ready" "$aba_release" "$aba_artifact" "$aba_copied" "$aba_finish" &&
    touch "$aba_published"
) &
mutation_pid=$!
for _ in {1..500}; do
  [[ -e "$aba_ready" ]] && break
  sleep 0.01
done
[[ -e "$aba_ready" ]] || fail "ABA child did not reach deterministic barrier"
printf 'transient caller mutation\n' >"$TEST_ROOT/tracked.txt"
printf 'release\n' >"$aba_release"
for _ in {1..500}; do
  [[ -e "$aba_copied" ]] && break
  sleep 0.01
done
[[ -e "$aba_copied" ]] || fail "ABA child did not consume isolated compile input"
grep -Fq 'tracked' "$aba_artifact" ||
  fail "caller ABA mutation influenced the isolated artifact"
git -C "$TEST_ROOT" checkout -q -- tracked.txt
printf 'finish\n' >"$aba_finish"
wait "$mutation_pid" || fail "isolated ABA-safe trusted command was rejected"
mutation_pid=""
[[ -e "$aba_published" ]] || fail "ABA-safe isolated artifact did not reach publication"

FAKE_BIN="$SYNC_ROOT/fake-bin"
FAKE_LOG="$SYNC_ROOT/fake-tauri-log"
mkdir -p "$FAKE_BIN" "$FAKE_LOG"
cat >"$FAKE_BIN/npm" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ "$#" -eq 1 && "$1" == "ci" ]]
[[ "$PWD" == */apps/desktop ]]
[[ -f package-lock.json ]]
mkdir -p node_modules
printf 'installed\n' >node_modules/.package-lock.json
printf '%s' "$PWD" >"$PETAL_FAKE_LOG/npm-cwd"
printf '%s\n' "$*" >"$PETAL_FAKE_LOG/npm-args"
EOF
cat >"$FAKE_BIN/npx" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ "$PWD" == */apps/desktop ]]
[[ -f node_modules/.package-lock.json ]]
[[ "$*" == "tauri build --config src-tauri/tauri.release.conf.json --target universal-apple-darwin --bundles app dmg updater" ]]
[[ "$CARGO_TARGET_DIR" == "$PETAL_EXPECTED_OUTPUT_ROOT/apps/desktop/src-tauri/target" ]]
bundle="$CARGO_TARGET_DIR/universal-apple-darwin/release/bundle/macos/Petal.app"
mkdir -p "$bundle/Contents/MacOS"
printf 'fake universal executable\n' >"$bundle/Contents/MacOS/desktop"
printf '%s' "$PWD" >"$PETAL_FAKE_LOG/npx-cwd"
printf '%s\n' "$*" >"$PETAL_FAKE_LOG/npx-args"
printf '%s' "$bundle" >"$PETAL_FAKE_LOG/bundle-path"
EOF
chmod +x "$FAKE_BIN/npm" "$FAKE_BIN/npx"
(
  cd "$TEST_ROOT/apps/desktop"
  PATH="$FAKE_BIN:$PATH" \
    PETAL_FAKE_LOG="$FAKE_LOG" \
    PETAL_EXPECTED_OUTPUT_ROOT="$TEST_ROOT" \
    "$WRAPPER" --require-clean bash -c \
      'npm ci && CARGO_TARGET_DIR="$PETAL_PROVENANCE_OUTPUT_ROOT/apps/desktop/src-tauri/target" npx tauri build --config src-tauri/tauri.release.conf.json --target universal-apple-darwin --bundles app dmg updater'
)
isolated_npm_cwd="$(cat "$FAKE_LOG/npm-cwd")"
isolated_npx_cwd="$(cat "$FAKE_LOG/npx-cwd")"
[[ "$isolated_npm_cwd" == "$isolated_npx_cwd" ]] ||
  fail "npm and npx did not run from the same isolated desktop cwd"
[[ "$isolated_npm_cwd" != "$TEST_ROOT/apps/desktop" ]] ||
  fail "production-shape smoke ran in the caller desktop cwd"
[[ ! -e "${isolated_npm_cwd%/apps/desktop}" ]] ||
  fail "isolated checkout survived wrapper cleanup"
[[ "$(cat "$FAKE_LOG/npm-args")" == "ci" ]] ||
  fail "production-shape smoke did not invoke npm ci"
[[ "$(cat "$FAKE_LOG/npx-args")" == "tauri build --config src-tauri/tauri.release.conf.json --target universal-apple-darwin --bundles app dmg updater" ]] ||
  fail "production-shape smoke used unexpected Tauri arguments"
expected_bundle="$TEST_ROOT/apps/desktop/src-tauri/target/universal-apple-darwin/release/bundle/macos/Petal.app"
actual_bundle="$(cat "$FAKE_LOG/bundle-path")"
[[ "$actual_bundle" == "$expected_bundle" ]] ||
  fail "production-shape smoke routed bundle to $actual_bundle, expected $expected_bundle"
[[ -f "$expected_bundle/Contents/MacOS/desktop" ]] ||
  fail "caller-target bundle did not survive isolated checkout cleanup"
[[ ! -e "$TEST_ROOT/apps/desktop/node_modules" ]] ||
  fail "isolated npm ci wrote dependencies into the caller checkout"

REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
grep -Fq \
  '../../scripts/run-with-source-provenance.sh --require-clean bash -c' \
  "$REPO_ROOT/.github/workflows/release.yml" ||
  fail "shipping release workflow bypasses the clean provenance wrapper"
grep -Fq \
  'CARGO_TARGET_DIR="$PETAL_PROVENANCE_OUTPUT_ROOT/apps/desktop/src-tauri/target" npx tauri build --config src-tauri/tauri.release.conf.json --target universal-apple-darwin --bundles app dmg updater' \
  "$REPO_ROOT/.github/workflows/release.yml" ||
  fail "shipping release workflow does not build isolated source into the expected output target"

echo "source provenance wrapper: 14/14 passed"

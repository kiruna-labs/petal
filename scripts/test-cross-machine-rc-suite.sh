#!/usr/bin/env bash
# Shell-only contract tests for the cross-machine harness. No SSH, signing
# identity, LiveKit credential, GUI app, or remote machine is used here.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HARNESS="$ROOT/scripts/cross-machine-rc-suite.sh"
TMP_ROOT="$(mktemp -d /tmp/petal-cross-machine-test.XXXXXX)"
trap 'rm -rf "$TMP_ROOT"' EXIT INT TERM

fail() { echo "FAIL: $*" >&2; exit 1; }
assert_contains() { grep -Fq "$2" "$1" || fail "missing: $2"; }
assert_not_contains() { ! grep -Fq "$2" "$1" || fail "unexpected: $2"; }

make_fake_tools() {
  local bin="$1"
  mkdir -p "$bin"
  cat >"$bin/uname" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "${LOCAL_ARCH:-arm64}"
EOF
  cat >"$bin/sysctl" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  *proc_translated*) printf '%s\n' "${LOCAL_TRANSLATED:-0}" ;;
  *hw.optional.arm64*) printf '%s\n' "${LOCAL_ARM_CAPABILITY:-1}" ;;
  *) exit 1 ;;
esac
EOF
  cat >"$bin/sw_vers" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "${LOCAL_VERSION:-15.0}"
EOF
  cat >"$bin/ssh" <<'EOF'
#!/usr/bin/env bash
cmd="${@: -1}"
all="$*"
case "$cmd" in
  true) exit 0 ;;
  'bash -s') cat >/dev/null; printf '%s' "${REMOTE_PROBE:-x86_64|0|0}" ;;
  *'who | grep'*) exit 0 ;;
  *'osascript -e'*) printf '1\n' ;;
esac
case "$all" in
  *sw_vers*productVersion*) printf '%s\n' "${REMOTE_VERSION:-15.0}" ;;
  *) exit 0 ;;
esac
EOF
  for command in rsync launchctl open scp; do
    cat >"$bin/$command" <<EOF
#!/usr/bin/env bash
printf '%s\n' '$command' >>"\${MUTATION_LOG:?}"
exit 99
EOF
  done
  chmod +x "$bin"/*
}

run_preflight() {
  local name="$1" expected="$2"
  local bin="$TMP_ROOT/$name/bin" log="$TMP_ROOT/$name/mutations"
  make_fake_tools "$bin"
  mkdir -p "$TMP_ROOT/$name"
  set +e
  PATH="$bin:$PATH" MUTATION_LOG="$log" PETAL_REMOTE_HOST=fake-peer \
    LOCAL_ARCH="$3" LOCAL_TRANSLATED="$4" LOCAL_ARM_CAPABILITY="$5" REMOTE_PROBE="$6" \
    "$HARNESS" --preflight-only >"$TMP_ROOT/$name/out" 2>&1
  local status=$?
  set -e
  [[ "$status" == "$expected" ]] || fail "$name expected exit $expected, got $status"
  [[ ! -e "$log" ]] || fail "$name attempted remote mutation before preflight passed"
}

# All four real physical combinations are valid preflight evidence.
run_preflight arm_to_arm 0 arm64 0 1 'arm64|0|1'
run_preflight arm_to_intel 0 arm64 0 1 'x86_64|0|0'
run_preflight intel_to_arm 0 x86_64 0 0 'arm64|0|1'
run_preflight intel_to_intel 0 x86_64 0 0 'x86_64|0|0'
# Rosetta, unknown architectures, and contradictory architecture evidence fail
# before rsync, launchctl, open, scp, or socket forwarding can be reached.
run_preflight translated_remote 1 arm64 0 1 'x86_64|1|1'
run_preflight unknown_remote 1 arm64 0 1 'mips64|0|0'
run_preflight contradictory_remote 1 arm64 0 1 'x86_64|0|1'

verify_bundle_case() {
  local name="$1" archs="$2" team="$3" expected="$4"
  local app="$TMP_ROOT/$name/Petal.app"
  mkdir -p "$app/Contents/MacOS"
  : >"$app/Contents/MacOS/desktop"
  chmod +x "$app/Contents/MacOS/desktop"
  set +e
  if (
    source "$HARNESS"
    fail() { echo "verify failure: $*" >&2; exit 1; }
    FAKE_ARCHS="$archs"
    lipo() { printf '%s\n' "$FAKE_ARCHS"; }
    shasum() { printf '%064d  %s\n' 0 "$2"; }
    codesign() {
      if [[ "$*" == *-dvv* ]]; then
        printf 'Identifier=com.petal.app\nTeamIdentifier=%s\nflags=0x10000(runtime)\n' "$team" >&2
      fi
      return 0
    }
    verify_app_bundle "$app" 0 >/dev/null
  ); then
    status=0
  else
    status=1
  fi
  set -e
  [[ "$status" == "$expected" ]] || fail "$name expected verify status $expected, got $status"
}

verify_bundle_case universal_ok 'arm64 x86_64' X83RP84J8Z 0
verify_bundle_case non_universal 'arm64' X83RP84J8Z 1
verify_bundle_case wrong_team 'arm64 x86_64' WRONGTEAM 1

verify_remote_case() {
  local name="$1" remote_archs="$2" remote_team="$3" remote_hash="$4" remote_version="$5" signature_ok="$6" runtime_ok="$7" stapler_ok="$8" gatekeeper_ok="$9" expected="${10}"
  local bin="$TMP_ROOT/$name/bin" app_dir="$TMP_ROOT/$name/remote"
  local manifest_sentinel="$TMP_ROOT/$name/manifest-sentinel"
  local launch_sentinel="$TMP_ROOT/$name/launch-sentinel"
  local open_sentinel="$TMP_ROOT/$name/open-sentinel"
  local osascript_sentinel="$TMP_ROOT/$name/osascript-sentinel"
  mkdir -p "$bin" "$app_dir/Petal.app/Contents/MacOS"
  : >"$app_dir/Petal.app/Contents/MacOS/desktop"
  chmod +x "$app_dir/Petal.app/Contents/MacOS/desktop"
  cat >"$bin/ssh" <<'EOF'
#!/usr/bin/env bash
for arg in "$@"; do command="$arg"; done
exec /bin/bash -c "$command"
EOF
  cat >"$bin/lipo" <<EOF
#!/usr/bin/env bash
printf '%s\\n' '$remote_archs'
EOF
cat >"$bin/codesign" <<EOF
#!/usr/bin/env bash
if [[ "\$*" != *-dvv* && '$signature_ok' != 1 ]]; then exit 1; fi
if [[ "\$*" == *-dvv* ]]; then
  if [[ '$runtime_ok' == 1 ]]; then flags='flags=0x10000(runtime)'; else flags='flags=0x0'; fi
  printf 'Identifier=com.petal.app\\nTeamIdentifier=$remote_team\\n%s\\n' "\$flags" >&2
fi
exit 0
EOF
  cat >"$bin/shasum" <<EOF
#!/usr/bin/env bash
printf '%s  %s\\n' '$remote_hash' "\$3"
EOF
  cat >"$bin/PlistBuddy" <<EOF
#!/usr/bin/env bash
printf '%s\\n' '$remote_version'
EOF
  cat >"$bin/xcrun" <<EOF
#!/usr/bin/env bash
[[ '$stapler_ok' == 1 ]]
EOF
  cat >"$bin/spctl" <<EOF
#!/usr/bin/env bash
[[ '$gatekeeper_ok' == 1 ]]
EOF
  cat >"$bin/uname" <<'EOF'
#!/usr/bin/env bash
printf 'x86_64\n'
EOF
  cat >"$bin/scp" <<EOF
#!/usr/bin/env bash
touch '$manifest_sentinel'
EOF
  cat >"$bin/launchctl" <<EOF
#!/usr/bin/env bash
touch '$launch_sentinel'
EOF
  cat >"$bin/open" <<EOF
#!/usr/bin/env bash
touch '$open_sentinel'
EOF
  cat >"$bin/osascript" <<EOF
#!/usr/bin/env bash
touch '$osascript_sentinel'
EOF
  chmod +x "$bin"/*
  set +e
  if (
    PATH="$bin:$PATH"
    source "$HARNESS"
    fail() { exit 1; }
    REMOTE_HOST=fake-peer
    REMOTE_APP_DIR="$app_dir"
    REMOTE_MACOS_MAJOR=15
    SOURCE_COMMIT=0123456789012345678901234567890123456789
    EXPECTED_BUNDLE_VERSION=0.7.12
    QA_PREBUILT_MODE=1
    PLIST_BUDDY="$bin/PlistBuddy"
    # Minimal extracted main-path boundary: successful peer verification must
    # enter the real remote launcher; any verifier failure exits before it.
    verify_remote_app "$(printf '%064d' 0)"
    launch_remote_app 127.0.0.1
  ); then status=0; else status=1; fi
  set -e
  [[ "$status" == "$expected" ]] || fail "$name expected remote verify status $expected, got $status"
  if [[ "$expected" == 0 ]]; then
    [[ -e "$manifest_sentinel" ]] || fail "$name did not retrieve only the allowlisted manifest"
    [[ -e "$launch_sentinel" && -e "$open_sentinel" ]] || fail "$name valid peer did not reach the real remote launch path"
    # #846: launch_remote_app no longer quits "Petal" by NAME via osascript --
    # LaunchServices name resolution can pick the wrong Petal.app (e.g. the
    # user's installed one) instead of this QA bundle. It must be gone, not
    # just skipped in this stub.
    [[ ! -e "$osascript_sentinel" ]] || fail "$name must not AppleEvent-quit \"Petal\" by name (#846: unsafe LaunchServices resolution)"
  else
    # `verify_remote_app` is called by deploy_app before launch_remote_app in
    # main. Each rejected peer must therefore leave every post-verifier action
    # untouched: no manifest collection, launch environment, app open, or
    # launch-time AppleEvent.
    for sentinel in "$manifest_sentinel" "$launch_sentinel" "$open_sentinel" "$osascript_sentinel"; do
      [[ ! -e "$sentinel" ]] || fail "$name reached a post-verifier action"
    done
  fi
}

verify_remote_case remote_ok 'arm64 x86_64' X83RP84J8Z "$(printf '%064d' 0)" 0.7.12 1 1 1 1 0
verify_remote_case remote_non_universal arm64 X83RP84J8Z "$(printf '%064d' 0)" 0.7.12 1 1 1 1 1
verify_remote_case remote_signature_failure 'arm64 x86_64' X83RP84J8Z "$(printf '%064d' 0)" 0.7.12 0 1 1 1 1
verify_remote_case remote_runtime_failure 'arm64 x86_64' X83RP84J8Z "$(printf '%064d' 0)" 0.7.12 1 0 1 1 1
verify_remote_case remote_wrong_team 'arm64 x86_64' WRONGTEAM "$(printf '%064d' 0)" 0.7.12 1 1 1 1 1
verify_remote_case remote_hash_mismatch 'arm64 x86_64' X83RP84J8Z "$(printf '%064d' 1)" 0.7.12 1 1 1 1 1
verify_remote_case remote_wrong_version 'arm64 x86_64' X83RP84J8Z "$(printf '%064d' 0)" 9.9.9 1 1 1 1 1
verify_remote_case remote_stapler_failure 'arm64 x86_64' X83RP84J8Z "$(printf '%064d' 0)" 0.7.12 1 1 0 1 1
verify_remote_case remote_gatekeeper_failure 'arm64 x86_64' X83RP84J8Z "$(printf '%064d' 0)" 0.7.12 1 1 1 0 1

# The reducer writes only terminal counts; all raw report content is untrusted.
RAW="$TMP_ROOT/raw.json"
SUMMARY="$TMP_ROOT/summary.json"
cat >"$RAW" <<'EOF'
{"summary":{"total":1,"pass":0,"fail":1,"skip":0},"results":[{"status":"fail","detail":"SECRET=do-not-export host=10.0.0.7 title=/Users/a/typed text x=12 raw-os-error"}],"terminalDeliveries":[{"inputId":"safe_input_1","inputSeq":1,"outcome":"replayFailed","deliveryRoute":"replay","failureCode":"injectionTimeout","windowId":7,"receivedAt":1234},{"inputId":"safe_input_1","inputSeq":1,"outcome":"replayFailed","deliveryRoute":"replay","failureCode":"injectionTimeout","windowId":7,"receivedAt":1235}],"terminalRecovery":{"duplicateReplayObserved":true,"sideEffectCount":1,"terminalDeliveries":[{"inputId":"safe_input_1","inputSeq":1,"outcome":"replayFailed","deliveryRoute":"replay","failureCode":"injectionTimeout","windowId":7,"receivedAt":1234},{"inputId":"safe_input_1","inputSeq":1,"outcome":"replayFailed","deliveryRoute":"replay","failureCode":"injectionTimeout","windowId":7,"receivedAt":1235}]}}
EOF
(
  source "$HARNESS"
  reduce_suite_results "$RAW" "$SUMMARY" 1
)
assert_contains "$SUMMARY" '"inputRoute":"packaged-default"'
assert_contains "$SUMMARY" '"suiteExit":1'
assert_contains "$SUMMARY" '"inputId":"safe_input_1"'
assert_contains "$SUMMARY" '"terminalRecovery"'
assert_contains "$SUMMARY" '"duplicateReplayObserved":true'
assert_contains "$SUMMARY" '"sideEffectCount":1'
for sentinel in SECRET 10.0.0.7 /Users/a 'typed text' raw-os-error; do
  assert_not_contains "$SUMMARY" "$sentinel"
done

# A contradictory child exit cannot turn a classified case failure into pass.
CONTRADICTORY="$TMP_ROOT/contradictory-summary.json"
(
  source "$HARNESS"
  reduce_suite_results "$RAW" "$CONTRADICTORY" 0
)
assert_contains "$CONTRADICTORY" '"suiteExit":1'

# Summary counts cannot contradict the per-case status ledger, even when the
# child exits zero and the recovery proof itself is valid.
STATUS_CONTRADICTION="$TMP_ROOT/status-contradiction.json"
STATUS_CONTRADICTION_SUMMARY="$TMP_ROOT/status-contradiction-summary.json"
cat >"$STATUS_CONTRADICTION" <<'EOF'
{"summary":{"total":1,"pass":1,"fail":0,"skip":0},"results":[{"status":"fail"}],"terminalDeliveries":[{"inputId":"a","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":1},{"inputId":"a","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":2}],"terminalRecovery":{"duplicateReplayObserved":true,"sideEffectCount":1,"terminalDeliveries":[{"inputId":"a","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":1},{"inputId":"a","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":2}]}}
EOF
(
  source "$HARNESS"
  reduce_suite_results "$STATUS_CONTRADICTION" "$STATUS_CONTRADICTION_SUMMARY" 0
)
assert_contains "$STATUS_CONTRADICTION_SUMMARY" '"suiteExit":1'

# A valid-looking recovery object must be backed by the same two records in
# the top-level terminal ledger.
MISSING_TERMINAL_PAIR="$TMP_ROOT/missing-terminal-pair.json"
MISSING_TERMINAL_PAIR_SUMMARY="$TMP_ROOT/missing-terminal-pair-summary.json"
cat >"$MISSING_TERMINAL_PAIR" <<'EOF'
{"summary":{"total":1,"pass":1,"fail":0,"skip":0},"results":[{"status":"pass"}],"terminalDeliveries":[],"terminalRecovery":{"duplicateReplayObserved":true,"sideEffectCount":1,"terminalDeliveries":[{"inputId":"a","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":1},{"inputId":"a","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":2}]}}
EOF
(
  source "$HARNESS"
  reduce_suite_results "$MISSING_TERMINAL_PAIR" "$MISSING_TERMINAL_PAIR_SUMMARY" 0
)
assert_contains "$MISSING_TERMINAL_PAIR_SUMMARY" '"suiteExit":1'

# Matching legacy terminal records with both optional disposition fields
# absent remain valid and are not expanded in the privacy-safe artifact.
LEGACY="$TMP_ROOT/legacy.json"
LEGACY_SUMMARY="$TMP_ROOT/legacy-summary.json"
cat >"$LEGACY" <<'EOF'
{"summary":{"total":1,"pass":1,"fail":0,"skip":0},"results":[{"status":"pass"}],"terminalDeliveries":[{"inputId":"legacy_1","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":1234},{"inputId":"legacy_1","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":1235}],"terminalRecovery":{"duplicateReplayObserved":true,"sideEffectCount":1,"terminalDeliveries":[{"inputId":"legacy_1","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":1234},{"inputId":"legacy_1","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":1235}]}}
EOF
(
  source "$HARNESS"
  reduce_suite_results "$LEGACY" "$LEGACY_SUMMARY" 0
)
assert_contains "$LEGACY_SUMMARY" '"suiteExit":0'
assert_contains "$LEGACY_SUMMARY" '"duplicateReplayObserved":true'
assert_not_contains "$LEGACY_SUMMARY" deliveryRoute
assert_not_contains "$LEGACY_SUMMARY" failureCode

# A process-level pass cannot hide a failed exactly-once recovery proof.
for fixture in wrong-correlation conflicting-disposition missing-second duplicate-side-effect; do
  FIXTURE_RAW="$TMP_ROOT/$fixture.json"
  FIXTURE_SUMMARY="$TMP_ROOT/$fixture-summary.json"
  case "$fixture" in
    wrong-correlation)
      RECOVERY='{"duplicateReplayObserved":true,"sideEffectCount":1,"terminalDeliveries":[{"inputId":"a","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":1},{"inputId":"b","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":2}]}'
      ;;
    conflicting-disposition)
      RECOVERY='{"duplicateReplayObserved":true,"sideEffectCount":1,"terminalDeliveries":[{"inputId":"a","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":1},{"inputId":"a","inputSeq":1,"outcome":"replayFailed","failureCode":"replayFailed","windowId":7,"receivedAt":2}]}'
      ;;
    missing-second)
      RECOVERY='{"duplicateReplayObserved":true,"sideEffectCount":1,"terminalDeliveries":[{"inputId":"a","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":1}]}'
      ;;
    duplicate-side-effect)
      RECOVERY='{"duplicateReplayObserved":true,"sideEffectCount":2,"terminalDeliveries":[{"inputId":"a","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":1},{"inputId":"a","inputSeq":1,"outcome":"applied","windowId":7,"receivedAt":2}]}'
      ;;
  esac
  printf '{"summary":{"total":1,"pass":1,"fail":0,"skip":0},"results":[{"status":"pass"}],"terminalDeliveries":[],"terminalRecovery":%s}\n' "$RECOVERY" >"$FIXTURE_RAW"
  (
    source "$HARNESS"
    reduce_suite_results "$FIXTURE_RAW" "$FIXTURE_SUMMARY" 0
  )
  assert_contains "$FIXTURE_SUMMARY" '"suiteExit":1'
done

# Unknown recovery fields are rejected rather than copied into retained
# evidence.
EXTRA_RECOVERY="$TMP_ROOT/extra-recovery.json"
cat >"$EXTRA_RECOVERY" <<'EOF'
{"summary":{"total":1,"pass":1,"fail":0,"skip":0},"results":[{"status":"pass"}],"terminalDeliveries":[],"terminalRecovery":{"duplicateReplayObserved":false,"sideEffectCount":0,"terminalDeliveries":[],"SECRET":"nope"}}
EOF
if (source "$HARNESS"; reduce_suite_results "$EXTRA_RECOVERY" "$TMP_ROOT/extra-recovery-summary.json" 0); then
  fail 'unknown terminal recovery field unexpectedly reduced'
fi

# Malformed raw data is reduced to a bounded runner failure; no raw sentinel is
# carried into the safe artifact and callers can remove the raw input on every
# exit via the harness cleanup trap.
MALFORMED="$TMP_ROOT/malformed.json"
printf 'SECRET malformed' >"$MALFORMED"
if (source "$HARNESS"; reduce_suite_results "$MALFORMED" "$TMP_ROOT/should-not-exist.json" 1); then
  fail 'malformed raw result unexpectedly reduced'
fi
(
  source "$HARNESS"
  write_classified_runner_failure "$TMP_ROOT/classified.json" malformed-results
)

# The reducer's SUMMARY allowlist must equal the PRODUCER's canonical key set,
# and a producer-shaped summary must actually reduce. The fixtures above are
# hand-written and omit `tokenlessDrops`, which is why they stayed green while
# #580 added that key to remote-control-scenario.mjs's SUMMARY and every real
# cross-machine run reduced to `malformed-results`. Deriving this fixture from
# SUITE_SUMMARY_KEYS makes that drift impossible to repeat silently.
PRODUCER_SHAPED="$TMP_ROOT/producer-shaped.json"
node --input-type=module - "$HARNESS" "$PRODUCER_SHAPED" <<'NODE'
import fs from 'node:fs';
import path from 'node:path';
const [harnessPath, fixturePath] = process.argv.slice(2);
const repoRoot = path.resolve(path.dirname(harnessPath), '..');
const { SUITE_SUMMARY_KEYS } = await import(
  path.join(repoRoot, 'apps/desktop/scripts/remote-control-exit.mjs')
);
const harness = fs.readFileSync(harnessPath, 'utf8');
const match = harness.match(/const allowedSummary = new Set\(\[([^\]]*)\]\)/);
if (!match) {
  console.error('FAIL: allowedSummary literal not found in reduce_suite_results');
  process.exit(1);
}
const allowed = [...match[1].matchAll(/'([^']+)'/g)].map((entry) => entry[1]);
const missing = SUITE_SUMMARY_KEYS.filter((key) => !allowed.includes(key));
const extra = allowed.filter((key) => !SUITE_SUMMARY_KEYS.includes(key));
if (missing.length > 0 || extra.length > 0) {
  console.error(
    `FAIL: reducer allowlist drifted from SUITE_SUMMARY_KEYS -- missing=[${missing}] extra=[${extra}]`
  );
  process.exit(1);
}
const values = {
  total: 1,
  pass: 1,
  fail: 0,
  skip: 0,
  recoveries: 0,
  tokenlessDrops: 0,
  mode: 'numbered',
  shareReadiness: 'live-tile',
  targetObservationLatency: { budgetMs: 500, samples: 1, maxMs: 12, p95Ms: 12 },
};
const summary = Object.fromEntries(
  SUITE_SUMMARY_KEYS.map((key) => {
    if (!(key in values)) {
      console.error(`FAIL: no fixture value for producer summary key '${key}'`);
      process.exit(1);
    }
    return [key, values[key]];
  })
);
const delivery = (receivedAt) => ({ inputId: 'a', inputSeq: 1, outcome: 'applied', windowId: 7, receivedAt });
const pair = [delivery(1), delivery(2)];
fs.writeFileSync(
  fixturePath,
  `${JSON.stringify({
    summary,
    results: [{ status: 'pass' }],
    terminalDeliveries: pair,
    terminalRecovery: { duplicateReplayObserved: true, sideEffectCount: 1, terminalDeliveries: pair },
  })}\n`
);
NODE
(
  source "$HARNESS"
  reduce_suite_results "$PRODUCER_SHAPED" "$TMP_ROOT/producer-shaped-summary.json" 0
)
assert_contains "$TMP_ROOT/producer-shaped-summary.json" '"suiteExit":0'
assert_contains "$TMP_ROOT/producer-shaped-summary.json" '"tokenlessDrops":0'
assert_contains "$TMP_ROOT/producer-shaped-summary.json" '"shareReadiness":"live-tile"'

# 6c: a relaxed --input-only run must be distinguishable in the privacy-safe
# artifact. It must never be readable as the full gate.
RELAXED="$TMP_ROOT/relaxed.json"
sed 's/"shareReadiness":"live-tile"/"shareReadiness":"target-present"/;s/"mode":"numbered"/"mode":"input-only"/' "$PRODUCER_SHAPED" >"$RELAXED"
(
  source "$HARNESS"
  reduce_suite_results "$RELAXED" "$TMP_ROOT/relaxed-summary.json" 0
)
assert_contains "$TMP_ROOT/relaxed-summary.json" '"mode":"input-only"'
assert_contains "$TMP_ROOT/relaxed-summary.json" '"shareReadiness":"target-present"'

# A tokenless drop means the packet never reached any injection route (#580).
# A zero child exit must not launder it into a pass.
TOKENLESS_DROPPED="$TMP_ROOT/tokenless-dropped.json"
sed 's/"tokenlessDrops":0/"tokenlessDrops":2/' "$PRODUCER_SHAPED" >"$TOKENLESS_DROPPED"
assert_contains "$TOKENLESS_DROPPED" '"tokenlessDrops":2'
(
  source "$HARNESS"
  reduce_suite_results "$TOKENLESS_DROPPED" "$TMP_ROOT/tokenless-dropped-summary.json" 0
)
assert_contains "$TMP_ROOT/tokenless-dropped-summary.json" '"suiteExit":1'
assert_contains "$TMP_ROOT/tokenless-dropped-summary.json" '"tokenlessDrops":2'

assert_contains "$TMP_ROOT/classified.json" '"runnerFailure":"malformed-results"'
assert_not_contains "$TMP_ROOT/classified.json" SECRET

# Pin the safety-critical launch/default-route and result-status flow in the
# executable shell source; the fake preflight above proves no mutation path is
# entered for rejected peers.
assert_contains "$HARNESS" '"$REPO/scripts/run-with-source-provenance.sh" --require-clean env'
assert_contains "$HARNESS" 'CARGO_TARGET_DIR="$PETAL_PROVENANCE_OUTPUT_ROOT/apps/desktop/src-tauri/target"'
assert_contains "$HARNESS" 'npx tauri build --target universal-apple-darwin'
assert_contains "$HARNESS" 'launchctl unsetenv PETAL_REMOTE_CONTROL_DIRECT_SCROLL'
assert_contains "$HARNESS" 'env -u PETAL_REMOTE_CONTROL_DIRECT_SCROLL'
assert_contains "$HARNESS" '[[ "$remote_hash" == "$expected_hash" ]]'
assert_contains "$HARNESS" '!resultCountsMatchSummary'
assert_contains "$HARNESS" '|| !recoverySucceeded'
assert_contains "$HARNESS" 'RESULTS_JSON="$(mktemp "$RAW_DIR/results.XXXXXX")"'
assert_contains "$HARNESS" 'chmod 600 "$RESULTS_JSON"'
assert_contains "$HARNESS" 'rm -f "$RAW_SUITE_OUTPUT" "$RESULTS_JSON"'
assert_contains "$HARNESS" 'EVIDENCE_ROOT="${PETAL_CROSS_MACHINE_EVIDENCE_DIR:-${TMPDIR:-/tmp}/petal-cross-machine-evidence}"'
assert_contains "$HARNESS" 'EVIDENCE_DIR="$(mktemp -d "$EVIDENCE_ROOT/run.XXXXXX")"'
assert_not_contains "$HARNESS" 'rm -rf "$EVIDENCE_DIR"'
assert_contains "$HARNESS" 'privacy-safe evidence retained'

# Cleanup has two deterministic outcomes: both a nominal and a failing run
# retain allowlisted evidence, while raw inputs disappear unconditionally.
for cleanup_status in nominal failing; do
  SAFE="$TMP_ROOT/$cleanup_status-safe"
  RAW_PRIVATE="$TMP_ROOT/$cleanup_status-raw"
  mkdir -p "$SAFE" "$RAW_PRIVATE"
  printf 'safe' >"$SAFE/cross-machine-summary.json"
  printf 'SECRET' >"$RAW_PRIVATE/results"
  printf 'SECRET' >"$RAW_PRIVATE/stdout"
  (
    source "$HARNESS"
    EVIDENCE_DIR="$SAFE"
    RAW_DIR="$RAW_PRIVATE"
    RESULTS_JSON="$RAW_PRIVATE/results"
    RAW_SUITE_OUTPUT="$RAW_PRIVATE/stdout"
    REMOTE_CLEANUP_ENABLED=0
    if [[ "$cleanup_status" == failing ]]; then false; fi
    cleanup
  ) || true
  [[ -f "$SAFE/cross-machine-summary.json" ]] || fail "$cleanup_status cleanup removed safe evidence"
  [[ ! -e "$RAW_PRIVATE" ]] || fail "$cleanup_status cleanup retained raw evidence"
done
assert_contains "$HARNESS" 'Remote app directory must be an absolute safe path.'
assert_contains "$HARNESS" 'remote_path_quote "$remote_manifest"'
assert_not_contains "$HARNESS" 'privacy-safe summary: $SANITIZED_RESULTS_JSON'

# Deterministic producer boundary fixture: model the scenario's destructuring
# selector and prove it emits exactly newly observed allowlisted fields.
node <<'NODE'
const source = {
  inputId: 'legacy_1', inputSeq: 1, outcome: 'applied', windowId: 7, receivedAt: 123,
  text: 'never export', x: .2, y: .3, peerIdentity: 'never export', arbitrary: { nope: true }
};
const select = ({ inputId, inputSeq, outcome, deliveryRoute, failureCode, windowId, receivedAt }) => ({ inputId, inputSeq, outcome, ...(deliveryRoute === undefined ? {} : { deliveryRoute }), ...(failureCode === undefined ? {} : { failureCode }), windowId, receivedAt });
const legacy = select(source);
const expectedLegacy = ['inputId', 'inputSeq', 'outcome', 'receivedAt', 'windowId'];
if (JSON.stringify(Object.keys(legacy).sort()) !== JSON.stringify(expectedLegacy)) process.exit(1);
const v2 = select({ ...source, deliveryRoute: 'replay', failureCode: 'injectionTimeout' });
if (v2.deliveryRoute !== 'replay' || v2.failureCode !== 'injectionTimeout') process.exit(1);
NODE
assert_contains "$ROOT/apps/desktop/scripts/remote-control-scenario.mjs" 'terminalDeliveries.push(...await collectTerminalDeliveries(ctx));'
assert_contains "$ROOT/apps/desktop/scripts/remote-control-scenario.mjs" 'api.resetMetrics(); api.request(target); return true;'
assert_contains "$ROOT/apps/desktop/scripts/remote-control-scenario.mjs" 'api.replayLastCompletedClick();'
assert_contains "$ROOT/apps/desktop/scripts/remote-control-scenario.mjs" 'duplicateReplayObserved'
assert_contains "$ROOT/apps/desktop/scripts/remote-control-scenario.mjs" 'sideEffectCount'
node - "$HARNESS" <<'NODE'
const fs = require('node:fs');
const source = fs.readFileSync(process.argv[2], 'utf8');
const deploy = source.slice(source.indexOf('deploy_app() {'), source.indexOf('verify_remote_app() {'));
const main = source.slice(source.indexOf('main() {'));
if (!deploy.includes('verify_remote_app "$LOCAL_BINARY_HASH"')) process.exit(1);
for (const forbidden of ['launchctl setenv', 'open -n', 'osascript', 'launch_remote_app', 'start_socket_forward']) {
  if (deploy.includes(forbidden)) process.exit(1);
}
if (!(main.indexOf('deploy_app') < main.indexOf('launch_remote_app') && main.indexOf('launch_remote_app') < main.indexOf('start_socket_forward'))) process.exit(1);
NODE

echo 'test result: cross-machine remote-control harness fake-shim tests passed'

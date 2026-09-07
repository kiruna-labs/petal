#!/usr/bin/env bash
#
# Local CI — the PRIMARY verification gate while GitHub Actions CI is
# manual-only for cost (macOS runners bill at 10x; see
# .github/workflows/ci.yml's header). Run this from anywhere before pushing;
# "green here" is meant to equal "green in the workflow".
#
# Mirrors the workflow's checks: frontend (svelte-check + static build),
# backend (tsc + tests), Rust (build + lib tests), and the #99 portability
# guard. Fast path uses the repo's committed dev toolchain config
# (.cargo/config.toml) so it reuses the warm target/ cache; the deeper
# full-Xcode / no-CLT-rpath portability proof is verified at release-build
# time (the notarized universal build) and by the macos-15 workflow when it's
# run manually.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ ! "${PETAL_SOURCE_PROVENANCE_WRAPPED:-}" =~ ^[0-9a-f]{64}$ ]] ||
   [[ "${PETAL_SOURCE_PROVENANCE_WRAPPED:-}" != "${PETAL_OFFICIAL_SOURCE_STATE:-}" ]]; then
  cd "$ROOT"
  exec "$ROOT/scripts/run-with-source-provenance.sh" "$ROOT/scripts/ci-local.sh" "$@"
fi

cd "$ROOT"

step() { printf '\n\033[1;36m==> %s\033[0m\n' "$1"; }

step "Release: version lockstep gate (9 fields, incl. Cargo.lock's desktop entry -- #671)"
# Self-check mode (no tag here): every version field must agree with
# tauri.conf.json's own version. release.yml runs the SAME script against
# the tag being released; this is the local equivalent so drift is caught
# before a release, not during one.
node "$ROOT/scripts/version-lockstep.mjs"

step "Release: bump-version.mjs + lockstep-gate unit tests (#671)"
node "$ROOT/scripts/test-bump-version.mjs"

step "Release: publish-blob.mjs pure-logic unit tests (#671)"
node "$ROOT/scripts/test-publish-blob-lib.mjs"

step "Source provenance wrapper + signed cross-machine integration"
"$ROOT/scripts/test-run-with-source-provenance.sh"
"$ROOT/scripts/test-cross-machine-rc-suite.sh"

step "Harness: owned-process cleanup contract (plan Item 7)"
"$ROOT/scripts/test-owned-process-cleanup.sh"

step "Harness: rc-live-suite foreign-instance guard (both directions)"
"$ROOT/scripts/test-rc-suite-instance-guard.sh"

step "Harness: capture-preflight contract (plan 6d step 2)"
"$ROOT/scripts/test-capture-preflight.sh"

# Self-installing, idempotent: makes scripts/git-hooks/pre-push (the local
# replacement for the disabled .github/workflows/rust-gate.yml -- GitHub
# Actions billing on macOS runners made that workflow unreliable) apply to
# every push from this checkout automatically, the first time anyone runs
# this script, with no separate "remember to install the hook" step. Repo
# config (not global), so it's shared by every worktree of this repo on this
# machine via the common .git dir.
if [ "$(git config --get core.hooksPath || true)" != "scripts/git-hooks" ]; then
  git config core.hooksPath scripts/git-hooks
  step "Installed scripts/git-hooks/pre-push (core.hooksPath) -- Rust gate now runs on push automatically"
fi

step "Frontend: svelte-check + unit tests + static build (apps/desktop)"
# build BEFORE test: secondaryWindowChrome.test.ts needs build/region-window.html on a clean checkout (#916)
( cd apps/desktop && npm ci && npm run check && npm run build && npm test )

step "Backend: tsc --noEmit + tests (backend)"
( cd backend && npm ci && npm run typecheck && npm test )

step "Rust: default-feature + cockpit-privileged build and lib tests (apps/desktop/src-tauri)"
(
  cd apps/desktop/src-tauri

  if ! cargo build --locked; then
    printf '\n\033[1;31mRUST GATE BLOCKED: cargo build did not complete.\033[0m\n' >&2
    printf 'The local CI result is not mergeable; no Rust verification was established.\n' >&2
    exit 1
  fi

  if ! cargo build --locked --features cockpit-privileged; then
    printf '\n\033[1;31mRUST GATE BLOCKED: cargo build with cockpit-privileged did not complete.\033[0m\n' >&2
    printf 'The local CI result is not mergeable; no privileged Rust verification was established.\n' >&2
    exit 1
  fi

  # The probe harnesses under examples/ link against the lib but are built by
  # NEITHER `cargo build` nor `cargo test --lib`, so a change to a public enum
  # or signature can break every probe while this gate stays green. That is not
  # hypothetical twice over: `compositor_probe` silently stopped compiling for
  # weeks after `enqueue_frame` was replaced by `prepare_sample`/
  # `enqueue_prepared` (#594), and separately adding `ServerEvent::ToolCall`
  # broke `token_probe` -- the exact harness needed to verify the feature that
  # added it (#658's follow-up). Neither was noticed by any gate. Build ALL
  # examples rather than naming favourites -- COURSE_CORRECTION.md §4b points
  # every agent at examples/ as the primary fast verification loop, and an
  # unnamed probe is an unwatched one, which is the failure this closes.
  # Compile-only: the probes need a real SFU, a window server, or Screen
  # Recording to actually RUN (docs/TESTING.md's display-requiring tier), so
  # this only stops them rotting. Keep this in lockstep with
  # .github/workflows/ci.yml's and rust-gate.yml's "cargo build (examples)"
  # steps (#635) -- a widened/narrowed example set here with no matching CI
  # change is exactly the blind spot this closes.
  if ! cargo build --locked --examples; then
    printf '\n\033[1;31mRUST GATE BLOCKED: cargo build --examples did not complete.\033[0m\n' >&2
    printf 'A probe harness under examples/ no longer compiles against the lib.\n' >&2
    exit 1
  fi

  # Same anti-rot rule for the ObjC/C probes under scripts/probes/ (the
  # window-classification live gate, #747 / plan §7.2). They need a display +
  # an Accessibility-granted launch identity to RUN
  # (scripts/verify-window-classification.sh; docs/TESTING.md display tier),
  # so this only keeps them compiling.
  if command -v clang >/dev/null 2>&1; then
    for probe in "$ROOT"/scripts/probes/*.m; do
      [ -e "$probe" ] || continue
      if ! clang -fobjc-arc -fsyntax-only "$probe"; then
        printf '\n\033[1;31mRUST GATE BLOCKED: probe %s no longer compiles.\033[0m\n' "$probe" >&2
        exit 1
      fi
    done
    for probe in "$ROOT"/scripts/probes/*.c; do
      [ -e "$probe" ] || continue
      if ! clang -fsyntax-only "$probe"; then
        printf '\n\033[1;31mRUST GATE BLOCKED: probe %s no longer compiles.\033[0m\n' "$probe" >&2
        exit 1
      fi
    done
  fi

  # cargo test's harness binary needs CLT's Swift concurrency dylib at launch
  # (documented env quirk on this machine; unrelated to the code under test).
  run_rust_lib_tests() {
    local configuration="$1"
    shift
    local test_output test_total
    test_output="$(mktemp)"

    if ! DYLD_FALLBACK_LIBRARY_PATH=/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx \
        cargo test --lib --locked "$@" >"$test_output" 2>&1; then
      cat "$test_output"
      rm -f "$test_output"
      printf '\n\033[1;31mRUST GATE BLOCKED: cargo test --lib (%s) did not complete.\033[0m\n' "$configuration" >&2
      printf 'Do not treat preceding compilation output as a passing Rust test result.\n' >&2
      printf 'The local CI result is not mergeable until the full Rust test stage completes.\n' >&2
      exit 1
    fi

    cat "$test_output"
    test_total="$(awk '$1 == "test" && $2 == "result:" && $3 == "ok." && $5 == "passed;" { total = $4 } END { print total }' "$test_output")"
    rm -f "$test_output"
    if [ -z "$test_total" ]; then
      printf '\n\033[1;31mRUST GATE BLOCKED: cargo test --lib (%s) did not report a test total.\033[0m\n' "$configuration" >&2
      printf 'Do not treat preceding compilation output as a passing Rust test result.\n' >&2
      exit 1
    fi
    printf '\033[1;32mRust %s lib-test total: %s passed.\033[0m\n' "$configuration" "$test_total"
  }

  run_rust_lib_tests "default-feature"
  run_rust_lib_tests "cockpit-privileged" --features cockpit-privileged

  # #705: the vendored tauri-nspanel crate (vendor/tauri-nspanel) carries its
  # own regression tests for the activation-policy-restore RAII fix. It is a
  # separate Cargo package, not part of the `desktop` lib target, so neither
  # `cargo build --locked` nor `run_rust_lib_tests` above ever touches it --
  # without this, those tests pass locally but execute nowhere in the gate.
  step "vendor/tauri-nspanel: lib tests (#705)"
  if ! DYLD_FALLBACK_LIBRARY_PATH=/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx \
      cargo test -p tauri-nspanel --lib --locked; then
    printf '\n\033[1;31mRUST GATE BLOCKED: cargo test -p tauri-nspanel --lib did not complete.\033[0m\n' >&2
    exit 1
  fi

  printf '\033[1;32mRust verification complete: default-feature and cockpit-privileged builds and lib tests passed.\033[0m\n'
)

# Plugin system (plugins/README.md): the SDK + first-party plugin workspace.
# The host runtime in shared/plugin-host is covered by web-harness's tests
# (tests/plugin*.test.ts, below); this step checks the workspace itself:
# SDK typecheck and the bundle packer's own tests.
if [ -f plugins/package.json ]; then
  step "plugins: workspace install + SDK typecheck + packer tests"
  ( cd plugins && npm ci && npx tsc --noEmit -p sdk/tsconfig.json && npm test )
fi

if [ -f web-harness/package.json ]; then
  step "web-harness: build + tests"
  ( cd web-harness && npm ci && npm run build && npm test )

  # #662: the check above builds INSIDE the full monorepo checkout, where
  # web-harness/shared's symlink to ../shared resolves fine either way -- it
  # cannot catch "does the ISOLATED deploy build still work," which is
  # exactly what broke silently for days (Vercel uploads only the deploy
  # invocation's cwd; a plain `cd web-harness && vercel --prod` never
  # uploads shared/ at all). This reuses the real deploy script's staging
  # logic in --build-only mode -- no Vercel CLI/auth/network needed -- so a
  # future shared/ change that breaks the isolated build fails HERE, not as
  # a silent stale production deploy.
  step "web-harness: isolated-deploy build simulation (#662)"
  scripts/deploy-web-harness.sh --build-only

  # CLAUDE.md "Never show a black frame" (#627). Deliberately a PIXEL gate, not
  # a unit test: "held the last frame" and "went black quietly" emit identical
  # events, so only sampled rendered pixels can tell them apart. Runs the gap in
  # BOTH directions in one pass -- reproduces the black frame with the shipped
  # CSS, then shows the fix holding the frame -- so a pass can never come from
  # the forced gap simply failing to happen.
  step "web-harness: no-black-frame pixel gate (#627)"
  node "$ROOT/scripts/verify-no-black-frame.mjs"
fi

step "Remote-control local-loopback harness: check-only"
( cd apps/desktop && npm run autotest:remote-control-loopback:check )

if [ -f site/package.json ]; then
  step "Docs site (site/): AUTO-pull sync + build + PII gate + link validation"
  (
    cd site
    npm ci
    npm run sync:auto

    # Source-drift gate (issue #437): fail if docs/SELF_HOSTING.md,
    # backend/README.md, or web-harness/README.md changed on disk without a
    # human re-pinning the manifest hash (forces a deliberate look at the
    # change, including for PII, before it publishes) — see
    # scripts/sync-auto-content.mjs's header comment.
    if ! npm run check:drift; then
      printf '\n\033[1;31mDOCS SITE GATE BLOCKED: an AUTO-pulled source doc changed since it was last pinned.\033[0m\n' >&2
      printf 'Review the change, then run `npm run sync:auto -- --update-manifest` in site/.\n' >&2
      exit 1
    fi

    # `npm run build` also runs the sync (prebuild hook) and, via the
    # starlight-links-validator plugin wired into astro.config.mjs, fails the
    # build on any broken internal link — no separate command needed.
    if ! npm run build; then
      printf '\n\033[1;31mDOCS SITE GATE BLOCKED: astro build (incl. link validation) did not complete.\033[0m\n' >&2
      exit 1
    fi

    # Hard, non-negotiable rule: this site must never publish personal or
    # individually-identifying operational detail. Scan the BUILT output,
    # not just the source pages, so nothing slips in via a template/plugin.
    if ! npm run check:pii; then
      printf '\n\033[1;31mDOCS SITE GATE BLOCKED: PII/secrets scan found hits in site/dist. See above.\033[0m\n' >&2
      exit 1
    fi

    printf '\033[1;32mDocs site verification complete: build + PII scan + link validation passed.\033[0m\n'
  )
fi

step "Public-tree PII/secrets scan"
# The docs-site gate above only sees site/dist. This runs the same deny-list
# over everything that would ship in the PUBLIC repository — source, config,
# workflows, docs — so a personal address in a Rust fixture or a live access
# code in a runbook fails here rather than after publication.
if ! ./scripts/scan-public-tree.sh; then
  printf '\n\033[1;31mPUBLIC TREE GATE BLOCKED: PII/secrets scan found hits. See above.\033[0m\n' >&2
  exit 1
fi

step "Portability note (#99)"
cat <<'EOF'
The "no CommandLineTools rpath" portability proof (#99) is enforced at
release-build time via the full-Xcode recipe (RUSTFLAGS="" DEVELOPER_DIR=
/Applications/Xcode.app ... — see CLAUDE.md) and by the macos-15 CI workflow.
This local gate builds with the committed dev config for speed; it does not
re-verify the release rpath. Run the release build before shipping.
EOF

printf '\n\033[1;32mALL LOCAL CI CHECKS PASSED ✅\033[0m\n'

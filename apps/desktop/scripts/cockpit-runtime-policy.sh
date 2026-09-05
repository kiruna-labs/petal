#!/usr/bin/env bash
# Shared QA-only Swift runtime policy. Full Xcode supplies static compatibility
# archives at build time; direct cockpit launches resolve Swift dynamically from
# the OS. Never add toolchain rpaths or DYLD wrappers here (#315).

COCKPIT_XCODE_DEVELOPER_DIR="${PETAL_COCKPIT_XCODE_DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
COCKPIT_XCODE_SWIFT_LINK_DIR="$COCKPIT_XCODE_DEVELOPER_DIR/Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx"
COCKPIT_SYSTEM_SWIFT_RPATH="/usr/lib/swift"

cockpit_runtime_configure_build() {
  [[ -x "$COCKPIT_XCODE_DEVELOPER_DIR/usr/bin/xcodebuild" ]] || { echo "cockpit runtime: full Xcode is required at $COCKPIT_XCODE_DEVELOPER_DIR" >&2; return 2; }
  [[ -d "$COCKPIT_XCODE_SWIFT_LINK_DIR" ]] || { echo "cockpit runtime: missing Xcode Swift link dir: $COCKPIT_XCODE_SWIFT_LINK_DIR" >&2; return 2; }
  # Deliberately replaces src-tauri/.cargo/config.toml's CLT flags.
  export DEVELOPER_DIR="$COCKPIT_XCODE_DEVELOPER_DIR"
  export MACOSX_DEPLOYMENT_TARGET=13.0
  export RUSTFLAGS="-L $COCKPIT_XCODE_SWIFT_LINK_DIR"
}

cockpit_runtime_assert_qa_artifact() {
  local artifact="$1" rpaths
  [[ -x "$artifact" ]] || { echo "cockpit runtime: artifact missing: $artifact" >&2; return 2; }
  rpaths="$(otool -l "$artifact" | awk '$1 == "path" { print $2 }')"
  printf '%s\n' "$rpaths" | grep -Fxq "$COCKPIT_SYSTEM_SWIFT_RPATH" || { echo "cockpit runtime: QA artifact lacks system Swift LC_RPATH: $artifact" >&2; return 1; }
  if printf '%s\n' "$rpaths" | grep -E 'CommandLineTools|XcodeDefault\.xctoolchain' >/dev/null || otool -L "$artifact" | grep -E 'CommandLineTools|XcodeDefault\.xctoolchain' >/dev/null; then
    echo "cockpit runtime: QA artifact carries forbidden toolchain runtime path: $artifact" >&2
    return 1
  fi
}

cockpit_runtime_assert_non_qa_artifact() {
  local artifact="$1"
  [[ -x "$artifact" ]] || { echo "cockpit runtime: artifact missing: $artifact" >&2; return 2; }
  if otool -l "$artifact" | grep -E 'CommandLineTools|XcodeDefault\.xctoolchain' >/dev/null; then
    echo "cockpit runtime: non-QA artifact carries forbidden toolchain runtime path: $artifact" >&2
    return 1
  fi
}

# `lsof` intentionally does not show dylibs mapped from macOS's dyld shared
# cache. vmmap does; reduce its many segment rows to distinct image paths.
cockpit_runtime_assert_system_swift_mapping() {
  local mapping="$1" concurrency_paths
  [[ -f "$mapping" ]] || { echo "cockpit runtime: vmmap evidence missing: $mapping" >&2; return 2; }
  if grep -E 'CommandLineTools|XcodeDefault\.xctoolchain' "$mapping" >/dev/null; then
    echo "cockpit runtime: vmmap contains a forbidden toolchain runtime image" >&2
    return 1
  fi
  concurrency_paths="$(awk '$NF == "/usr/lib/swift/libswift_Concurrency.dylib" { print $NF }' "$mapping" | sort -u)"
  if [[ "$concurrency_paths" != "/usr/lib/swift/libswift_Concurrency.dylib" ]]; then
    echo "cockpit runtime: expected exactly one system libswift_Concurrency image in vmmap" >&2
    return 1
  fi
}

# Launches only this owned process. Run in a subshell so EXIT cleanup is
# unconditional for every failed assertion (including signals), unlike a
# RETURN trap whose scope depends on the caller's shell frame.
cockpit_runtime_verify_direct_launch() (
  set -euo pipefail
  local artifact="$1" label="$2" run_dir sock log mapping pid="" response
  cockpit_runtime_assert_qa_artifact "$artifact"
  run_dir="$(mktemp -d -t petal-cockpit-runtime)"; sock="$run_dir/autotest.sock"; log="$run_dir/${label}.log"; mapping="$run_dir/${label}-swift-mapping.txt"
  cleanup() {
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    rm -f "$sock"
    echo "cockpit runtime: retained launch evidence: $run_dir"
  }
  trap cleanup EXIT INT TERM
  env -i PATH="$(getconf PATH)" HOME="$HOME" PETAL_DISABLE_AUDIO=1 PETAL_AUTOTEST_SOCK="$sock" "$artifact" >"$log" 2>&1 & pid=$!
  for _ in $(seq 1 40); do [[ -S "$sock" ]] && break; kill -0 "$pid" 2>/dev/null || { cat "$log" >&2; exit 1; }; sleep 0.25; done
  [[ -S "$sock" ]] || { cat "$log" >&2; exit 1; }
  response="$(printf '{"cmd":"dump_state"}\n' | nc -U "$sock")"; [[ "$response" == *'"ok":true'* ]] || { printf '%s\n' "$response" >&2; exit 1; }
  vmmap -interleaved "$pid" >"$mapping" 2>&1 || { cat "$mapping" >&2; exit 1; }
  if ! cockpit_runtime_assert_system_swift_mapping "$mapping" || grep -E 'Class .* is implemented in both|Library not loaded:|dyld\[.*\]:' "$log" >/dev/null; then
    echo "cockpit runtime: direct launch found a mixed Swift runtime, duplicate class, or dyld failure" >&2; exit 1
  fi
  echo "cockpit runtime: sanitized $label launch and single-runtime mapping passed"
)

# Focused, non-GUI contract test for the Mach-O inspection policy. It uses a
# temporary fake `otool`, not a real binary or process, so CI can exercise the
# acceptance boundary without Screen Recording/Accessibility/TCC.
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  [[ "${1:-}" == "--self-test" ]] || { echo "usage: $0 --self-test" >&2; exit 64; }
  test_dir="$(mktemp -d -t petal-cockpit-runtime-policy)"
  trap 'rm -rf "$test_dir"' EXIT
  mkdir -p "$test_dir/bin" "$test_dir/fixtures"
  : > "$test_dir/fixtures/qa"; : > "$test_dir/fixtures/default"; : > "$test_dir/fixtures/bad"
  chmod +x "$test_dir/fixtures/qa" "$test_dir/fixtures/default" "$test_dir/fixtures/bad"
  cat > "$test_dir/bin/otool" <<'EOF'
#!/usr/bin/env bash
artifact="${@: -1}"
case "$artifact" in
  */qa) printf 'Load command 1\n cmd LC_RPATH\n path /usr/lib/swift (offset 12)\n' ;;
  */default) printf 'Load command 1\n cmd LC_RPATH\n path /usr/lib/swift (offset 12)\n' ;;
  */bad) printf 'Load command 1\n cmd LC_RPATH\n path /Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx (offset 12)\n' ;;
esac
EOF
  chmod +x "$test_dir/bin/otool"
  PATH="$test_dir/bin:$PATH"
  cockpit_runtime_assert_qa_artifact "$test_dir/fixtures/qa"
  cockpit_runtime_assert_non_qa_artifact "$test_dir/fixtures/default"
  if cockpit_runtime_assert_qa_artifact "$test_dir/fixtures/bad"; then
    echo "cockpit runtime self-test: forbidden toolchain QA path was accepted" >&2
    exit 1
  fi
  if cockpit_runtime_assert_non_qa_artifact "$test_dir/fixtures/bad"; then
    echo "cockpit runtime self-test: forbidden toolchain default path was accepted" >&2
    exit 1
  fi
  cat > "$test_dir/good-vmmap.txt" <<'EOF'
__TEXT 123 /usr/lib/swift/libswift_Concurrency.dylib
__DATA 456 /usr/lib/swift/libswift_Concurrency.dylib
EOF
  cat > "$test_dir/toolchain-vmmap.txt" <<'EOF'
__TEXT 123 /usr/lib/swift/libswift_Concurrency.dylib
__TEXT 456 /Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx/libswift_Concurrency.dylib
EOF
  cockpit_runtime_assert_system_swift_mapping "$test_dir/good-vmmap.txt"
  if cockpit_runtime_assert_system_swift_mapping "$test_dir/toolchain-vmmap.txt"; then
    echo "cockpit runtime self-test: toolchain vmmap was accepted" >&2
    exit 1
  fi
  echo "cockpit runtime self-test: passed"
fi

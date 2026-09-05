#!/usr/bin/env bash
# Non-GUI regression for #315: every runtime-policy failure must stop the peer
# builder before it can print its success line. No Cargo build or app launch.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TMP="$(mktemp -d -t petal-peer-runtime-test)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/apps/desktop/scripts" "$TMP/fake-bin"
cp "$SCRIPT_DIR/build-test-peer.sh" "$SCRIPT_DIR/cockpit-runtime-policy.sh" "$TMP/apps/desktop/scripts/"
chmod +x "$TMP/apps/desktop/scripts/build-test-peer.sh"

cat > "$TMP/fake-bin/cargo" <<'EOF'
#!/usr/bin/env bash
mkdir -p "$CARGO_TARGET_DIR/debug"
cat > "$CARGO_TARGET_DIR/debug/desktop" <<'PY'
#!/usr/bin/env python3
import os, socket, sys
path = os.environ["PETAL_AUTOTEST_SOCK"]
s = socket.socket(socket.AF_UNIX); s.bind(path); s.listen(1)
c, _ = s.accept(); c.recv(4096); c.sendall(b'{"ok":true}\n'); c.close(); s.close()
PY
chmod +x "$CARGO_TARGET_DIR/debug/desktop"
EOF
chmod +x "$TMP/fake-bin/cargo"

cat > "$TMP/fake-bin/otool" <<'EOF'
#!/usr/bin/env bash
if [[ "${PETAL_PEER_TEST_MODE:-}" == "assert" ]]; then
  printf 'Load command 1\n cmd LC_RPATH\n path /Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx (offset 12)\n'
else
  printf 'Load command 1\n cmd LC_RPATH\n path /usr/lib/swift (offset 12)\n'
fi
EOF
chmod +x "$TMP/fake-bin/otool"

expect_failure() {
  local name="$1"; shift
  local output status
  set +e
  output="$("$@" 2>&1)"; status=$?
  set -e
  if [[ $status -eq 0 ]] || grep -Fq 'OK -- test-peer binary' <<<"$output"; then
    echo "peer runtime self-test: $name incorrectly reached success" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
  echo "peer runtime self-test: $name failed closed"
}

PEER="$TMP/apps/desktop/scripts/build-test-peer.sh"
expect_failure configure env PETAL_COCKPIT_XCODE_DEVELOPER_DIR=/does/not/exist "$PEER"
expect_failure assert env PATH="$TMP/fake-bin:$PATH" PETAL_PEER_TEST_MODE=assert "$PEER"
expect_failure verify env PATH="$TMP/fake-bin:$PATH" PETAL_PEER_TEST_MODE=verify "$PEER" --verify-direct-launch

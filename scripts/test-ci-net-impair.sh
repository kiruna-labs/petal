#!/usr/bin/env bash
# scripts/ci/net-impair.sh (#378), both directions, with fake pfctl/dnctl:
# rule text is port-scoped, an impairment that does not measurably take is
# REFUSED and torn down, a failed apply tears down, and check-clear fails while
# a pipe is still installed.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT/scripts/ci/net-impair.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "ok - $*"; }

# Fakes log every call; pfctl -f fails when FAKE_PF_LOAD_FAIL=1; dnctl `pipe show`
# prints a live pipe while $WORK/pipe exists.
cat > "$WORK/pfctl" <<'EOF'
#!/bin/bash
echo "pfctl $*" >> "$FAKE_LOG"
case " $* " in
  *" -f "*) cat >/dev/null; [ "${FAKE_PF_LOAD_FAIL:-0}" = 1 ] && exit 1; exit 0 ;;
  *" -E "*) echo "Token : 4242" >&2; exit 0 ;;
esac
exit 0
EOF
cat > "$WORK/dnctl" <<'EOF'
#!/bin/bash
echo "dnctl $*" >> "$FAKE_LOG"
case " $* " in
  *" config "*) touch "$FAKE_DIR/pipe" ;;
  *" delete "*) rm -f "$FAKE_DIR/pipe" ;;
  *" show "*) [ -f "$FAKE_DIR/pipe" ] && echo "37801:  unlimited   20 ms   50 sl. plr 0.010000" ;;
esac
exit 0
EOF
# Prints the next value from $FAKE_DIR/probe-values (one per call).
cat > "$WORK/probe" <<'EOF'
#!/bin/bash
n=$(cat "$FAKE_DIR/probe-n" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$FAKE_DIR/probe-n"
sed -n "${n}p" "$FAKE_DIR/probe-values"
EOF
chmod +x "$WORK/pfctl" "$WORK/dnctl" "$WORK/probe"

run() {
  FAKE_LOG="$WORK/log" FAKE_DIR="$WORK" \
  PETAL_CI_NET_IMPAIR_PFCTL="$WORK/pfctl" PETAL_CI_NET_IMPAIR_DNCTL="$WORK/dnctl" \
  PETAL_CI_NET_IMPAIR_SUDO="" PETAL_CI_NET_IMPAIR_PROBE_CMD="$WORK/probe" \
  PETAL_CI_NET_IMPAIR_STATE_DIR="$WORK/state" \
    /bin/bash "$SCRIPT" "$@"
}
reset() { rm -f "$WORK/log" "$WORK/pipe" "$WORK/probe-n"; rm -rf "$WORK/state"; printf "%s\n" "$@" > "$WORK/probe-values"; }

# 1. Dry run: rule text scopes to the LiveKit ports, both directions, one pipe.
out="$(PETAL_CI_NET_IMPAIR_DRY_RUN=1 PETAL_CI_NET_IMPAIR_SUDO="" /bin/bash "$SCRIPT" on 20 1)"
grep -qx 'dummynet out quick proto { tcp udp } from any to any port 7880:7882 pipe 37801' <<<"$out" || fail "dry run: missing dst-port rule: $out"
grep -qx 'dummynet out quick proto { tcp udp } from any port 7880:7882 to any pipe 37801' <<<"$out" || fail "dry run: missing src-port rule: $out"
grep -q 'pipe 37801 config delay 20ms plr 0.0100' <<<"$out" || fail "dry run: pipe config wrong: $out"
! grep -Eq 'port (9222|5185)|from any to any pipe' <<<"$out" || fail "dry run: rule is not port-scoped: $out"
pass "dry run prints port-scoped rules and the pipe config"

# 2. GREEN: connect time rises by ~2x delay -> ON, token stored.
reset 0.4 41.0
run on 20 1 > "$WORK/out" 2>&1 || { cat "$WORK/out"; fail "a measurable impairment was refused"; }
grep -q 'ON: ports 7880:7882, 20ms' "$WORK/out" || fail "no ON line: $(cat "$WORK/out")"
[ "$(cat "$WORK/state/pf-token")" = 4242 ] || fail "pf token not stored"
pass "measurable impairment is applied and reported"

# 3. check-clear goes RED while the pipe is installed...
run check-clear > "$WORK/out" 2>&1 && fail "check-clear passed with pipe 37801 installed"
grep -q 'STILL INSTALLED' "$WORK/out" || fail "check-clear failure not named: $(cat "$WORK/out")"
pass "check-clear fails while the impairment is installed"

# 4. ...and off tears down (anchor flush, pipe delete, token release) -> GREEN.
run off > "$WORK/out" 2>&1 || { cat "$WORK/out"; fail "off failed"; }
grep -q "pfctl -q -a com.apple/petal-ci-impair -F all" "$WORK/log" || fail "off did not flush the anchor"
grep -q "dnctl -q pipe delete 37801" "$WORK/log" || fail "off did not delete the pipe"
grep -q "pfctl -q -X 4242" "$WORK/log" || fail "off did not release the pf token"
run check-clear > "$WORK/out" 2>&1 || { cat "$WORK/out"; fail "check-clear failed after off"; }
pass "off tears down and check-clear then passes"

# 5. RED: the rule loads but the path does not slow down -> refused + torn down.
reset 0.4 0.5
run on 20 1 > "$WORK/out" 2>&1 && fail "an impairment that did not take was accepted"
grep -q 'did NOT take effect' "$WORK/out" || fail "refusal not named: $(cat "$WORK/out")"
[ ! -f "$WORK/pipe" ] || fail "refused impairment left pipe 37801 installed"
pass "an impairment that does not measurably take is refused and removed"

# 6. RED: pf refuses the rules -> the ERR trap tears the pipe back down.
reset 0.4 41.0
FAKE_PF_LOAD_FAIL=1 run on 20 1 > "$WORK/out" 2>&1 && fail "a failed pf load was accepted"
[ ! -f "$WORK/pipe" ] || fail "failed apply left pipe 37801 installed"
pass "a failed apply is torn down"

# 7. Nothing listening -> refuse before touching pf (an unmeasurable impairment).
reset ""
run on 20 1 > "$WORK/out" 2>&1 && fail "applied with no SFU to measure"
! grep -q "config" "$WORK/log" 2>/dev/null || fail "touched dnctl with no SFU listening"
pass "refuses when the SFU is not listening"

# 8. Argument validation.
for bad in "on x 1" "on 20 abc" "on 20 75" "on 5000 1" "bogus"; do
  # shellcheck disable=SC2086
  run $bad > "$WORK/out" 2>&1 && fail "accepted bad arguments: $bad"
done
pass "rejects malformed / outage-sized arguments"
echo "net-impair: all checks passed"

#!/usr/bin/env bash
#
# Long-lived ShelldClient connection across a shelld execv self-update.
#
# Today's incident root-cause: the client's reader thread treated socket
# EOF from a daemon image swap as "all sessions exited", marspot-core saw
# every pane exited, and quit — applications disappeared. The fix added
# a supervisor that reconnects + re-attaches across the EOF.
#
# This is the regression test. It:
#   1. Starts a sandbox shelld.
#   2. Launches the probe (long_connection_execv_probe) which opens 2
#      sessions, writes a PRE_MARK on each, then SIGSTOPs itself after
#      printing PROBE_READY.
#   3. While the probe is stopped, this script SIGUSR1's shelld (the
#      execv path) and SIGCONT's the probe.
#   4. Asserts the probe exits 0 with PROBE_PASS — sessions stayed
#      alive across the swap.
#
# SAFETY: sandbox via MARSPOT_STATE_DIR; never touches real state.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="${MARSPOT_TEST_PROFILE:-debug}"
SHELLD="$ROOT/target/$PROFILE/marspot-shelld"
PROBE="$ROOT/target/$PROFILE/examples/long_connection_execv_probe"
[[ -x "$SHELLD" && -x "$PROBE" ]] || {
  echo "FAIL: build first — cargo build${PROFILE:+ --$PROFILE} && cargo build${PROFILE:+ --$PROFILE} -p marspot-session --example long_connection_execv_probe"
  exit 1
}

STATE_DIR="/tmp/marspot-long-conn-execv.$$"
TREE="$STATE_DIR/binaries"
SOCK="$STATE_DIR/shelld.sock"
export MARSPOT_STATE_DIR="$STATE_DIR"

cleanup() {
  for p in ${PROBE_PID:-} ${SHELLD_PID:-}; do
    kill -KILL "$p" 2>/dev/null || true
    wait "$p" 2>/dev/null || true
  done
  rm -rf "$STATE_DIR"
}
trap cleanup EXIT

fail() {
  echo
  echo "FAIL: $*"
  echo "  (probe stdout):"
  sed 's/^/    /' "$STATE_DIR/probe.stdout" 2>/dev/null
  echo "  (probe stderr):"
  sed 's/^/    /' "$STATE_DIR/probe.stderr" 2>/dev/null
  echo "  (shelld marspot.log execv slice):"
  grep -E $'\tEXECV_|\tSHELLD_START' "$STATE_DIR/logs/marspot.log" 2>/dev/null | head -8 | sed 's/^/    /'
  exit 1
}

# Setup: copy binary into the tree's current/ slot (so the execv path
# can also see it as a promote target).
mkdir -p "$TREE/current"
cp "$SHELLD" "$TREE/current/marspot-shelld"
chmod +x "$TREE/current/marspot-shelld"

# Launch shelld.
"$TREE/current/marspot-shelld" >"$STATE_DIR/shelld.stderr" 2>&1 &
SHELLD_PID=$!
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  [[ -S "$SOCK" ]] && break
  sleep 0.1
done
[[ -S "$SOCK" ]] || fail "shelld did not bind socket"
echo "shelld up: pid=$SHELLD_PID"

# Launch the probe. It will block on SIGSTOP after printing PROBE_READY.
"$PROBE" >"$STATE_DIR/probe.stdout" 2>"$STATE_DIR/probe.stderr" &
PROBE_PID=$!

# Wait until the probe has reached the SIGSTOP point.
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  grep -q "^PROBE_READY" "$STATE_DIR/probe.stdout" 2>/dev/null && break
  sleep 0.1
done
grep -q "^PROBE_READY" "$STATE_DIR/probe.stdout" 2>/dev/null \
  || fail "probe did not reach PROBE_READY"

# Confirm the probe is actually stopped (T = stopped on macOS ps).
state=$(ps -o stat= -p "$PROBE_PID" 2>/dev/null | tr -d ' ')
[[ "$state" == T* || "$state" == U* ]] \
  || fail "probe is not stopped (state='$state')"

ids=$(grep '^PROBE_READY' "$STATE_DIR/probe.stdout" | head -1 | awk '{print $2}')
echo "probe parked with sessions $ids"

# Stage a byte-identical pending so the execv path has something to
# promote — the probe survives if the swap preserves session ids.
mkdir -p "$TREE/pending"
cp "$SHELLD" "$TREE/pending/marspot-shelld"
chmod +x "$TREE/pending/marspot-shelld"

# Trigger the execv swap on shelld.
PRE_PID=$SHELLD_PID
kill -USR1 "$SHELLD_PID" || fail "kill -USR1 failed"

# Give shelld 800 ms to do the swap. The probe is suspended, so the
# wall time here is purely the shelld-side work.
sleep 0.8

# Same PID = execv preserved process identity (= sessions survived,
# at least on the shelld side).
kill -0 "$SHELLD_PID" 2>/dev/null \
  || fail "shelld died across SIGUSR1 — execv crashed mid-swap"
[[ "$(ps -o pid= -p "$SHELLD_PID" | tr -d ' ')" == "$PRE_PID" ]] \
  || fail "shelld PID changed"

# Wake the probe; it will now run phase 2 (assert is_exited=false) and
# phase 3 (write again through the reconnected stream).
kill -CONT "$PROBE_PID" || fail "kill -CONT probe failed"

# Wait for the probe to finish.
wait "$PROBE_PID"
rc=$?
PROBE_PID=""
if [[ "$rc" != "0" ]]; then
  fail "probe exited rc=$rc"
fi

grep -q "^PROBE_PASS" "$STATE_DIR/probe.stdout" \
  || fail "probe didn't print PROBE_PASS"
echo "  $(grep '^PROBE_PASS' $STATE_DIR/probe.stdout)"

echo "ALL PASSED: long-lived ShelldClient survived a shelld execv swap"

#!/usr/bin/env bash
#
# shelld cold-start boot-promote test. When the updater (or anyone) drops
# a new binary into `binaries/pending/marspot-shelld`, the daemon's next
# launch must atomically slide it into `current/` so subsequent launches
# load the new image — without any external --apply-pending invocation.
# This is the cold-start safety net under the upcoming SIGUSR1 execv
# self-update path; if execv fails and launchd KeepAlive re-execs us,
# we still come up on the new binary.
#
# SAFETY: runs marspot-shelld in a sandboxed state dir (MARSPOT_STATE_DIR)
# with a unique pid-suffixed socket; never touches ~/Library/Caches/marspot
# or the installed daemon. Build the binary first (`cargo build` or
# `bin/run.sh` already did it for you).

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SHELLD="$ROOT/target/debug/marspot-shelld"
if [[ ! -x "$SHELLD" ]]; then
  SHELLD="$ROOT/target/release/marspot-shelld"
fi
[[ -x "$SHELLD" ]] || { echo "FAIL: build marspot-shelld first (cargo build)"; exit 1; }

STATE_DIR="/tmp/marspot-shelld-bootpromote.$$"
TREE="$STATE_DIR/binaries"

export MARSPOT_STATE_DIR="$STATE_DIR"

cleanup() {
  # Kill any daemon we left running.
  if [[ -n "${SHELLD_PID:-}" ]]; then
    kill -TERM "$SHELLD_PID" 2>/dev/null || true
    wait "$SHELLD_PID" 2>/dev/null || true
  fi
  rm -rf "$STATE_DIR"
}
trap cleanup EXIT

run_shelld_briefly() {
  # Run shelld in the background, give it a moment to do boot work + bind,
  # then SIGTERM it. We only need its startup-side effects (boot-promote).
  "$SHELLD" >"$STATE_DIR/stderr.log" 2>&1 &
  SHELLD_PID=$!
  # Poll up to 2 s for the socket to appear, then kill.
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    if [[ -S "$STATE_DIR/shelld.sock" ]]; then break; fi
    sleep 0.1
  done
  kill -TERM "$SHELLD_PID" 2>/dev/null || true
  wait "$SHELLD_PID" 2>/dev/null || true
  SHELLD_PID=""
}

fail() {
  echo "FAIL: $*"
  echo "  (stderr.log):"
  sed 's/^/    /' "$STATE_DIR/stderr.log" 2>/dev/null | head -20
  echo "  (tree contents):"
  find "$TREE" -type f 2>/dev/null | sed 's/^/    /'
  exit 1
}

# --- case 1: no pending → nothing promoted, no current/ created. -------
mkdir -p "$STATE_DIR"
run_shelld_briefly
if [[ -e "$TREE/current/marspot-shelld" ]]; then
  fail "no pending should not create current/"
fi
echo "PASS: no-pending → no promote"

# --- case 2: pending present → promoted into current/. -----------------
rm -rf "$STATE_DIR"
mkdir -p "$TREE/pending"
echo "FAKE_PENDING_BINARY_BYTES" > "$TREE/pending/marspot-shelld"
chmod +x "$TREE/pending/marspot-shelld"

run_shelld_briefly

if [[ -e "$TREE/pending/marspot-shelld" ]]; then
  fail "pending/ should be empty after promote"
fi
if [[ ! -e "$TREE/current/marspot-shelld" ]]; then
  fail "current/marspot-shelld should exist after promote"
fi
if ! grep -q "FAKE_PENDING_BINARY_BYTES" "$TREE/current/marspot-shelld"; then
  fail "current/ does not contain the pending bytes"
fi
if ! grep -q "boot-promote: pending → current" "$STATE_DIR/stderr.log"; then
  fail "boot-promote log line missing from stderr"
fi
echo "PASS: pending → current"

# --- case 3: pending + prior current → current goes to prev/, pending → current.
rm -rf "$STATE_DIR"
mkdir -p "$TREE/current" "$TREE/pending"
echo "OLD_CURRENT_BYTES" > "$TREE/current/marspot-shelld"
chmod +x "$TREE/current/marspot-shelld"
echo "NEW_PENDING_BYTES" > "$TREE/pending/marspot-shelld"
chmod +x "$TREE/pending/marspot-shelld"

run_shelld_briefly

grep -q "NEW_PENDING_BYTES" "$TREE/current/marspot-shelld" \
  || fail "current/ should contain new pending bytes"
grep -q "OLD_CURRENT_BYTES" "$TREE/prev/marspot-shelld" \
  || fail "prev/ should contain old current bytes"
[[ ! -e "$TREE/pending/marspot-shelld" ]] \
  || fail "pending/ should be empty"
echo "PASS: rotation current → prev, pending → current"

echo "ALL PASSED"

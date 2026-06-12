#!/usr/bin/env bash
#
# Integration smoke test for the shell/core architecture (Step 8).
#
# What it covers:
#   1. Boot — shell + core start, HelloAck arrives within 5 s.
#   2. Crash recovery — SIGKILL the core, shell respawns one within
#      ~1 s, HelloAck arrives again.
#   3. Crash budget — 4 SIGKILLs back-to-back trip
#      `auto_restart_disabled`.
#
# This is a smoke test, not an exhaustive functional one: it doesn't
# exercise input forwarding, resize, or the IOSurface render path.
# Those need a windowed environment; this script can run headless on
# the dev box and still catches the supervisor / liveness regressions
# that an unattended Step 8 should guard against.
#
# Exits 0 on pass, non-zero with a diagnostic line on the first
# failure.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOG=/tmp/marspot-test-shell-core.log
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"

fail() {
  echo "FAIL: $*"
  echo "  (last 40 lines of $LOG):"
  tail -40 "$LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() {
  pkill -9 -f marspot-shell  >/dev/null 2>&1 || true
  pkill -9 -f marspot-core   >/dev/null 2>&1 || true
}
trap cleanup EXIT

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" ]]; then
  echo "building release …"
  ( cd "$ROOT" && cargo build --release --bin marspot-shell --bin marspot-core 2>&1 | tail -3 )
fi

# Wipe stale binary tree so the supervisor starts from a clean slate.
rm -rf "$HOME/Library/Caches/marspot/binaries"

# A previously-tripped crash budget poisons the test: each SIGKILL
# pushes a new entry into the rolling window, so re-running within
# 5 min would re-trip immediately.  We fix this by re-launching the
# shell (each shell process has its own crash window).  No-op here
# beyond that note.

cleanup
> "$LOG"

# --- 1. Boot --------------------------------------------------------
nohup "$SHELL_BIN" >"$LOG" 2>&1 < /dev/null &
disown
SHELL_PID=$!

# Wait up to 5 s for HelloAck.
for _ in $(seq 1 50); do
  if grep -q "HelloAck v=1" "$LOG"; then
    break
  fi
  sleep 0.1
done
if ! grep -q "HelloAck v=1" "$LOG"; then
  fail "no HelloAck within 5 s"
fi
echo "[1/3] boot OK — HelloAck v=1 received"

# --- 2. Crash recovery ---------------------------------------------
PRE_PID=$(pgrep -f "$CORE_BIN" | head -1)
[[ -n "$PRE_PID" ]] || fail "no marspot-core running after boot"

# Count HelloAcks before we kill; the restart should produce a fresh one.
BEFORE_ACKS=$(grep -c "HelloAck v=1" "$LOG")
kill -9 "$PRE_PID"
for _ in $(seq 1 30); do
  POST_PID=$(pgrep -f "$CORE_BIN" | head -1)
  if [[ -n "$POST_PID" && "$POST_PID" != "$PRE_PID" ]]; then
    AFTER_ACKS=$(grep -c "HelloAck v=1" "$LOG")
    if (( AFTER_ACKS > BEFORE_ACKS )); then
      break
    fi
  fi
  sleep 0.1
done
if [[ -z "${POST_PID:-}" || "$POST_PID" == "$PRE_PID" ]]; then
  fail "core did not respawn after SIGKILL"
fi
if (( AFTER_ACKS <= BEFORE_ACKS )); then
  fail "respawned core did not HelloAck"
fi
echo "[2/3] crash recovery OK — core $PRE_PID → $POST_PID, fresh HelloAck"

# --- 3. Crash budget ------------------------------------------------
# We've already used 1 crash; 3 more in quick succession should
# trip the budget (MAX_CRASHES_IN_WINDOW = 3).
for _ in 1 2 3; do
  P=$(pgrep -f "$CORE_BIN" | head -1)
  if [[ -n "$P" ]]; then
    kill -9 "$P"
    sleep 1
  fi
done

# Give the shell a beat to log + decide.
sleep 2
if ! grep -q "crash budget exceeded" "$LOG"; then
  fail "crash budget did not trip after 4 SIGKILLs"
fi
if pgrep -f "$CORE_BIN" >/dev/null; then
  fail "core still running after crash budget trip"
fi
echo "[3/3] crash budget OK — auto-restart disabled, no core respawn"

cleanup
echo "ALL PASS"

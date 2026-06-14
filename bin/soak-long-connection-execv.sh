#!/usr/bin/env bash
#
# Long-lived ShelldClient connection across N back-to-back shelld
# execv self-updates.
#
# Builds on test-long-connection-execv.sh: that one verifies a single
# swap; this one stresses the supervisor's reconnect / re-attach loop
# under sustained churn. Each cycle:
#
#   - Stages a fresh pending shelld binary (byte-identical — the
#     interesting work is in the swap mechanics, not the contents)
#   - Generates a unique "version" string per cycle and prints it as a
#     log marker so a triage operator can correlate marspot.log events
#     to the cycle that produced them; this is what the user meant by
#     "每次都 bump 一下版本号没关系" without forcing a real cargo build
#     per cycle.
#   - Sends SIGUSR1 to shelld to trigger the execv swap.
#   - Asserts: shelld PID unchanged, socket inode unchanged (= listen
#     fd inherited), every session id present + child PID alive,
#     long-lived probe connection still operational (no session
#     reported exited during the reconnect window).
#
# Default: SESSIONS=2 CYCLES=5. Set CYCLES=20 for a thorough run.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SHELLD="$ROOT/target/debug/marspot-shelld"
PROBE="$ROOT/target/debug/examples/long_connection_execv_probe"
SESSION_PROBE="$ROOT/target/debug/examples/shelld_session_probe"
[[ -x "$SHELLD" && -x "$PROBE" && -x "$SESSION_PROBE" ]] || {
  echo "FAIL: build first — cargo build && cargo build -p marspot-session --examples"
  exit 1
}

SESSIONS=${SESSIONS:-2}
CYCLES=${CYCLES:-5}

STATE_DIR="/tmp/marspot-long-conn-soak.$$"
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
  echo "  (last 30 execv-related events from marspot.log):"
  grep -E $'\tEXECV_|\tSHELLD_' "$STATE_DIR/logs/marspot.log" 2>/dev/null \
    | tail -30 | sed 's/^/    /'
  echo "  (probe stdout):"
  sed 's/^/    /' "$STATE_DIR/probe.stdout" 2>/dev/null
  echo "  (probe stderr):"
  sed 's/^/    /' "$STATE_DIR/probe.stderr" 2>/dev/null
  exit 1
}

# Setup: provision binaries/current/ and start shelld.
mkdir -p "$TREE/current"
cp "$SHELLD" "$TREE/current/marspot-shelld"
chmod +x "$TREE/current/marspot-shelld"
xattr -c "$TREE/current/marspot-shelld" 2>/dev/null || true

"$TREE/current/marspot-shelld" >"$STATE_DIR/shelld.stderr" 2>&1 &
SHELLD_PID=$!
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  [[ -S "$SOCK" ]] && break
  sleep 0.1
done
[[ -S "$SOCK" ]] || fail "shelld did not bind socket"
INITIAL_INODE="$(stat -f '%i' "$SOCK")"
echo "shelld up: pid=$SHELLD_PID, inode=$INITIAL_INODE"

# Phase 1: long-lived probe takes a couple of sessions, suspends.
# We send SIGCONT only at the END of the soak; through every cycle the
# probe is parked, and supervisor_loop owns the reconnect work on its
# own thread inside the probe process — exactly the case the production
# GUI experiences.
"$PROBE" >"$STATE_DIR/probe.stdout" 2>"$STATE_DIR/probe.stderr" &
PROBE_PID=$!
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  grep -q "^PROBE_READY" "$STATE_DIR/probe.stdout" 2>/dev/null && break
  sleep 0.1
done
grep -q "^PROBE_READY" "$STATE_DIR/probe.stdout" 2>/dev/null \
  || fail "probe did not reach PROBE_READY"
PROBE_IDS=$(grep '^PROBE_READY' "$STATE_DIR/probe.stdout" | head -1 | awk '{print $2}')
echo "probe parked with sessions $PROBE_IDS"

# Cycle loop.
for ((cycle=1; cycle<=CYCLES; cycle++)); do
  echo
  echo "--- cycle $cycle/$CYCLES ---"

  # "Bump version" marker — logged so the marspot.log timeline carries
  # a per-cycle anchor without requiring a real cargo rebuild.
  VERSION_TAG="SOAK_V0_2_3_PLUS_$cycle"
  "$SHELLD" --log-event "SOAK_CYCLE_BEGIN" "$VERSION_TAG" >/dev/null 2>&1 || true

  # Stage byte-identical pending.
  mkdir -p "$TREE/pending"
  cp "$SHELLD" "$TREE/pending/marspot-shelld"
  chmod +x "$TREE/pending/marspot-shelld"

  # Trigger execv swap.
  kill -USR1 "$SHELLD_PID" || fail "cycle $cycle: kill -USR1 failed"
  sleep 1

  # Same PID (execv preserves identity).
  kill -0 "$SHELLD_PID" 2>/dev/null \
    || fail "cycle $cycle: shelld pid $SHELLD_PID died"
  # Same socket inode (listen fd inherited).
  CYCLE_INODE="$(stat -f '%i' "$SOCK")"
  [[ "$CYCLE_INODE" == "$INITIAL_INODE" ]] \
    || fail "cycle $cycle: socket inode drifted $INITIAL_INODE → $CYCLE_INODE"
  # Pending consumed, prev/ populated.
  [[ ! -e "$TREE/pending/marspot-shelld" ]] \
    || fail "cycle $cycle: pending/ not consumed"
  [[ -e "$TREE/prev/marspot-shelld" ]] \
    || fail "cycle $cycle: prev/ missing after rotation"

  # The probe is parked; we can't query its in-process is_exited()
  # directly without sending SIGCONT (which would end the test). But
  # the session_probe `list` is an independent process and gives us
  # shelld's authoritative session table.
  CYCLE_LIST="$STATE_DIR/cycle$cycle.list"
  "$SESSION_PROBE" list > "$CYCLE_LIST" 2>"$STATE_DIR/probe.err" \
    || fail "cycle $cycle: session_probe list failed: $(cat $STATE_DIR/probe.err)"
  ACTUAL_N="$(wc -l <"$CYCLE_LIST" | tr -d ' ')"
  (( ACTUAL_N >= SESSIONS )) \
    || fail "cycle $cycle: expected ≥ $SESSIONS sessions, got $ACTUAL_N"
  while IFS=$'\t' read -r sid spid alive; do
    [[ "$alive" == "true" ]] || fail "cycle $cycle: session $sid not alive (child=$spid)"
    kill -0 "$spid" 2>/dev/null \
      || fail "cycle $cycle: child PID $spid (session $sid) dead in OS"
  done < "$CYCLE_LIST"

  "$SHELLD" --log-event "SOAK_CYCLE_OK" "$VERSION_TAG cycle=$cycle sessions=$ACTUAL_N" >/dev/null 2>&1 || true
  echo "  PASS: cycle $cycle — $ACTUAL_N sessions live, shelld pid stable, inode stable"
done

# Phase 3: wake the long-lived probe; assert it sees no exited session
# and can write through the (post-reconnect) writer.
kill -CONT "$PROBE_PID" || fail "kill -CONT probe failed"
wait "$PROBE_PID"
rc=$?
PROBE_PID=""
[[ "$rc" == "0" ]] || fail "long-lived probe exited rc=$rc after $CYCLES cycles"
grep -q "^PROBE_PASS" "$STATE_DIR/probe.stdout" \
  || fail "probe didn't print PROBE_PASS after $CYCLES cycles"

# Final structured-log sanity: there should be exactly $CYCLES SOAK_CYCLE_OK
# events and the matching $CYCLES EXECV_RESUME_DONE events.
ok_events=$(grep -c $'\tSOAK_CYCLE_OK\t' "$STATE_DIR/logs/marspot.log" 2>/dev/null || true)
resume_events=$(grep -c $'\tEXECV_RESUME_DONE\t' "$STATE_DIR/logs/marspot.log" 2>/dev/null || true)
[[ "$ok_events" == "$CYCLES" ]] \
  || fail "expected $CYCLES SOAK_CYCLE_OK events, got $ok_events"
[[ "$resume_events" == "$CYCLES" ]] \
  || fail "expected $CYCLES EXECV_RESUME_DONE events, got $resume_events"

echo
echo "ALL PASSED: $SESSIONS-session long connection survived $CYCLES execv swaps"

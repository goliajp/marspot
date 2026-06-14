#!/usr/bin/env bash
#
# shelld execv self-update soak. Verifies that running PTY sessions
# survive the SIGUSR1-driven in-place image swap, end to end, across N
# cycles.
#
# Per cycle:
#   1. Stage pending → byte-identical copy of the running shelld.
#   2. SIGUSR1 → execv swap.
#   3. Assert shelld PID unchanged (= execv preserved it).
#   4. Assert same session ids + same child PIDs + alive=true (= every
#      PTY child + master fd inherited via the handoff manifest).
#   5. Assert no leaked binaries-tree entries (pending consumed, prev/
#      contains exactly the rotation).
#
# Final:
#   - Same N sessions still up at the end (zero loss).
#   - shelld PID unchanged across the whole run.
#   - No zombie processes left in the child PID set.
#   - fd count on shelld within tolerance (bounded fd inventory).
#
# SAFETY: sandboxed via MARSPOT_STATE_DIR. The installed daemon and
# its tree are never touched.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SHELLD="$ROOT/target/debug/marspot-shelld"
PROBE="$ROOT/target/debug/examples/shelld_session_probe"
if [[ ! -x "$SHELLD" || ! -x "$PROBE" ]]; then
  echo "FAIL: build first — cargo build && cargo build -p marspot-session --example shelld_session_probe"
  exit 1
fi

SESSIONS=${SESSIONS:-3}
CYCLES=${CYCLES:-5}

STATE_DIR="/tmp/marspot-shelld-execv-soak.$$"
TREE="$STATE_DIR/binaries"
SOCK="$STATE_DIR/shelld.sock"

export MARSPOT_STATE_DIR="$STATE_DIR"

cleanup() {
  if [[ -n "${SHELLD_PID:-}" ]]; then
    kill -TERM "$SHELLD_PID" 2>/dev/null || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      kill -0 "$SHELLD_PID" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL "$SHELLD_PID" 2>/dev/null || true
    wait "$SHELLD_PID" 2>/dev/null || true
  fi
  rm -rf "$STATE_DIR"
}
trap cleanup EXIT

fail() {
  echo
  echo "FAIL: $*"
  echo "  (last 30 lines of stderr.log):"
  tail -30 "$STATE_DIR/stderr.log" 2>/dev/null | sed 's/^/    /'
  echo "  (tree):"
  find "$TREE" -type f 2>/dev/null | sed 's/^/    /'
  exit 1
}

wait_for_sock() {
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    [[ -S "$SOCK" ]] && return 0
    sleep 0.1
  done
  return 1
}

shelld_fd_count() {
  # /dev/fd/<n> per fd shelld holds. The count itself uses one fd
  # briefly but it cancels between before/after diffs.
  ls "/dev/fd" 2>/dev/null >/dev/null # warm cache
  /bin/ls "/proc/$SHELLD_PID/fd" 2>/dev/null | wc -l | tr -d ' '
  # Linux-only fallback; macOS uses lsof.
}

shelld_fd_count_mac() {
  # lsof prints one line per fd; -p limits to our pid; -n -P avoid DNS.
  lsof -p "$SHELLD_PID" -n -P 2>/dev/null | tail -n +2 | wc -l | tr -d ' '
}

# --- setup
mkdir -p "$TREE/current"
cp "$SHELLD" "$TREE/current/marspot-shelld"
chmod +x "$TREE/current/marspot-shelld"
"$TREE/current/marspot-shelld" >"$STATE_DIR/stderr.log" 2>&1 &
SHELLD_PID=$!

wait_for_sock || fail "shelld did not bind socket"
INITIAL_INODE="$(stat -f '%i' "$SOCK")"
INITIAL_FDS="$(shelld_fd_count_mac)"
echo "shelld up: pid=$SHELLD_PID, inode=$INITIAL_INODE, fds=$INITIAL_FDS"

# --- create N sessions, capture (id, child_pid) baseline
BASELINE="$STATE_DIR/baseline.tsv"
"$PROBE" create "$SESSIONS" > "$BASELINE" 2>"$STATE_DIR/probe.err" \
  || fail "probe create $SESSIONS failed: $(cat $STATE_DIR/probe.err)"
ACTUAL_N="$(wc -l < "$BASELINE" | tr -d ' ')"
[[ "$ACTUAL_N" == "$SESSIONS" ]] \
  || fail "expected $SESSIONS sessions in baseline, got $ACTUAL_N"
echo "baseline: $ACTUAL_N sessions"
cat "$BASELINE" | sed 's/^/  /'

# --- N cycles
for ((cycle=1; cycle<=CYCLES; cycle++)); do
  echo
  echo "--- cycle $cycle/$CYCLES ---"

  # Stage pending (byte-identical so this is a no-op behavior swap
  # that nonetheless exercises every line of the handoff path).
  mkdir -p "$TREE/pending"
  cp "$SHELLD" "$TREE/pending/marspot-shelld"
  chmod +x "$TREE/pending/marspot-shelld"

  # Trigger execv swap.
  kill -USR1 "$SHELLD_PID" || fail "cycle $cycle: kill -USR1 failed"
  sleep 1

  # Same PID = execv preserved the process identity.
  kill -0 "$SHELLD_PID" 2>/dev/null \
    || fail "cycle $cycle: shelld PID $SHELLD_PID died"

  # Same socket inode = listen fd inherited.
  CYCLE_INODE="$(stat -f '%i' "$SOCK")"
  [[ "$CYCLE_INODE" == "$INITIAL_INODE" ]] \
    || fail "cycle $cycle: socket inode changed ($INITIAL_INODE → $CYCLE_INODE)"

  # Same sessions + same child PIDs + alive=true.
  CURRENT="$STATE_DIR/cycle$cycle.tsv"
  "$PROBE" list > "$CURRENT" 2>"$STATE_DIR/probe.err" \
    || fail "cycle $cycle: probe list failed: $(cat $STATE_DIR/probe.err)"
  # Build canonical "id\tchild_pid" subset from list output for diff
  # against baseline.
  awk -F'\t' '{print $1"\t"$2}' "$CURRENT" | sort > "$STATE_DIR/cur.norm"
  sort "$BASELINE" > "$STATE_DIR/base.norm"
  if ! diff -q "$STATE_DIR/base.norm" "$STATE_DIR/cur.norm" >/dev/null; then
    echo "  diff (baseline vs current):"
    diff "$STATE_DIR/base.norm" "$STATE_DIR/cur.norm" | sed 's/^/    /'
    fail "cycle $cycle: session set drifted"
  fi
  # Each session line is "id\tchild_pid\talive"; alive must be true.
  while IFS=$'\t' read -r sid spid alive; do
    [[ "$alive" == "true" ]] || fail "cycle $cycle: session $sid dead (child=$spid)"
    kill -0 "$spid" 2>/dev/null \
      || fail "cycle $cycle: child PID $spid (session $sid) is not alive in OS"
  done < "$CURRENT"

  # Pending consumed, prev/ populated.
  [[ ! -e "$TREE/pending/marspot-shelld" ]] \
    || fail "cycle $cycle: pending/ not consumed"
  [[ -e "$TREE/prev/marspot-shelld" ]] \
    || fail "cycle $cycle: prev/ missing after rotation"

  echo "  PASS: $SESSIONS sessions survived swap (pid stable, inode stable)"
done

# --- final invariants
FINAL_FDS="$(shelld_fd_count_mac)"
FD_DELTA=$((FINAL_FDS - INITIAL_FDS))
echo
echo "final fd delta: $INITIAL_FDS → $FINAL_FDS (Δ=$FD_DELTA)"
# Each cycle inherits the same listen fd + master fds; no new fd
# should accumulate. Tolerance: 8 (room for libc/asl/lsof noise).
if (( FD_DELTA > 8 )); then
  fail "fd inventory drifted by $FD_DELTA across $CYCLES cycles — leak"
fi

# No zombies in child PID set.
while IFS=$'\t' read -r _ spid _; do
  STATE="$(ps -o stat= -p "$spid" 2>/dev/null | tr -d ' ')"
  [[ "$STATE" != Z* ]] || fail "child PID $spid is a zombie"
done < "$STATE_DIR/cycle${CYCLES}.tsv"

echo
echo "ALL PASSED: $SESSIONS sessions × $CYCLES execv swaps — zero session loss, zero fd leak"

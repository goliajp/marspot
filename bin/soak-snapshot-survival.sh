#!/usr/bin/env bash
#
# RFC-002 step 10 — snapshot-survival soak.
#
# Verifies that L4's Terminal SoT (introduced in step 8a) round-trips
# through the execv self-update path bit-for-bit: per session, a
# fingerprint (FNV-1a hash of every grid cell) captured before the swap
# matches the fingerprint captured after, and generation never goes
# backwards.
#
# Per cycle:
#   1. Quiesce: every session has already received an idle prompt
#      from earlier; sleep $SETTLE_S so any in-flight PTY bytes flush
#      into the L4 mirror.
#   2. Pre-fingerprint each session.
#   3. SIGUSR1 → execv swap (uses the same stage-pending machinery
#      as soak-shelld-execv-swap).
#   4. Post-fingerprint each session.
#   5. Assert pre.hash == post.hash AND post.gen >= pre.gen for all.
#
# Defaults: SESSIONS=9 CYCLES=5 SETTLE_S=0.6.
#
# SAFETY: fully sandboxed via MARSPOT_STATE_DIR; the installed daemon
# is never touched.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="${MARSPOT_TEST_PROFILE:-debug}"
SHELLD="$ROOT/target/$PROFILE/marspot-shelld"
PROBE="$ROOT/target/$PROFILE/examples/shelld_session_probe"
if [[ ! -x "$SHELLD" || ! -x "$PROBE" ]]; then
  echo "FAIL: build first — cargo build && cargo build -p marspot-session --example shelld_session_probe"
  exit 1
fi

SESSIONS=${SESSIONS:-9}
CYCLES=${CYCLES:-5}
SETTLE_S=${SETTLE_S:-0.6}

STATE_DIR="/tmp/marspot-snapshot-survival.$$"
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
  exit 1
}

wait_for_sock() {
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    [[ -S "$SOCK" ]] && return 0
    sleep 0.1
  done
  return 1
}

# Capture (id\thash\tgen) for every session into the given file.
snapshot_all() {
  local out="$1"
  : > "$out"
  while IFS=$'\t' read -r sid _; do
    "$PROBE" fingerprint "$sid" >> "$out" 2>"$STATE_DIR/probe.err" \
      || fail "fingerprint $sid failed: $(cat $STATE_DIR/probe.err)"
  done < "$STATE_DIR/baseline.tsv"
}

# --- setup
mkdir -p "$TREE/current"
cp "$SHELLD" "$TREE/current/marspot-shelld"
chmod +x "$TREE/current/marspot-shelld"
"$TREE/current/marspot-shelld" >"$STATE_DIR/stderr.log" 2>&1 &
SHELLD_PID=$!

wait_for_sock || fail "shelld did not bind socket"
echo "shelld up: pid=$SHELLD_PID, SESSIONS=$SESSIONS, CYCLES=$CYCLES, SETTLE_S=$SETTLE_S"

# --- create N sessions; record (id, child_pid).
"$PROBE" create "$SESSIONS" > "$STATE_DIR/baseline.tsv" 2>"$STATE_DIR/probe.err" \
  || fail "probe create $SESSIONS failed: $(cat $STATE_DIR/probe.err)"
ACTUAL_N="$(wc -l < "$STATE_DIR/baseline.tsv" | tr -d ' ')"
[[ "$ACTUAL_N" == "$SESSIONS" ]] \
  || fail "expected $SESSIONS sessions, got $ACTUAL_N"
echo "$ACTUAL_N sessions created"

# --- prime: send a sentinel echo per session so the L4 grid is non-blank
# in a deterministic, identifiable way.  The shell's response (typed
# bytes + executed echo + prompt redraw) populates several rows.
TS="$(date +%Y%m%dT%H%M%S)"
while IFS=$'\t' read -r sid _; do
  printf 'echo MARSPOT-RFC002-SOAK-id%s-ts%s\n' "$sid" "$TS" | \
    "$PROBE" write "$sid" 2>"$STATE_DIR/probe.err" \
    || fail "prime write $sid failed: $(cat $STATE_DIR/probe.err)"
done < "$STATE_DIR/baseline.tsv"

# --- cycles
for ((cycle=1; cycle<=CYCLES; cycle++)); do
  echo
  echo "--- cycle $cycle/$CYCLES ---"

  # Settle: shell prompt round-trip + reader thread feed-through.
  sleep "$SETTLE_S"

  # Pre-swap snapshot of every session.
  snapshot_all "$STATE_DIR/pre.$cycle.tsv"

  # Stage pending (byte-identical) + SIGUSR1 → execv.
  mkdir -p "$TREE/pending"
  cp "$SHELLD" "$TREE/pending/marspot-shelld"
  chmod +x "$TREE/pending/marspot-shelld"
  kill -USR1 "$SHELLD_PID" || fail "cycle $cycle: SIGUSR1 failed"
  sleep 1
  kill -0 "$SHELLD_PID" 2>/dev/null \
    || fail "cycle $cycle: shelld PID $SHELLD_PID died"

  # Settle again so the new image's reader thread drains anything that
  # was queued in the kernel PTY buffer during the swap.
  sleep "$SETTLE_S"

  # Post-swap snapshot.
  snapshot_all "$STATE_DIR/post.$cycle.tsv"

  # Diff: hash must be identical, generation may only increase.
  # awk reads pre + post side-by-side (they're in the same order — we
  # iterate baseline.tsv) and asserts the invariant per row.
  paste "$STATE_DIR/pre.$cycle.tsv" "$STATE_DIR/post.$cycle.tsv" | \
    awk -F'\t' -v cyc="$cycle" '
      {
        if ($1 != $4) {
          printf("session-id mismatch at line %d: pre=%s post=%s\n", NR, $1, $4) > "/dev/stderr"
          exit 1
        }
        if ($2 != $5) {
          printf("cycle %s session %s HASH mismatch: pre=%s post=%s\n", cyc, $1, $2, $5) > "/dev/stderr"
          exit 1
        }
        if ($6 + 0 < $3 + 0) {
          printf("cycle %s session %s generation REGRESSED: pre=%s post=%s\n", cyc, $1, $3, $6) > "/dev/stderr"
          exit 1
        }
      }
    ' || fail "cycle $cycle: snapshot invariant broken (see stderr above)"

  echo "  PASS: $SESSIONS sessions — hash stable, gen non-regressing"
done

echo
echo "ALL PASSED: $SESSIONS sessions × $CYCLES execv swaps —"
echo "  L4 Terminal SoT round-trips bit-for-bit through state.bin persist"

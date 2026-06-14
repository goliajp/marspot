#!/usr/bin/env bash
#
# shelld execv self-update test. The daemon receives SIGUSR1, promotes
# pending → current, and execs over its own image preserving the listen
# socket (so reconnecting clients land on the new daemon transparently).
#
# This test covers the no-session path end-to-end: process up → SIGUSR1
# → execv → new image up → same socket still bound → pending consumed.
# The multi-session variant (sessions survive the swap with continuous
# bytelog + child PTY) lives in bin/soak-shelld-execv-swap.sh.
#
# SAFETY: sandboxed via MARSPOT_STATE_DIR, never touches the installed
# daemon or ~/Library/Caches/marspot. Pending = a byte-identical copy
# of the running binary, so behaviour after swap is unchanged but the
# bytes on disk demonstrably rotated through the slots.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SHELLD="$ROOT/target/debug/marspot-shelld"
if [[ ! -x "$SHELLD" ]]; then
  SHELLD="$ROOT/target/release/marspot-shelld"
fi
[[ -x "$SHELLD" ]] || { echo "FAIL: build marspot-shelld first (cargo build)"; exit 1; }

STATE_DIR="/tmp/marspot-shelld-execv.$$"
TREE="$STATE_DIR/binaries"
SOCK="$STATE_DIR/shelld.sock"

export MARSPOT_STATE_DIR="$STATE_DIR"

cleanup() {
  if [[ -n "${SHELLD_PID:-}" ]]; then
    kill -TERM "$SHELLD_PID" 2>/dev/null || true
    # SIGTERM eventually wins; don't wait forever.
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
  echo "FAIL: $*"
  echo "  (stderr.log):"
  sed 's/^/    /' "$STATE_DIR/stderr.log" 2>/dev/null | tail -30
  echo "  (tree):"
  find "$TREE" -type f 2>/dev/null | sed 's/^/    /'
  exit 1
}

wait_for_sock() {
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    if [[ -S "$SOCK" ]]; then return 0; fi
    sleep 0.1
  done
  return 1
}

# --- Setup: launch shelld pointing at current/marspot-shelld so future
#            execv-target = current/marspot-shelld is well-defined.
mkdir -p "$TREE/current" "$TREE/pending"
cp "$SHELLD" "$TREE/current/marspot-shelld"
chmod +x "$TREE/current/marspot-shelld"
# The installed daemon is exec'd by launchd; here we launch the current/
# binary directly so the running shelld's `current_exe()` matches what
# execv will target.
"$TREE/current/marspot-shelld" >"$STATE_DIR/stderr.log" 2>&1 &
SHELLD_PID=$!

wait_for_sock || fail "shelld did not bind socket in 2s"
INITIAL_INODE="$(stat -f '%i' "$SOCK")"
echo "shelld up: pid=$SHELLD_PID, socket inode=$INITIAL_INODE"

# --- Stage a fresh pending (byte-identical so post-swap behaviour matches).
cp "$SHELLD" "$TREE/pending/marspot-shelld"
chmod +x "$TREE/pending/marspot-shelld"
PENDING_HASH="$(shasum "$TREE/pending/marspot-shelld" | awk '{print $1}')"

# --- Trigger execv swap.
kill -USR1 "$SHELLD_PID" || fail "kill -USR1 failed"

# Give shelld time to: signal_handler → wake → do_execv_swap → execv →
# new image main() → resume → bind ok. 1s budget is generous.
sleep 1

# After execv the PID is unchanged.
if ! kill -0 "$SHELLD_PID" 2>/dev/null; then
  fail "shelld died after SIGUSR1 (execv likely failed mid-swap)"
fi

# Pending must be gone, current must be the staged bytes.
[[ ! -e "$TREE/pending/marspot-shelld" ]] || fail "pending/ should be empty"
[[ -e "$TREE/current/marspot-shelld" ]] || fail "current/ missing"
CURRENT_HASH="$(shasum "$TREE/current/marspot-shelld" | awk '{print $1}')"
[[ "$CURRENT_HASH" == "$PENDING_HASH" ]] || fail "current/ != pending bytes"

# Socket inode is preserved: same kernel-side bound socket, the listen
# fd survived execv. Reconnecting clients hit the new image transparently.
NEW_INODE="$(stat -f '%i' "$SOCK")"
[[ "$NEW_INODE" == "$INITIAL_INODE" ]] \
  || fail "socket inode changed ($INITIAL_INODE → $NEW_INODE); listen fd was not inherited"

# Structured log must show both the outgoing image's EXECV_INVOKE and
# the new image's EXECV_RESUME_BEGIN events on the same TSV stream.
LOGFILE="$STATE_DIR/logs/marspot.log"
grep -q $'\tEXECV_INVOKE\t' "$LOGFILE" 2>/dev/null \
  || fail "outgoing image didn't emit EXECV_INVOKE"
grep -q $'\tEXECV_RESUME_BEGIN\t' "$LOGFILE" 2>/dev/null \
  || fail "new image didn't emit EXECV_RESUME_BEGIN"

echo "PASS: SIGUSR1 → execv → new image up on same listen fd, pending consumed"
echo "ALL PASSED"

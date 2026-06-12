#!/usr/bin/env bash
#
# End-to-end silent-update flow test.  Drives the supervisor through
# a complete swap-and-stabilise cycle using a faked "new" core
# binary that's bit-identical to the current build (the architecture
# doesn't care; it just needs the slot populated).
#
# What it covers:
#   1. Boot — supervisor + core start, HelloAck arrives.
#   2. Stage — drop a binary into `binaries/pending/marspot-core`.
#   3. Trigger — `marspot-shell --trigger` (SIGUSR1) starts the swap.
#   4. Probation — new core spawns from `binaries/current/`, fresh
#      HelloAck arrives.  Live-fail testing (the probation rollback
#      path) is covered by `test-shell-core.sh`'s crash-budget case;
#      here we only verify the happy path graduates.
#   5. Stable — 30 s probation expires, `prev/` is deleted, the log
#      records UPDATE_STABLE.
#
# Run after `cargo build --release`; exits 0 on success.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SUP_LOG="$HOME/Library/Logs/Marspot/supervisor.log"
RUN_LOG=/tmp/marspot-test-update-flow.log

fail() {
  echo "FAIL: $*"
  echo "  (last 30 supervisor events):"
  tail -30 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() {
  # IMPORTANT: matches must be specific to avoid clobbering
  # `marspot-shelld` (the daemon — `marspot-shelld` contains the
  # substring `marspot-shell`).  Match on the trailing space /
  # whitespace boundary using `-x` doesn't quite work for `-f`
  # patterns, so we use a regex that requires not-`d` at end.
  pkill -9 -f '/marspot-shell( |$)'  >/dev/null 2>&1 || true
  pkill -9 -f '/marspot-core( |$)'   >/dev/null 2>&1 || true
}
trap cleanup EXIT

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" ]]; then
  ( cd "$ROOT" && cargo build --release --bin marspot-shell --bin marspot-core 2>&1 | tail -3 )
fi

cleanup
rm -rf "$HOME/Library/Caches/marspot/binaries"
> "$SUP_LOG" 2>/dev/null
nohup "$SHELL_BIN" >"$RUN_LOG" 2>&1 < /dev/null &
disown

# --- 1. Boot --------------------------------------------------------
for _ in $(seq 1 50); do
  if grep -q HELLO_ACK "$SUP_LOG" 2>/dev/null; then break; fi
  sleep 0.1
done
grep -q HELLO_ACK "$SUP_LOG" 2>/dev/null || fail "boot HelloAck never landed"
PRE_CORE_PID=$(pgrep -f "$CORE_BIN" | head -1)
[[ -n "$PRE_CORE_PID" ]] || fail "boot: no core pid"
echo "[1/5] boot OK — core pid=$PRE_CORE_PID, HelloAck logged"

# --- 2. Stage -------------------------------------------------------
mkdir -p "$HOME/Library/Caches/marspot/binaries/pending"
cp "$CORE_BIN" "$HOME/Library/Caches/marspot/binaries/pending/marspot-core"
[[ -f "$HOME/Library/Caches/marspot/binaries/pending/marspot-core" ]] \
  || fail "staging: pending/marspot-core didn't land"
echo "[2/5] stage OK — pending/marspot-core in place"

# --- 3. Trigger -----------------------------------------------------
"$SHELL_BIN" --trigger >/dev/null
# Wait for the SIGUSR1 entry to log.
for _ in $(seq 1 50); do
  if grep -q SIGUSR1 "$SUP_LOG"; then break; fi
  sleep 0.1
done
grep -q SIGUSR1 "$SUP_LOG" || fail "trigger: no SIGUSR1 entry"
grep -q UPDATE_APPLY "$SUP_LOG" || fail "trigger: no UPDATE_APPLY"
echo "[3/5] trigger OK — SIGUSR1 → UPDATE_APPLY"

# --- 4. Probation ---------------------------------------------------
# Wait for a CORE_SPAWN from binaries/current/ (the new exec).
for _ in $(seq 1 30); do
  if grep -q "CORE_SPAWN.*binaries/current/marspot-core" "$SUP_LOG"; then break; fi
  sleep 0.1
done
grep -q "CORE_SPAWN.*binaries/current/marspot-core" "$SUP_LOG" \
  || fail "probation: new core never spawned from binaries/current"
# Wait for the fresh HelloAck (count goes from 1 to 2).
for _ in $(seq 1 50); do
  ACKS_AFTER=$(grep -c HELLO_ACK "$SUP_LOG")
  if (( ACKS_AFTER >= 2 )); then break; fi
  sleep 0.1
done
(( ACKS_AFTER >= 2 )) || fail "probation: new core never HelloAck'd"
NEW_CORE_PID=$(pgrep -f "$HOME/Library/Caches/marspot/binaries/current/marspot-core" | head -1)
[[ -n "$NEW_CORE_PID" ]] || fail "probation: pgrep didn't find the new core process"
echo "[4/5] probation OK — new core pid=$NEW_CORE_PID, HelloAck'd"

# --- 5. Stable ------------------------------------------------------
# Probation is 30 s; wait up to 45 with a hard upper bound.
START=$(date +%s)
until grep -q UPDATE_STABLE "$SUP_LOG" 2>/dev/null; do
  if (( $(date +%s) - START > 45 )); then
    fail "stable: UPDATE_STABLE never logged within 45 s"
  fi
  sleep 1
done
[[ ! -f "$HOME/Library/Caches/marspot/binaries/prev/marspot-core" ]] \
  || fail "stable: prev/ still has marspot-core (finalize_stable didn't run?)"
[[ ! -f "$HOME/Library/Caches/marspot/binaries/pending/marspot-core" ]] \
  || fail "stable: pending/ still has marspot-core (promote didn't consume?)"
echo "[5/5] stable OK — UPDATE_STABLE logged, prev/ + pending/ empty"

cleanup
echo "ALL PASS"

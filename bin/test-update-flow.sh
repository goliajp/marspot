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
source "$ROOT/bin/_dev-sandbox.sh"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SUP_LOG="$MARSPOT_STATE_DIR/logs/supervisor.log"
TREE="$MARSPOT_STATE_DIR/binaries"
RUN_LOG=/tmp/marspot-test-update-flow.log

fail() {
  echo "FAIL: $*"
  echo "  (last 30 supervisor events):"
  tail -30 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() {
  dev_kill_shell_core
}
trap cleanup EXIT

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" ]]; then
  ( cd "$ROOT" && cargo build --release --bin marspot-shell --bin marspot-core 2>&1 | tail -3 )
fi

dev_ensure_shelld || { echo "FAIL: sandbox shelld"; exit 1; }
cleanup
dev_wipe_state
mkdir -p "$(dirname "$SUP_LOG")"
> "$SUP_LOG" 2>/dev/null
nohup "$SHELL_BIN" >"$RUN_LOG" 2>&1 < /dev/null &
disown

# --- 1. Boot --------------------------------------------------------
for _ in $(seq 1 50); do
  if grep -q HELLO_ACK "$SUP_LOG" 2>/dev/null; then break; fi
  sleep 0.1
done
grep -q HELLO_ACK "$SUP_LOG" 2>/dev/null || fail "boot HelloAck never landed"
PRE_CORE_PID=$(pgrep -f "$CORE_BIN( |$)" | head -1)
[[ -n "$PRE_CORE_PID" ]] || fail "boot: no core pid"
echo "[1/5] boot OK — core pid=$PRE_CORE_PID, HelloAck logged"

# --- 2. Stage -------------------------------------------------------
mkdir -p "$TREE/pending"
cp "$CORE_BIN" "$TREE/pending/marspot-core"
[[ -f "$TREE/pending/marspot-core" ]] \
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
# The pending core spawns from binaries/current/ (after promote) and
# proves itself via PENDING_HELLO_ACK + PENDING_SURFACE_READY — the two
# gates that, plus probation, authorise the swap.  The active core is
# untouched throughout.
for _ in $(seq 1 30); do
  if grep -q "CORE_SPAWN.*binaries/current/marspot-core" "$SUP_LOG"; then break; fi
  sleep 0.1
done
grep -q "CORE_SPAWN.*binaries/current/marspot-core" "$SUP_LOG" \
  || fail "probation: pending core never spawned from binaries/current"
for _ in $(seq 1 50); do
  if grep -q PENDING_SURFACE_READY "$SUP_LOG"; then break; fi
  sleep 0.1
done
grep -q PENDING_HELLO_ACK "$SUP_LOG" || fail "probation: pending core never HelloAck'd"
grep -q PENDING_SURFACE_READY "$SUP_LOG" || fail "probation: pending core never SurfaceReady'd"
NEW_CORE_PID=$(pgrep -f "$TREE/current/marspot-core( |$)" | head -1)
[[ -n "$NEW_CORE_PID" ]] || fail "probation: pgrep didn't find the pending core process"
echo "[4/5] probation OK — pending core pid=$NEW_CORE_PID, HelloAck'd + SurfaceReady'd"

# --- 5. Stable ------------------------------------------------------
# Probation is 30 s; wait up to 45 with a hard upper bound.  Success is
# the atomic swap (UPDATE_SWAP) + finalize (UPDATE_STABLE).
START=$(date +%s)
until grep -q UPDATE_STABLE "$SUP_LOG" 2>/dev/null; do
  if (( $(date +%s) - START > 45 )); then
    fail "stable: UPDATE_STABLE never logged within 45 s"
  fi
  sleep 1
done
grep -q UPDATE_SWAP "$SUP_LOG" || fail "stable: UPDATE_SWAP (presenter swap) not logged"
[[ ! -f "$TREE/prev/marspot-core" ]] \
  || fail "stable: prev/ still has marspot-core (finalize_stable didn't run?)"
[[ ! -f "$TREE/pending/marspot-core" ]] \
  || fail "stable: pending/ still has marspot-core (promote didn't consume?)"
echo "[5/5] stable OK — UPDATE_SWAP + UPDATE_STABLE, prev/ + pending/ empty"

cleanup
echo "ALL PASS"

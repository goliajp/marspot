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
#   3. Trigger — `marspot-shell --trigger` (SIGUSR1) starts a probe of
#      the staged binary on a background thread.
#   4. Swap — the probe passes, the old core is retired and a new one
#      spawns from `binaries/current/`.
#   5. Stable — `prev/` is deleted, the log records UPDATE_STABLE.
#
# Stages 4-5 used to assert the dual-core probation events
# (PENDING_HELLO_ACK / PENDING_SURFACE_READY).  RFC-003 replaced that
# with the single-core in-place swap in 127f3c9 and those events stopped
# existing, which quietly broke this test from 2026-06-17 until
# 2026-07-29 — nothing failed loudly because the tarball builder it
# shares a suite with was broken too.
#
# Run after `cargo build --release`; exits 0 on success.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SUP_LOG="$MARSPOT_STATE_DIR/logs/marspot.log"
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
rm -f "$SUP_LOG" 2>/dev/null || true
nohup "$SHELL_BIN" >"$RUN_LOG" 2>&1 < /dev/null &
disown

# --- 1. Boot --------------------------------------------------------
for _ in $(seq 1 50); do
  if grep -q $'\tHELLO_ACK\t' "$SUP_LOG" 2>/dev/null; then break; fi
  sleep 0.1
done
grep -q $'\tHELLO_ACK\t' "$SUP_LOG" 2>/dev/null || fail "boot HelloAck never landed"
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
  if grep -q $'\tSIGUSR1\t' "$SUP_LOG"; then break; fi
  sleep 0.1
done
grep -q $'\tSIGUSR1\t' "$SUP_LOG" || fail "trigger: no SIGUSR1 entry"
# The swap no longer happens inside the SIGUSR1 tick.  The staged binary
# is exec'd once on a background thread first, both to prove it starts
# and to pay its Gatekeeper assessment while the live core is still
# drawing — so UPDATE_APPLY lands a probe later, not immediately.
# How long the probe takes is macOS's call, not ours: it is a full
# Gatekeeper assessment of a brand-new inode.  Measured 2.9 s on a
# loaded box, 204 s on 2026-07-29 when syspolicyd was being flooded.
# The test waits for the verdict to *arrive*, not for it to be quick.
for _ in $(seq 1 600); do
  if grep -q $'\tUPDATE_PROBE_OK\t' "$SUP_LOG"; then break; fi
  sleep 0.1
done
grep -q $'\tUPDATE_PROBE_START\t' "$SUP_LOG" || fail "trigger: no UPDATE_PROBE_START"
grep -q $'\tUPDATE_PROBE_OK\t' "$SUP_LOG" || fail "trigger: probe never passed within 60 s"
echo "[3/5] trigger OK — SIGUSR1 → probe started → probe passed"

# --- 4. Swap --------------------------------------------------------
# Probe passed, so the supervisor promotes pending → current, retires
# the live core, and spawns the replacement from binaries/current/.
for _ in $(seq 1 100); do
  if grep -q $'\tCORE_SPAWN\t.*binaries/current/marspot-core' "$SUP_LOG"; then break; fi
  sleep 0.1
done
grep -q $'\tUPDATE_APPLY\t' "$SUP_LOG" || fail "swap: no UPDATE_APPLY after a passing probe"
grep -q $'\tACTIVE_SHUTDOWN\t' "$SUP_LOG" || fail "swap: live core was never retired"
grep -q $'\tCORE_SPAWN\t.*binaries/current/marspot-core' "$SUP_LOG" \
  || fail "swap: replacement core never spawned from binaries/current"
NEW_CORE_PID=$(pgrep -f "$TREE/current/marspot-core( |$)" | head -1)
[[ -n "$NEW_CORE_PID" ]] || fail "swap: pgrep didn't find the replacement core process"
echo "[4/5] swap OK — old core retired, replacement pid=$NEW_CORE_PID from current/"

# --- 5. Stable ------------------------------------------------------
# Success is the completed swap (UPDATE_SWAP) + finalize (UPDATE_STABLE).
START=$(date +%s)
until grep -q $'\tUPDATE_STABLE\t' "$SUP_LOG" 2>/dev/null; do
  if (( $(date +%s) - START > 45 )); then
    fail "stable: UPDATE_STABLE never logged within 45 s"
  fi
  sleep 1
done
grep -q $'\tUPDATE_SWAP\t' "$SUP_LOG" || fail "stable: UPDATE_SWAP not logged"
[[ ! -f "$TREE/prev/marspot-core" ]] \
  || fail "stable: prev/ still has marspot-core (finalize_stable didn't run?)"
[[ ! -f "$TREE/pending/marspot-core" ]] \
  || fail "stable: pending/ still has marspot-core (promote didn't consume?)"
echo "[5/5] stable OK — UPDATE_SWAP + UPDATE_STABLE, prev/ + pending/ empty"

cleanup
echo "ALL PASS"

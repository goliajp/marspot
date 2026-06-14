#!/usr/bin/env bash
#
# Dual-core update rollback test.  Stages a binary that exits
# immediately on spawn (so the *pending* core dies inside probation)
# and verifies the supervisor:
#
#   - notices the pending death and aborts (`UPDATE_ABORT`),
#   - restores `prev/marspot-core` into `current/` (`ROLLBACK`),
#   - quarantines the broken binary, and
#   - leaves the live (active) core completely untouched — the whole
#     point of dual-core: a failed update is invisible to the user.
#
# Run after `cargo build --release`.  Exits 0 on success.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SUP_LOG="$MARSPOT_STATE_DIR/logs/marspot.log"
TREE="$MARSPOT_STATE_DIR/binaries"
RUN_LOG=/tmp/marspot-test-rollback.log

fail() {
  echo "FAIL: $*"
  echo "  (last 25 supervisor events):"
  tail -25 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() {
  dev_kill_shell_core
}
trap cleanup EXIT

# The "broken" core we'll stage.  A shell-script wrapper that
# immediately exits non-zero — looks to the supervisor like the new
# core SEGV-ed during startup.
stage_broken() {
  local dst="$1"
  mkdir -p "$(dirname "$dst")"
  cat >"$dst" <<'PYBROKE'
#!/bin/bash
exit 1
PYBROKE
  chmod 0755 "$dst"
  [[ -x "$dst" ]] || fail "stage_broken: $dst not executable after write"
}

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" ]]; then
  ( cd "$ROOT" && cargo build --release --bin marspot-shell --bin marspot-core 2>&1 | tail -3 )
fi

dev_ensure_shelld || fail "sandbox shelld"
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
echo "[1/4] boot OK — HelloAck logged"

# Rollback only restores a previous binary if `prev/` is populated.
# After a fresh install (shell ran from its dev/install sibling),
# `prev/` is empty.  To exercise the realistic production path —
# "a previous-good landed via prior update, *then* a bad update
# fails" — we first do a successful update with the real CORE_BIN
# so the second promote has something to fall back to.

# --- 2. First update (populate prev/ via successful promote) -------
mkdir -p "$TREE/pending"
cp "$CORE_BIN" "$TREE/pending/marspot-core"
"$SHELL_BIN" --trigger >/dev/null
# Wait for UPDATE_STABLE — this is the moment current/ is the new
# binary and prev/ has just been deleted by finalize_stable.  We
# need to actually let probation expire so we have a known-good in
# current/, ready to become prev/ on the next promote.
START=$(date +%s)
until grep -q $'\tUPDATE_STABLE\t' "$SUP_LOG"; do
  if (( $(date +%s) - START > 45 )); then
    fail "first update never reached UPDATE_STABLE"
  fi
  sleep 1
done
# Capture the live (active) core after the swap — this is the process
# that must survive the next, failed update completely untouched.
ACTIVE_PID=$(pgrep -f "$TREE/current/marspot-core( |$)" | head -1)
[[ -n "$ACTIVE_PID" ]] || fail "no active core after first update"
echo "[2/4] first update OK — current/ holds real binary, active pid=$ACTIVE_PID"

# --- 3. Stage broken + trigger rollback -----------------------------
stage_broken "$TREE/pending/marspot-core"
"$SHELL_BIN" --trigger >/dev/null
START=$(date +%s)
until grep -q $'\tROLLBACK\t' "$SUP_LOG"; do
  if (( $(date +%s) - START > 20 )); then
    fail "rollback never logged within 20 s"
  fi
  sleep 0.5
done
grep -q $'\tUPDATE_ABORT\t' "$SUP_LOG" || fail "UPDATE_ABORT not in log"
grep -q $'\tROLLBACK\t.*prev/' "$SUP_LOG" || fail "ROLLBACK (prev/→current/) not in log"
echo "[3/4] rollback OK — UPDATE_ABORT → ROLLBACK (prev/→current/)"

# --- 4. Verify filesystem + silent rollback -------------------------
[[ -f "$TREE/current/marspot-core" ]] \
  || fail "current/ has no marspot-core after rollback"
[[ -f "$TREE/quarantine/marspot-core" ]] \
  || fail "quarantine/marspot-core missing — broken binary not preserved"
if ! cmp -s "$CORE_BIN" "$TREE/current/marspot-core"; then
  fail "current/marspot-core != original CORE_BIN — rollback restored the wrong file"
fi
# The broken update ran as a *pending* core and never touched the live
# one.  Assert the active core from step 2 is still the same process —
# the silent-rollback guarantee: the user saw nothing.
sleep 1
POST_PID=$(pgrep -f "$TREE/current/marspot-core( |$)" | head -1)
[[ -n "$POST_PID" ]] || fail "active core gone after rollback (should be untouched)"
[[ "$POST_PID" == "$ACTIVE_PID" ]] \
  || fail "active core was disturbed by the failed update (pid $ACTIVE_PID → $POST_PID)"
echo "[4/4] silent rollback OK — active core pid=$ACTIVE_PID never disturbed"

cleanup
echo "ALL PASS"

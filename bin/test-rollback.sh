#!/usr/bin/env bash
#
# Broken-update containment test, on a tree that already has history.
# Does one good update first (so current/ + prev/ are both populated),
# then stages a binary that cannot start and verifies the supervisor:
#
#   - rejects it before anything is retired (`UPDATE_PROBE_REJECT`),
#   - quarantines it so the next trigger won't retry it forever,
#   - leaves current/ byte-identical to the good binary, and
#   - leaves the live core completely untouched — a failed update is
#     invisible to the user.
#
# That last guarantee is the point, and it has outlived two mechanisms.
# It used to come from dual-core probation: the candidate ran as a
# *pending* core beside the live one and was aborted (`UPDATE_ABORT` →
# `ROLLBACK`) if it died.  127f3c9 replaced that with the single-core
# in-place swap and both events stopped existing — which broke this
# test from 2026-06-17 until 2026-07-29.  The guarantee now comes from
# the pre-swap probe instead, one step earlier: a candidate that can't
# start never gets as far as a swap.
#
# `test-adversarial-update.sh` case 3 covers the same rejection on a
# clean tree; what's specific here is that a bad candidate must not
# disturb an already-stable current/ + prev/.
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

# The "broken" core we'll stage: a wrapper that exits non-zero however
# it is invoked, which is what an unsigned binary AMFI kills, a
# wrong-arch build, or a truncated copy all look like from here.
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
until grep -q $'\tUPDATE_PROBE_REJECT\t' "$SUP_LOG"; do
  if (( $(date +%s) - START > 60 )); then
    fail "candidate was never rejected within 60 s"
  fi
  sleep 0.5
done
# Rejection must come *instead of* a swap, not after one.
if grep -q $'\tUPDATE_SWAP\tsingle-core swap complete' "$SUP_LOG"; then
  swaps=$(grep -c $'\tUPDATE_SWAP\tsingle-core swap complete' "$SUP_LOG")
  (( swaps == 1 )) || fail "a second swap ran for the broken candidate ($swaps total)"
fi
echo "[3/4] rejection OK — UPDATE_PROBE_REJECT, no second swap"

# --- 4. Verify filesystem + the silent guarantee --------------------
[[ -f "$TREE/current/marspot-core" ]] \
  || fail "current/ has no marspot-core after the rejected update"
[[ -f "$TREE/quarantine/marspot-core" ]] \
  || fail "quarantine/marspot-core missing — broken binary not preserved"
[[ ! -f "$TREE/pending/marspot-core" ]] \
  || fail "pending/ still holds the broken candidate — the next trigger would retry it"
if ! cmp -s "$CORE_BIN" "$TREE/current/marspot-core"; then
  fail "current/marspot-core != original CORE_BIN — a rejected candidate reached current/"
fi
# The candidate never ran as a core at all.  Assert the live core from
# step 2 is still the same process — the silent guarantee: the user saw
# nothing.
sleep 1
POST_PID=$(pgrep -f "$TREE/current/marspot-core( |$)" | head -1)
[[ -n "$POST_PID" ]] || fail "live core gone after the rejected update (should be untouched)"
[[ "$POST_PID" == "$ACTIVE_PID" ]] \
  || fail "live core was disturbed by the failed update (pid $ACTIVE_PID → $POST_PID)"
echo "[4/4] silent containment OK — live core pid=$ACTIVE_PID never disturbed"

cleanup
echo "ALL PASS"

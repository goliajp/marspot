#!/usr/bin/env bash
#
# Probation-failure / rollback test.  Stages a binary that exits
# immediately on spawn (so the new "core" dies inside probation)
# and verifies the supervisor:
#
#   - notices the death (`PROBATION_FAIL`),
#   - restores `prev/marspot-core` into `current/` (`ROLLBACK`),
#   - quarantines the broken binary, and
#   - respawns from the rolled-back binary (fresh HELLO_ACK).
#
# Run after `cargo build --release`.  Exits 0 on success.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SUP_LOG="$HOME/Library/Logs/Marspot/supervisor.log"
RUN_LOG=/tmp/marspot-test-rollback.log

fail() {
  echo "FAIL: $*"
  echo "  (last 25 supervisor events):"
  tail -25 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() {
  pkill -9 -f '/marspot-shell( |$)' >/dev/null 2>&1 || true
  pkill -9 -f '/marspot-core( |$)'  >/dev/null 2>&1 || true
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
echo "[1/4] boot OK — HelloAck logged"

# Rollback only restores a previous binary if `prev/` is populated.
# After a fresh install (shell ran from its dev/install sibling),
# `prev/` is empty.  To exercise the realistic production path —
# "a previous-good landed via prior update, *then* a bad update
# fails" — we first do a successful update with the real CORE_BIN
# so the second promote has something to fall back to.

# --- 2. First update (populate prev/ via successful promote) -------
mkdir -p "$HOME/Library/Caches/marspot/binaries/pending"
cp "$CORE_BIN" "$HOME/Library/Caches/marspot/binaries/pending/marspot-core"
"$SHELL_BIN" --trigger >/dev/null
# Wait for UPDATE_STABLE — this is the moment current/ is the new
# binary and prev/ has just been deleted by finalize_stable.  We
# need to actually let probation expire so we have a known-good in
# current/, ready to become prev/ on the next promote.
START=$(date +%s)
until grep -q UPDATE_STABLE "$SUP_LOG"; do
  if (( $(date +%s) - START > 45 )); then
    fail "first update never reached UPDATE_STABLE"
  fi
  sleep 1
done
echo "[2/4] first update OK — current/ holds real binary"

# --- 3. Stage broken + trigger rollback -----------------------------
stage_broken "$HOME/Library/Caches/marspot/binaries/pending/marspot-core"
"$SHELL_BIN" --trigger >/dev/null
START=$(date +%s)
until grep -q "ROLLBACK\b" "$SUP_LOG"; do
  if (( $(date +%s) - START > 20 )); then
    fail "rollback never logged within 20 s"
  fi
  sleep 0.5
done
grep -q PROBATION_FAIL "$SUP_LOG" || fail "PROBATION_FAIL not in log"
grep -q "ROLLBACK\b.*prev/" "$SUP_LOG" || fail "ROLLBACK (prev/→current/) not in log"
echo "[3/4] rollback OK — PROBATION_FAIL → ROLLBACK (prev/→current/)"

# --- 4. Verify filesystem + re-spawn --------------------------------
[[ -f "$HOME/Library/Caches/marspot/binaries/current/marspot-core" ]] \
  || fail "current/ has no marspot-core after rollback"
[[ -f "$HOME/Library/Caches/marspot/binaries/quarantine/marspot-core" ]] \
  || fail "quarantine/marspot-core missing — broken binary not preserved"
if ! cmp -s "$CORE_BIN" "$HOME/Library/Caches/marspot/binaries/current/marspot-core"; then
  fail "current/marspot-core != original CORE_BIN — rollback restored the wrong file"
fi
# Wait for post-rollback HelloAck.
# Earlier handshakes: boot + first-update + post-rollback = 3.
for _ in $(seq 1 50); do
  acks=$(grep -c HELLO_ACK "$SUP_LOG")
  if (( acks >= 3 )); then break; fi
  sleep 0.1
done
(( acks >= 3 )) || fail "post-rollback core never HelloAck'd (acks=$acks)"
echo "[4/4] respawn OK — rolled-back core spawned, HelloAck'd"

cleanup
echo "ALL PASS"

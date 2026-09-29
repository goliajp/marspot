#!/usr/bin/env bash
#
# Update-cycle soak — the "cannot get slower / heavier the longer it
# runs" architecture commitment, applied to the silent-update path.
# Drives the supervisor through N back-to-back swap-and-stabilise
# cycles and asserts nothing accumulates:
#
#   - exactly ONE live core after each cycle (old cores reaped, no
#     zombie pile-up)
#   - pending/ and prev/ empty after every stabilise (promote + finalize
#     consumed them)
#   - quarantine/ never grows (happy swaps never quarantine)
#   - shell RSS stays bounded across all N cycles (no per-swap leak in
#     the supervisor's CoreConn / PendingUpdate bookkeeping)
#   - the shelld session is never lost (each new core reattaches and
#     HelloAcks — a swap must not SIGHUP sessions)
#
# Uses a short probation (MARSPOT_PROBATION_S=3) so 20 cycles run in
# ~1.5 min instead of ~12.  The faked "new" core is bit-identical to
# the current build — the swap machinery doesn't care, it just needs the
# slot populated.  Sandbox-only; never touches the installed app.
#
# Env: CYCLES (default 20), RSS_GROWTH_PCT (max % shell-RSS growth
# tolerated end-to-end, default 25).  Run after `cargo build --release`.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SUP_LOG="$MARSPOT_STATE_DIR/logs/marspot.log"
TREE="$MARSPOT_STATE_DIR/binaries"
RUN_LOG=/tmp/marspot-soak-update-cycle.log
CYCLES="${CYCLES:-20}"
RSS_GROWTH_PCT="${RSS_GROWTH_PCT:-25}"

export MARSPOT_PROBATION_S=3

fail() {
  echo "FAIL: $*"
  echo "  (last 25 supervisor events):"
  tail -25 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}
cleanup() { dev_kill_shell_core; }
trap cleanup EXIT

[[ -x "$SHELL_BIN" && -x "$CORE_BIN" ]] \
  || ( cd "$ROOT" && cargo build --release --bin marspot-shell --bin marspot-core 2>&1 | tail -3 )

dev_ensure_shelld || { echo "FAIL: sandbox shelld"; exit 1; }
cleanup
dev_wipe_state
mkdir -p "$(dirname "$SUP_LOG")"
rm -f "$SUP_LOG" 2>/dev/null || true
nohup "$SHELL_BIN" >"$RUN_LOG" 2>&1 < /dev/null &
disown

# --- boot ------------------------------------------------------------
for _ in $(seq 1 80); do
  grep -q $'\tHELLO_ACK\t' "$SUP_LOG" 2>/dev/null && break
  sleep 0.1
done
grep -q $'\tHELLO_ACK\t' "$SUP_LOG" 2>/dev/null || fail "boot HelloAck never landed"
SHELL_PID=$(cat "$MARSPOT_STATE_DIR/shell.pid" 2>/dev/null \
            || pgrep -f "$SHELL_BIN( |$)" | head -1)
[[ -n "$SHELL_PID" ]] || fail "no shell pid"

rss_of() { ps -o rss= -p "$1" 2>/dev/null | tr -d ' '; }
core_count() { pgrep -f "$TREE/current/marspot-core( |$)" 2>/dev/null | wc -l | tr -d ' '; }
# grep -c already prints 0 on no match (and exits 1, which the command
# substitution swallows) — an `|| echo 0` would double the output.
stable_count() { grep -c $'\tUPDATE_STABLE\t' "$SUP_LOG" 2>/dev/null; }
quarantine_count() { ls "$TREE/quarantine" 2>/dev/null | wc -l | tr -d ' '; }

RSS_START=$(rss_of "$SHELL_PID")
[[ -n "$RSS_START" ]] || fail "could not read shell RSS"
echo "[boot] shell pid=$SHELL_PID RSS=${RSS_START}KiB — running $CYCLES update cycles (probation ${MARSPOT_PROBATION_S}s)"

# --- N cycles --------------------------------------------------------
for c in $(seq 1 "$CYCLES"); do
  before=$(stable_count)
  mkdir -p "$TREE/pending"
  cp "$CORE_BIN" "$TREE/pending/marspot-core"
  "$SHELL_BIN" --trigger >/dev/null 2>&1

  # Wait for this cycle's UPDATE_STABLE.
  START=$(date +%s)
  until (( $(stable_count) > before )); do
    if (( $(date +%s) - START > 30 )); then
      fail "cycle $c: UPDATE_STABLE never logged (probation/swap stalled)"
    fi
    sleep 0.3
  done

  # Invariants after this cycle.
  [[ ! -f "$TREE/pending/marspot-core" ]] || fail "cycle $c: pending/ not consumed"
  [[ ! -f "$TREE/prev/marspot-core" ]]    || fail "cycle $c: prev/ not finalized"
  n=$(core_count)
  [[ "$n" == "1" ]] || fail "cycle $c: expected exactly 1 live core, found $n (zombie/leak)"
  q=$(quarantine_count)
  [[ "$q" == "0" ]] || fail "cycle $c: quarantine grew to $q on a happy swap (should never quarantine)"
  # Shell pid must be stable — a core swap never restarts the supervisor.
  kill -0 "$SHELL_PID" 2>/dev/null || fail "cycle $c: shell pid $SHELL_PID died"

  if (( c % 5 == 0 )); then
    echo "  [cycle $c/$CYCLES] core reattached+stable, tree clean, RSS=$(rss_of "$SHELL_PID")KiB"
  fi
done

# --- final boundedness checks ---------------------------------------
RSS_END=$(rss_of "$SHELL_PID")
echo "[done] $CYCLES cycles — shell RSS ${RSS_START} → ${RSS_END} KiB"

# Session never lost: the shell logged a fresh PENDING_HELLO_ACK every
# cycle (each promoted core reattached), and the shell is still up.
acks=$(grep -c $'\tPENDING_HELLO_ACK\t' "$SUP_LOG" 2>/dev/null)
(( acks >= CYCLES )) || fail "only $acks PENDING_HELLO_ACK for $CYCLES cycles — a swap lost its session?"

# RSS growth bounded.  Allow RSS_GROWTH_PCT% end-to-end (steady-state
# allocator slack, not a per-cycle leak).
max_rss=$(( RSS_START + RSS_START * RSS_GROWTH_PCT / 100 ))
(( RSS_END <= max_rss )) \
  || fail "shell RSS grew ${RSS_START}→${RSS_END} KiB (> ${RSS_GROWTH_PCT}% cap ${max_rss}) — per-swap leak"

# No core zombies, tree clean.
[[ "$(core_count)" == "1" ]] || fail "more than one core alive at end"
[[ "$(quarantine_count)" == "0" ]] || fail "quarantine non-empty at end"

cleanup
trap - EXIT
echo "ALL PASS — $CYCLES update cycles, 1 core throughout, tree clean, RSS bounded, sessions kept"

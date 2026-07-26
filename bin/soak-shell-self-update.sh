#!/usr/bin/env bash
#
# Shell self-update soak — the L1 `execv` path, which had NO coverage
# until it took the app down twice on 2026-07-26.
#
# `soak-update-cycle.sh` drives *core* swaps.  The shell replacing its
# own image is a different machine and a far less forgiving one:
#
#   - the successor inherits the pid, so a failure looks like the app
#     simply vanishing;
#   - the redirect at the top of `main()` re-execs into
#     `current/marspot-shell` BEFORE `logx::init`, so a successor that
#     cannot start leaves *no* log line, no panic, no crash report.
#     That combination is why the outage was invisible.
#
# What this asserts, per cycle:
#   - the shell survives its own execv (pid preserved, process alive)
#   - the successor logs SHELL_SELF_UPDATE, i.e. it got past the
#     redirect and past logx::init
#   - it re-spawns a core and reaches HELLO_ACK (the window is usable
#     again, not just the process alive)
#   - pending/ is consumed and quarantine/ never grows
#
# It also runs one **negative** cycle: an unsigned (adhoc) shell staged
# into pending/ must NOT take the supervisor down.  That is the exact
# shape of the 2026-07-26 outage, and the assertion that would have
# caught it.
#
# Sandbox-only; never touches the installed app.
#
# Env: CYCLES (default 5).  Run after `cargo build --release`.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SUP_LOG="$MARSPOT_STATE_DIR/logs/marspot.log"
TREE="$MARSPOT_STATE_DIR/binaries"
RUN_LOG=/tmp/marspot-soak-shell-self-update.log
CYCLES="${CYCLES:-5}"

export MARSPOT_PROBATION_S=3

fail() {
  echo "FAIL: $*"
  echo "  (last 25 supervisor events):"
  tail -25 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'
  echo "  (last 15 lines of the shell's own stdio):"
  tail -15 "$RUN_LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}
cleanup() { dev_kill_shell_core; }
trap cleanup EXIT

[[ -x "$SHELL_BIN" && -x "$CORE_BIN" ]] \
  || ( cd "$ROOT" && cargo build --release --bin marspot-shell --bin marspot-core 2>&1 | tail -3 )

count_of() { grep -c "$1" "$SUP_LOG" 2>/dev/null; }
shell_alive() { kill -0 "$1" 2>/dev/null; }
quarantine_count() { ls "$TREE/quarantine" 2>/dev/null | wc -l | tr -d ' '; }
pending_count() { ls "$TREE/pending" 2>/dev/null | wc -l | tr -d ' '; }

wait_for() {  # wait_for <pattern> <count> <timeout_tenths>
  local pat="$1" want="$2" tries="$3"
  for _ in $(seq 1 "$tries"); do
    [[ "$(count_of "$pat")" -ge "$want" ]] && return 0
    sleep 0.1
  done
  return 1
}

# --- boot ------------------------------------------------------------
cleanup
dev_wipe_state
# Explicitly: a `pending/` left over from a previous run (the negative
# cycle below deliberately puts a broken binary there) would be
# promoted during THIS run's boot and take the supervisor down before
# the first cycle — a harness artifact that reads exactly like the bug
# under test.
rm -rf "$TREE"
mkdir -p "$(dirname "$SUP_LOG")" "$TREE/current" "$TREE/pending"
rm -f "$SUP_LOG" 2>/dev/null || true
# The redirect means the shell we launch re-execs into current/ — so
# current/ has to hold a real binary before anything starts.
install -m 755 "$SHELL_BIN" "$TREE/current/marspot-shell"
install -m 755 "$CORE_BIN" "$TREE/current/marspot-core"

nohup "$SHELL_BIN" >"$RUN_LOG" 2>&1 < /dev/null &
disown

wait_for $'\tHELLO_ACK\t' 1 100 || fail "boot HelloAck never landed"
SHELL_PID=$(cat "$MARSPOT_STATE_DIR/shell.pid" 2>/dev/null \
            || pgrep -f "marspot-shell( |$)" | head -1)
[[ -n "$SHELL_PID" ]] || fail "no shell pid after boot"
echo "[boot] shell pid=$SHELL_PID — $CYCLES self-update cycles"

# --- positive cycles -------------------------------------------------
for i in $(seq 1 "$CYCLES"); do
  before_self=$(count_of $'\tSHELL_SELF_UPDATE\t')
  before_ack=$(count_of $'\tHELLO_ACK\t')

  install -m 755 "$SHELL_BIN" "$TREE/pending/marspot-shell"
  kill -USR1 "$SHELL_PID" 2>/dev/null || fail "cycle $i: SIGUSR1 to $SHELL_PID failed"

  # The successor keeps the pid, so "alive" is the first thing to check
  # — a dead pid here IS the outage this soak exists for.
  wait_for $'\tSHELL_SELF_UPDATE\t' $((before_self + 1)) 150 \
    || fail "cycle $i: successor never logged SHELL_SELF_UPDATE (died before logx::init?)"
  shell_alive "$SHELL_PID" \
    || fail "cycle $i: shell pid $SHELL_PID gone after execv"
  wait_for $'\tHELLO_ACK\t' $((before_ack + 1)) 200 \
    || fail "cycle $i: successor never got a core to HelloAck"

  [[ "$(pending_count)" -eq 0 ]] || fail "cycle $i: pending/ not consumed"
  [[ "$(quarantine_count)" -eq 0 ]] || fail "cycle $i: quarantine/ grew on a healthy swap"
  echo "  cycle $i ok (pid preserved, core reattached)"
done

# --- negative cycle: an unsigned successor must not kill the app -----
#
# 2026-07-26: `install-local.sh --no-build` skipped code signing, so an
# adhoc-signed shell landed in pending/.  The redirect exec'd into it,
# AMFI killed the process, and the whole app went down with sixteen live
# sessions behind it — no log line anywhere.  The supervisor must treat
# a successor that cannot start as a failed update, not as a way to die.
echo "[negative] staging a deliberately broken successor"
before_ack=$(count_of $'\tHELLO_ACK\t')
printf '#!/bin/sh\nexit 9\n' > "$TREE/pending/marspot-shell"
chmod 755 "$TREE/pending/marspot-shell"
kill -USR1 "$SHELL_PID" 2>/dev/null || true
sleep 3

if shell_alive "$SHELL_PID"; then
  echo "  supervisor survived a broken successor (pid $SHELL_PID)"
else
  fail "a broken successor took the supervisor down — this is the \
2026-07-26 outage shape.  The self-update path must verify the \
successor can start (or roll back) before exec'ing into it."
fi

# Never leave the broken successor on disk — see the wipe note above.
rm -f "$TREE/pending/marspot-shell"

echo "PASS: $CYCLES self-update cycles + broken-successor survival"

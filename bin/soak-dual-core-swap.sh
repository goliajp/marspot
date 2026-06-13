#!/usr/bin/env bash
#
# Dual-core swap soak.  The flash-free silent update holds *two* cores
# (2× Metal) for the ~30 s probation, then swaps the presenter and
# retires the old one.  This soak proves the transient is genuinely
# transient — that each swap fully reaps the old core + releases its
# IOSurface, so RSS returns to baseline and nothing accumulates across
# repeated updates.
#
# For ITERATIONS swaps it asserts:
#   - during probation exactly TWO sandbox cores exist (2× Metal),
#   - after UPDATE_SWAP exactly ONE remains (old core fully reaped),
#   - shell RSS after each swap stays within baseline + SHELL_GROWTH_KIB
#     (no per-swap leak of textures / surfaces / sockets),
#   - the shell PID never changes (window owner is stable).
#
# Probation is a hard 30 s in the binary, so each swap takes ~35 s.
# Defaults to 3 iterations (~2 min).  Run after `cargo build --release`.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SUP_LOG="$MARSPOT_STATE_DIR/logs/supervisor.log"
TREE="$MARSPOT_STATE_DIR/binaries"
RUN_LOG=/tmp/marspot-soak-dual-core.log
RSS_LOG=/tmp/marspot-soak-dual-core.rss.tsv

ITERATIONS="${ITERATIONS:-3}"
# A single core (Metal + AppKit + CoreText) sits ~80 MiB.  Allow the
# shell a generous per-run growth budget; a *leak* shows up as
# monotonic growth proportional to ITERATIONS, which this catches.
SHELL_GROWTH_KIB="${SHELL_GROWTH_KIB:-40960}" # 40 MiB total over the run

fail() {
  echo "FAIL: $*"
  echo "  RSS samples:"
  cat "$RSS_LOG" 2>/dev/null | sed 's/^/    /'
  echo "  (last 30 supervisor events):"
  tail -30 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() { dev_kill_shell_core; }
trap cleanup EXIT

# Count sandbox cores only (sibling boot path + promoted current/
# path) so a separately-installed Marspot.app can't pollute the count.
count_cores() {
  { pgrep -f "$CORE_BIN" 2>/dev/null
    pgrep -f "$TREE/current/marspot-core" 2>/dev/null
  } | sort -u | grep -c . || true
}

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" ]]; then
  ( cd "$ROOT" && cargo build --release --bin marspot-shell --bin marspot-core 2>&1 | tail -3 )
fi

dev_ensure_shelld || fail "sandbox shelld"
cleanup
dev_wipe_state
mkdir -p "$(dirname "$SUP_LOG")"
> "$SUP_LOG" 2>/dev/null
echo -e "phase\tt_s\tshell_rss_kib\tcores" > "$RSS_LOG"

nohup "$SHELL_BIN" >"$RUN_LOG" 2>&1 < /dev/null &
disown

# --- Boot -----------------------------------------------------------
for _ in $(seq 1 50); do
  grep -q HELLO_ACK "$SUP_LOG" 2>/dev/null && break
  sleep 0.1
done
grep -q HELLO_ACK "$SUP_LOG" 2>/dev/null || fail "boot HelloAck never landed"
SHELL_PID=$(pgrep -f "$SHELL_BIN" | head -1)
[[ -n "$SHELL_PID" ]] || fail "shell not running after boot"
sleep 1
BASE_RSS=$(ps -p "$SHELL_PID" -o rss= 2>/dev/null | tr -d ' ')
[[ -n "$BASE_RSS" ]] || fail "couldn't read baseline shell RSS"
echo -e "boot\t0\t${BASE_RSS}\t$(count_cores)" >> "$RSS_LOG"
echo "boot OK — shell pid=$SHELL_PID baseline RSS=${BASE_RSS} KiB, cores=$(count_cores)"

start=$(date +%s)
for i in $(seq 1 "$ITERATIONS"); do
  echo "--- swap $i/$ITERATIONS ---"
  prev_stable=$(grep -c UPDATE_STABLE "$SUP_LOG")

  # Stage an identical "new" binary + trigger.
  mkdir -p "$TREE/pending"
  cp "$CORE_BIN" "$TREE/pending/marspot-core"
  "$SHELL_BIN" --trigger >/dev/null

  # Wait for the pending core to spawn, then confirm 2 cores coexist.
  for _ in $(seq 1 50); do
    (( $(count_cores) >= 2 )) && break
    sleep 0.1
  done
  cores_mid=$(count_cores)
  t=$(( $(date +%s) - start ))
  mid_rss=$(ps -p "$SHELL_PID" -o rss= 2>/dev/null | tr -d ' ')
  echo -e "probation\t${t}\t${mid_rss:-0}\t${cores_mid}" >> "$RSS_LOG"
  (( cores_mid == 2 )) \
    || fail "swap $i: expected 2 cores during probation, saw ${cores_mid}"
  echo "  probation OK — 2 cores (2× Metal), shell RSS=${mid_rss} KiB"

  # Wait for the swap to finalise (UPDATE_STABLE count increments).
  sw_start=$(date +%s)
  until (( $(grep -c UPDATE_STABLE "$SUP_LOG") > prev_stable )); do
    if (( $(date +%s) - sw_start > 45 )); then
      fail "swap $i: UPDATE_STABLE never incremented within 45 s"
    fi
    sleep 1
  done
  grep -q UPDATE_SWAP "$SUP_LOG" || fail "swap $i: UPDATE_SWAP not logged"

  # Old core must be fully reaped — back to exactly one.
  for _ in $(seq 1 30); do
    (( $(count_cores) == 1 )) && break
    sleep 0.1
  done
  cores_after=$(count_cores)
  t=$(( $(date +%s) - start ))
  post_rss=$(ps -p "$SHELL_PID" -o rss= 2>/dev/null | tr -d ' ')
  echo -e "post-swap\t${t}\t${post_rss:-0}\t${cores_after}" >> "$RSS_LOG"
  (( cores_after == 1 )) \
    || fail "swap $i: expected 1 core after swap, saw ${cores_after} (old core leaked?)"

  # Shell PID must be unchanged (it owns the window).
  now_shell=$(pgrep -f "$SHELL_BIN" | head -1)
  [[ "$now_shell" == "$SHELL_PID" ]] \
    || fail "swap $i: shell pid changed ($SHELL_PID → ${now_shell:-none})"

  # No cumulative leak: shell RSS within baseline + growth budget.
  if (( post_rss > BASE_RSS + SHELL_GROWTH_KIB )); then
    fail "swap $i: shell RSS ${post_rss} KiB > baseline ${BASE_RSS} + ${SHELL_GROWTH_KIB}"
  fi
  echo "  swap OK — 1 core, shell RSS=${post_rss} KiB (baseline ${BASE_RSS})"
done

FINAL_RSS=$(ps -p "$SHELL_PID" -o rss= 2>/dev/null | tr -d ' ')
echo "PASS — ${ITERATIONS} swaps, shell pid stable, RSS ${BASE_RSS}→${FINAL_RSS} KiB, steady-state 1 core"
echo "RSS samples: $RSS_LOG"

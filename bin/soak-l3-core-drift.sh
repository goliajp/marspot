#!/usr/bin/env bash
#
# Core-side sustained-load RSS-drift gate (perf-attack A1, renderer half).
#
# The per-session half of A1 is covered by bin/soak-l3-drift.sh (the L3
# engine).  This covers the OTHER half under the L3 architecture: with
# shell→core→9 L3 running for real, flood all 9 sessions and watch
# **marspot-core's** RSS (the renderer: render scratch high-water-mark,
# Metal command-buffer / autorelease-pool drain, 9 shm readers + mirror
# grids).  Asserts core RSS plateaus (drift q4/q1 ≤ 1.10) under sustained
# 9-session output — the "cannot get slower the longer it runs" commitment
# for the renderer.
#
# Launches a real shell→core→L3 tree in the dev sandbox (a window appears;
# it is the sandbox app, never the installed one).  Tears it down after.
#
# Usage:
#   bin/soak-l3-core-drift.sh                 # default 180 s
#   DURATION_S=300 bin/soak-l3-core-drift.sh  # A1 gate window

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

SHELL_BIN="$DEV_TARGET/marspot-shell"
CORE_BIN="$DEV_TARGET/marspot-core"
FLOOD_BIN="$DEV_TARGET/examples/flood_sessions"
DURATION_S="${DURATION_S:-180}"
INTERVAL_S="${INTERVAL_S:-10}"
RUN_LOG=/tmp/marspot-core-drift.log

fail() { echo "FAIL: $*"; teardown; exit 1; }

teardown() {
  pkill -9 -f "$DEV_TARGET/marspot-shell( |$)" >/dev/null 2>&1 || true
  pkill -9 -f "$DEV_TARGET/marspot-core( |$)"  >/dev/null 2>&1 || true
  pkill -9 -f "$DEV_TARGET/marspot-session( |$)" >/dev/null 2>&1 || true
}
trap teardown EXIT

( cd "$ROOT" && cargo build --release \
    --bin marspot-shell --bin marspot-core --bin marspot-shelld 2>&1 | tail -3 )
( cd "$ROOT" && cargo build --release -p marspot-session \
    --bin marspot-session --example flood_sessions 2>&1 | tail -3 )
for b in "$SHELL_BIN" "$CORE_BIN" "$FLOOD_BIN"; do
  [[ -x "$b" ]] || fail "not built: $b"
done

dev_ensure_shelld || fail "sandbox shelld"
teardown; sleep 0.5

echo "==> launching shell→core→L3 (sandbox)"
: > "$RUN_LOG"
MARSPOT_L3=1 nohup "$SHELL_BIN" >"$RUN_LOG" 2>&1 < /dev/null &

# Wait for the full 9-L3 tree.
i=0
while [[ $(pgrep -f "$DEV_TARGET/marspot-session( |$)" | grep -c .) -lt 9 && $i -lt 600 ]]; do
  i=$((i+1)); for _ in $(seq 1 50000); do :; done
done
n_l3=$(pgrep -f "$DEV_TARGET/marspot-session( |$)" | grep -c .)
core_pid=$(pgrep -f "$DEV_TARGET/marspot-core( |$)" | head -1)
[[ -n "$core_pid" ]] || fail "core not running (boot failed — see $RUN_LOG)"
echo "==> tree up: core pid=$core_pid, ${n_l3} L3 sessions"

echo "==> flooding all sessions"
"$FLOOD_BIN" 2>&1 | sed 's/^/  /'

echo "==> sampling core RSS for ${DURATION_S}s (every ${INTERVAL_S}s)"
samples=()
elapsed=0
while (( elapsed < DURATION_S )); do
  sleep "$INTERVAL_S"; elapsed=$((elapsed + INTERVAL_S))
  rss=$(ps -o rss= -p "$core_pid" 2>/dev/null | tr -d ' ')
  [[ -z "$rss" ]] && fail "core pid=$core_pid vanished mid-soak (crash? see $RUN_LOG)"
  samples+=("$rss")
  echo "  t=${elapsed}s core_rss=${rss} KiB"
done

python3 - "$DURATION_S" "${samples[@]}" <<'PY'
import sys
dur = int(sys.argv[1]); s = [int(x) for x in sys.argv[2:]]
if len(s) < 4:
    print("FAIL: too few samples"); sys.exit(1)
q = len(s)//4
q1 = sum(s[:q])/q; q4 = sum(s[-q:])/q
drift = q4/q1 if q1 else 0
first, mx = s[0], max(s)
abs_growth = mx - first
print(f"core RSS: q1={q1:.0f} q4={q4:.0f} drift={drift:.3f} first={first} max={mx} abs_growth={abs_growth} KiB")
ok = drift <= 1.10 and abs_growth <= 12*1024
print(("PASS" if ok else "FAIL") + f": core RSS drift {drift:.3f} (≤1.10), abs growth {abs_growth} KiB (≤12288) over {dur}s")
sys.exit(0 if ok else 1)
PY
rc=$?
(( rc == 0 )) || fail "core-side RSS drift gate failed"
echo "PASS — marspot-core holds the bounded-memory commitment under ${DURATION_S}s sustained 9-session load"

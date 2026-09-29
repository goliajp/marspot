#!/usr/bin/env bash
#
# Per-session L3 sustained-load RSS-drift gate (perf-attack A1, re-scoped
# to the L3 architecture that is now the default).
#
# A1 ("active-9x-soak RSS drift", ~10 MiB/min leak) was filed against the
# standalone in-process `marspot`.  The product is now shell→core→L3: the
# parser/terminal/grid/scrollback/PTY half — where A1's live leak
# candidates (PTY chunk heap fragmentation, Terminal/Session accumulation,
# scrollback growth) all live — runs in N separate `marspot-session`
# processes.  This drives ONE L3 with continuous output for a sustained
# window and asserts its RSS plateaus (drift q4/q1 ≤ 1.10, the "cannot get
# slower the longer it runs" commitment).  That's the per-session factor
# the real 9-grid product multiplies.
#
# Runs entirely in the dev sandbox; never touches the installed app.
#
# Usage:
#   bin/soak-l3-drift.sh                 # default 300 s (A1 gate window)
#   DURATION_S=60  bin/soak-l3-drift.sh  # quick dev iteration
#   DURATION_S=1800 bin/soak-l3-drift.sh # 30-min extended (release / nightly)

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

SESSION_BIN="$DEV_TARGET/marspot-session"
DRIFT_PROBE_BIN="$DEV_TARGET/examples/l3_drift_probe"
DURATION_S="${DURATION_S:-300}"
INTERVAL_S="${INTERVAL_S:-10}"

fail() { echo "FAIL: $*"; exit 1; }

# Always (re)build bin + probe together — incremental is instant when
# current, and rules out the grid_shm version-skew trap.
( cd "$ROOT" && cargo build --release -p marspot-session --example l3_drift_probe 2>&1 | tail -3 )
[[ -x "$SESSION_BIN" ]]     || fail "marspot-session not built at $SESSION_BIN"
[[ -x "$DRIFT_PROBE_BIN" ]] || fail "drift probe not built at $DRIFT_PROBE_BIN"

# Zero-GUI floor still holds.
if otool -L "$SESSION_BIN" | grep -qiE 'Metal|AppKit|CoreText'; then
  fail "marspot-session links a GUI framework (RSS floor broken)"
fi

dev_ensure_shelld || fail "sandbox shelld"
pkill -9 -f "$DEV_TARGET/marspot-session( |$)" >/dev/null 2>&1 || true

echo "==> L3 sustained-load drift: ${DURATION_S}s (sample ${INTERVAL_S}s)"
"$DRIFT_PROBE_BIN" "$SESSION_BIN" "$DURATION_S" "$INTERVAL_S"
rc=$?

# No orphan left behind.
sleep 0.3
orphans=$(pgrep -f "$DEV_TARGET/marspot-session( |$)" 2>/dev/null | grep -c . || true)
(( orphans == 0 )) || fail "left ${orphans} orphan marspot-session proc(s)"

(( rc == 0 )) || exit "$rc"
echo "PASS — L3 per-session engine holds the bounded-memory commitment under ${DURATION_S}s sustained load"

#!/usr/bin/env bash
#
# Per-session idle RSS scaling gate (perf-attack C, re-scoped onto L3).
#
# C ("per-session RSS bloat vs Terminal.app") was filed against the
# standalone marspot: 9 in-process grids + Metal + a per-cell scrollback
# ring + the 16 MiB atlas idled at +90 MiB for 9 sessions — the same
# lazy-fault-into-mmap-ring trajectory as A1.  A1 was resolved under L3
# (growth candidates moved into separate bounded processes).  This gate
# re-measures C the way the product actually runs: spawn N idle
# `marspot-session` processes and assert the per-session footprint is
# small AND scales linearly (no fixed bloat, no superlinear growth).
#
# A regression here means either an accidental GUI link into L3 (RSS
# floor broken) or unbounded per-session state — exactly the "cannot get
# slower / heavier the longer it runs" commitment, per session.
#
# Runs entirely in the dev sandbox; never touches the installed app.
#
#   bin/soak-l3-rss-scaling.sh

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_dev-sandbox.sh"

SESSION_BIN="$DEV_TARGET/marspot-session"
PROBE_BIN="$DEV_TARGET/examples/l3_rss_scaling"

# Per-session idle L3 RSS cap.  Measured ~1.9 MiB on dev box; cap at 4 MiB
# so a real regression (GUI link → tens of MiB, unbounded buffer) trips it
# without flapping on allocator / page-rounding noise.
PER_SESSION_CAP_KIB="${PER_SESSION_CAP_KIB:-4096}"
# Linearity: per-session RSS at N=9 must not exceed N=1's by >30 % — i.e.
# no shared-state-per-session creep that compounds with pane count.
LINEARITY_MAX="${LINEARITY_MAX:-1.30}"

fail() { echo "FAIL: $*"; exit 1; }

( cd "$ROOT" && cargo build --release -p marspot-session --example l3_rss_scaling 2>&1 | tail -3 )
[[ -x "$SESSION_BIN" ]] || fail "marspot-session not built at $SESSION_BIN"
[[ -x "$PROBE_BIN" ]]   || fail "probe not built at $PROBE_BIN"

# Zero-GUI floor: an accidental Metal/AppKit/CoreText link is the most
# likely way per-session RSS balloons.
if otool -L "$SESSION_BIN" | grep -qiE 'Metal|AppKit|CoreText'; then
  fail "marspot-session links a GUI framework (RSS floor broken)"
fi

dev_ensure_shelld || fail "sandbox shelld did not come up"
cleanup() { pkill -9 -f "$SESSION_BIN( |\$)" >/dev/null 2>&1 || true; }
trap cleanup EXIT

run_n() {
  local n=$1
  pkill -9 -f "$SESSION_BIN( |\$)" >/dev/null 2>&1 || true
  sleep 0.3
  "$PROBE_BIN" "$SESSION_BIN" "$n" 2>/dev/null
}

echo "==> per-session idle L3 RSS scaling (N=1, N=9)"
OUT1=$(run_n 1) || fail "N=1 probe failed"
OUT9=$(run_n 9) || fail "N=9 probe failed"
PER1=$(echo "$OUT1" | awk '{print $3}')
TOT9=$(echo "$OUT9" | awk '{print $2}')
PER9=$(echo "$OUT9" | awk '{print $3}')
[[ -n "$PER1" && -n "$PER9" ]] || fail "could not parse probe output (got '$OUT1' / '$OUT9')"

echo "    N=1 per-session ${PER1} KiB"
echo "    N=9 total ${TOT9} KiB, per-session ${PER9} KiB"

(( PER9 <= PER_SESSION_CAP_KIB )) || \
  fail "per-session idle RSS ${PER9} KiB > cap ${PER_SESSION_CAP_KIB} KiB (GUI link or unbounded per-session state?)"

ratio=$(python3 -c "print(f'{$PER9/$PER1:.3f}')")
ok=$(python3 -c "print(1 if $PER9 <= $PER1 * $LINEARITY_MAX else 0)")
(( ok == 1 )) || \
  fail "per-session RSS grows with pane count: N=9 ${PER9} KiB vs N=1 ${PER1} KiB = ${ratio}× > ${LINEARITY_MAX}× (superlinear per-session state)"

orphans=$(pgrep -f "$SESSION_BIN( |$)" 2>/dev/null | grep -c . || true)
(( orphans == 0 )) || fail "left ${orphans} orphan marspot-session proc(s)"

echo "PASS: per-session idle L3 RSS ${PER9} KiB ≤ ${PER_SESSION_CAP_KIB} KiB, linear (${ratio}× ≤ ${LINEARITY_MAX}×), no orphans"

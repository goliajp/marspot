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
PROBE_BIN="$DEV_TARGET/examples/l3_idle_cost"

# Per-session idle L3 RSS cap.  Measured ~1.9 MiB on dev box; cap at 4 MiB
# so a real regression (GUI link → tens of MiB, unbounded buffer) trips it
# without flapping on allocator / page-rounding noise.
PER_SESSION_CAP_KIB="${PER_SESSION_CAP_KIB:-4096}"
# Linearity: per-session RSS at N=9 must not exceed N=1's by >30 % — i.e.
# no shared-state-per-session creep that compounds with pane count.
LINEARITY_MAX="${LINEARITY_MAX:-1.30}"

fail() { echo "FAIL: $*"; exit 1; }

# The pipe used to swallow this build's exit code.  The example it names
# had not existed since the history rebuild, so the failure was invisible
# and a binary from June sitting in `target/` satisfied the `-x` check
# below -- this gate spent four months either failing with a misleading
# message or, if the wire protocol had happened not to change, measuring
# June's code.
if ! ( cd "$ROOT" && cargo build --release -p marspot-session \
         --example l3_idle_cost 2>&1 | tail -20 ); then
  fail "could not build the probe"
fi
if ! ( cd "$ROOT" && cargo build --release -p marspot-session 2>&1 | tail -20 ); then
  fail "could not build marspot-session"
fi
[[ -x "$SESSION_BIN" ]] || fail "marspot-session not built at $SESSION_BIN"
[[ -x "$PROBE_BIN" ]]   || fail "probe not built at $PROBE_BIN"

# Zero-GUI floor: an accidental Metal/AppKit/CoreText link is the most
# likely way per-session RSS balloons.
if otool -L "$SESSION_BIN" | grep -qiE 'Metal|AppKit|CoreText'; then
  fail "marspot-session links a GUI framework (RSS floor broken)"
fi

# The probe's sessions are its children; dev_run_probe kills any that
# outlive it and returns 3, which fails the run below.
run_n() {
  dev_run_probe "$PROBE_BIN" "$SESSION_BIN" "$1" 2>/dev/null
}

echo "==> per-session idle L3 RSS scaling (N=1, 4, 9, 16)"
# Columns: N total_rss_kib per_rss_kib cpu_secs cpu_pct.  This gate reads
# the two RSS ones; T4 of the public bench reads the CPU ones from the
# same probe with a 60-second window.
# One, four, nine and sixteen: the pane counts a 2x2, 3x3 and 4x4 grid
# actually produce, so the linearity bound is tested across the range the
# product offers rather than at its ends.  N=1 and N=9 alone would miss a
# cost that only appears past a threshold.
OUT1=$(run_n 1) || fail "N=1 probe failed (exit $?; 3: a session outlived it)"
OUT4=$(run_n 4) || fail "N=4 probe failed (exit $?; 3: a session outlived it)"
OUT9=$(run_n 9) || fail "N=9 probe failed (exit $?; 3: a session outlived it)"
OUT16=$(run_n 16) || fail "N=16 probe failed (exit $?; 3: a session outlived it)"
PER4=$(echo "$OUT4" | awk '{print $3}')
PER16=$(echo "$OUT16" | awk '{print $3}')
PER1=$(echo "$OUT1" | awk '{print $3}')
TOT9=$(echo "$OUT9" | awk '{print $2}')
PER9=$(echo "$OUT9" | awk '{print $3}')
[[ -n "$PER1" && -n "$PER9" ]] || fail "could not parse probe output (got '$OUT1' / '$OUT9')"

echo "    N=1 per-session ${PER1} KiB"
echo "    N=9 total ${TOT9} KiB, per-session ${PER9} KiB"

for n_per in "4:$PER4" "16:$PER16"; do
  n=${n_per%%:*}; per=${n_per##*:}
  [[ -n "$per" ]] || fail "could not parse the N=$n probe output"
  echo "    N=$n per-session ${per} KiB"
  (( per <= PER_SESSION_CAP_KIB )) || \
    fail "per-session idle RSS at N=$n is ${per} KiB > cap ${PER_SESSION_CAP_KIB} KiB"
  ok=$(python3 -c "print(1 if $per <= $PER1 * $LINEARITY_MAX else 0)")
  (( ok == 1 )) || \
    fail "per-session RSS grows with pane count: N=$n ${per} KiB vs N=1 ${PER1} KiB > ${LINEARITY_MAX}x"
done

(( PER9 <= PER_SESSION_CAP_KIB )) || \
  fail "per-session idle RSS ${PER9} KiB > cap ${PER_SESSION_CAP_KIB} KiB (GUI link or unbounded per-session state?)"

ratio=$(python3 -c "print(f'{$PER9/$PER1:.3f}')")
ok=$(python3 -c "print(1 if $PER9 <= $PER1 * $LINEARITY_MAX else 0)")
(( ok == 1 )) || \
  fail "per-session RSS grows with pane count: N=9 ${PER9} KiB vs N=1 ${PER1} KiB = ${ratio}× > ${LINEARITY_MAX}× (superlinear per-session state)"

echo "PASS: per-session idle L3 RSS ${PER9} KiB ≤ ${PER_SESSION_CAP_KIB} KiB, linear (${ratio}× ≤ ${LINEARITY_MAX}×), no orphans"


# Shadow panes (sessions with no window) in a sandbox app: their L3s are
# held to the same per-session cap, and the core's resident size may grow
# by at most SHADOW_CORE_KIB_CAP per shadow pane.  The core maps each
# shadow's frame region and keeps a `Pane` for it, and draws nothing.
# Opens a window, so this part runs on the runner:
#   bin/test-e2e-remote.sh soak-l3-rss-scaling.sh
# Measured on mini (2026-11-23, 8 shadow panes against none, three runs):
# 372, 348 and 340 KiB per shadow pane; the cap is 1.5x the largest.
# SHADOW_CORE_KIB_CAP=0 measures without a verdict.
SHADOW_N="${SHADOW_N:-8}"
SHADOW_CORE_KIB_CAP="${SHADOW_CORE_KIB_CAP:-558}"
SHELL_BIN="$DEV_TARGET/marspot-shell"
CORE_BIN="$DEV_TARGET/marspot-core"
for b in "$SHELL_BIN" "$CORE_BIN"; do
  [[ -x $b ]] || fail "$b is not built (bin/test-e2e-remote.sh builds release first)"
done
trap dev_kill_shell_core EXIT

# the core's resident KiB with $1 shadow panes beside one window pane,
# then the largest shadow L3's RSS in KiB
shadow_app() {
  local n=$1 i want core kib l3max=0 e p r
  dev_kill_shell_core
  dev_wipe_state
  rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
  mkdir -p "$MARSPOT_STATE_DIR/logs"
  python3 - "$MARSPOT_STATE_DIR" <<'PY' || return 1
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1])
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
body  = struct.pack('<II', 0xA5505010, 2) + struct.pack('<HH', 0, 1)
body += struct.pack('<HHHH', 1, 1, 0, 1) + struct.pack('<Q', 0) + s('') + s('')
(d / 'shell-state.bin').write_bytes(body)
frames  = struct.pack('<IIH', 0xA5505011, 2, 1) + struct.pack('<Idddd', 0, 80.0, 80.0, 1200.0, 800.0)
(d / 'window-state.bin').write_bytes(frames)
PY
  env SHELL=/bin/sh MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
    ${n:+MARSPOT_DEV_SHADOWS=$n} nohup "$SHELL_BIN" >/dev/null 2>&1 </dev/null & dev_record_pid $!
  disown
  want=$((1 + ${n:-0}))
  for (( i = 0; i < 600; i++ )); do
    (( $(dev_pane_shell_pids | grep -c .) >= want )) && break
    sleep 0.1
  done
  (( $(dev_pane_shell_pids | grep -c .) >= want )) || return 1
  sleep "${SHADOW_SETTLE:-20}"
  core=$(dev_core_pids | head -1)
  [[ -n $core ]] || return 1
  kib=$(python3 -c 'import ctypes, sys
buf = (ctypes.c_uint64 * 64)()
ctypes.CDLL("/usr/lib/libSystem.B.dylib").proc_pid_rusage(int(sys.argv[1]), 4, ctypes.byref(buf)) == 0 or sys.exit(1)
print(buf[8] // 1024)' "$core") || return 1
  for e in "$MARSPOT_STATE_DIR"/sessions/*/entry.toml; do
    grep -q '^shadow_of = [1-9]' "$e" || continue
    p=$(sed -n 's/^pid = \([0-9][0-9]*\)$/\1/p' "$e")
    r=$(ps -o rss= -p "$p" 2>/dev/null | tr -d ' ')
    if [[ -n $r ]] && (( r > l3max )); then l3max=$r; fi
  done
  echo "$kib $l3max"
}

echo "==> the core's resident size per shadow pane (0 vs $SHADOW_N shadow panes)"
BASE=$(shadow_app "") || fail "the sandbox app with no shadow panes did not come up"
WITH=$(shadow_app "$SHADOW_N") || fail "the sandbox app with $SHADOW_N shadow panes did not come up"
marked=$(cat "$MARSPOT_STATE_DIR"/sessions/*/entry.toml 2>/dev/null | grep -c '^shadow_of = [1-9]')
dev_kill_shell_core
(( marked == SHADOW_N )) || fail "$marked of the sessions were shadow panes, $SHADOW_N asked for"
C0=${BASE%% *}; C1=${WITH%% *}; L3MAX=${WITH##* }
PER_SHADOW=$(( (C1 - C0) / SHADOW_N ))
echo "    core resident: ${C0} KiB with none, ${C1} KiB with $SHADOW_N; ${PER_SHADOW} KiB per shadow pane"
echo "    largest shadow L3: ${L3MAX} KiB"
(( L3MAX <= PER_SESSION_CAP_KIB )) || fail "a shadow pane's L3 holds ${L3MAX} KiB > cap ${PER_SESSION_CAP_KIB} KiB"
if (( SHADOW_CORE_KIB_CAP == 0 )); then
  echo "MEASURED: ${PER_SHADOW} KiB per shadow pane (no cap set)"
  exit 0
fi
(( PER_SHADOW <= SHADOW_CORE_KIB_CAP )) || \
  fail "the core grows by ${PER_SHADOW} KiB per shadow pane > cap ${SHADOW_CORE_KIB_CAP} KiB"
echo "PASS: the core grows by ${PER_SHADOW} KiB per shadow pane ≤ ${SHADOW_CORE_KIB_CAP} KiB; shadow L3s ≤ ${PER_SESSION_CAP_KIB} KiB"

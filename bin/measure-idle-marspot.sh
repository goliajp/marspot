#!/usr/bin/env bash
# bin/measure-idle-marspot.sh — what a sandbox marspot with N idle panes
# costs, start to finish: lay out a window of N panes, let it settle,
# hand it to bin/measure-idle.sh.
#
# Opens a real window, so it runs on the runner:
#   E2E_LOCK=bench bin/test-e2e-remote.sh measure-idle-marspot.sh
#
# The panes run IDLE_SHELL (default /bin/sh): a prompt that never redraws
# itself, so what is measured is the terminal and not somebody's zsh
# theme.  Sandbox only: MARSPOT_STATE_DIR is redirected and the installed
# app is never touched.
#
#   IDLE_COLS / IDLE_ROWS   layout, default 4 x 3 (12 panes)
#   IDLE_SETTLE             seconds to wait after the panes are up (30)
#   IDLE_WINDOW             measured window in seconds (60)
#   IDLE_PROFILE            after measuring, sample L1 and L2 for this
#                           many seconds and list their busiest frames
#   IDLE_READINGS           take this many windows with bin/_idle_wakeups.py
#                           (timer wake-ups, CPU in nanoseconds, per thread)
#                           instead of the one `ps` window
#   IDLE_WAKEUP_BUDGET      with IDLE_READINGS: fail when any window's
#                           wake-ups per second, summed, are over this
#   IDLE_MEMORY             also print `footprint` of every process
#   IDLE_STACKS             sample L1, L2 and one L3 for this many seconds
#                           and print where each thread sits

#   IDLE_LAYOUTS            "1x1,2x2,4x3": measure each layout in turn, a
#                           fresh sandbox app for each

set -uo pipefail

if [[ -n ${IDLE_LAYOUTS:-} ]]; then
  rc=0
  for layout in ${IDLE_LAYOUTS//,/ }; do
    echo "════ layout $layout ════"
    IDLE_LAYOUTS='' IDLE_COLS="${layout%x*}" IDLE_ROWS="${layout#*x}" "$0" || rc=$?
  done
  exit $rc
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

COLS="${IDLE_COLS:-4}"
ROWS="${IDLE_ROWS:-3}"
N=$((COLS * ROWS))
SETTLE="${IDLE_SETTLE:-30}"
WINDOW="${IDLE_WINDOW:-60}"
IDLE_SHELL="${IDLE_SHELL:-/bin/sh}"

RUNLOG=/tmp/marspot-measure-idle.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"

fail() {
  echo "FAIL: $*"
  echo "  --- last 30 lines of $APPLOG:"
  tail -30 "$APPLOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() { dev_kill_shell_core; }
trap cleanup EXIT

for b in "$SHELL_BIN" "$CORE_BIN" "$SESSION_BIN"; do
  [[ -x $b ]] || fail "$b is not built (bin/test-e2e-remote.sh builds release first)"
done

cleanup
dev_wipe_state
rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
mkdir -p "$MARSPOT_STATE_DIR/logs"

# One window of COLS x ROWS fresh panes -- the restore format the shell
# reads at start (see bin/test-close-last-pane.sh for the same seed).
python3 - "$MARSPOT_STATE_DIR" "$COLS" "$ROWS" <<'PY' || fail "could not write the layout"
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1]); cols, rows = int(sys.argv[2]), int(sys.argv[3])
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
def pane(): return struct.pack('<Q', 0) + s('') + s('')
n = cols * rows
body  = struct.pack('<II', 0xA5505010, 2)
body += struct.pack('<HH', 0, 1)
body += struct.pack('<HHHH', cols, rows, 0, n) + b''.join(pane() for _ in range(n))
(d / 'shell-state.bin').write_bytes(body)
frames  = struct.pack('<IIH', 0xA5505011, 2, 1)
frames += struct.pack('<Idddd', 0, 80.0, 80.0, 1600.0, 1000.0)
(d / 'window-state.bin').write_bytes(frames)
PY

: > "$RUNLOG"
env SHELL="$IDLE_SHELL" MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
disown

# Every pane has its shell: the live shells the registry names.  Not
# the sessions' children: a shell launchd started is nobody's child.
shells() { dev_pane_shell_pids | grep -c .; }
for (( i = 0; i < 600; i++ )); do
  (( $(shells) >= N )) && break
  sleep 0.1
done
(( $(shells) >= N )) || fail "only $(shells) of $N panes have a shell after 60 s"
echo "==> $N panes up ($(dev_session_pids | wc -l | tr -d ' ') L3), settling ${SETTLE}s"
sleep "$SETTLE"

if [[ -n ${IDLE_READINGS:-} ]]; then
  "$ROOT/bin/_idle_wakeups.py" --label "marspot-dev $N panes" --window "$WINDOW" \
    --readings "$IDLE_READINGS" ${IDLE_WAKEUP_BUDGET:+--budget "$IDLE_WAKEUP_BUDGET"} \
    --pattern "$ROOT/target/(debug|release)/marspot(-shell|-core|-session)?( |\$)"
  rc=$?
  echo "    lock: $(/usr/local/bin/bench-lock status 2>/dev/null | grep -c waiting) waiting"
  if [[ -n ${IDLE_RESIDENT:-} ]]; then
    # the app quit with its sessions kept: the shell stays with no
    # window and has nothing to wake up for
    l1="$(dev_shell_pid)"
    osascript -l JavaScript -e "ObjC.import('AppKit');
      \$.NSRunningApplication.runningApplicationWithProcessIdentifier($l1).terminate" >/dev/null
    for (( i = 0; i < 100; i++ )); do grep -q $'\tSHELL_RESIDENT\t' "$APPLOG" && break; sleep 0.1; done
    grep -q $'\tSHELL_RESIDENT\t' "$APPLOG" && [[ "$(dev_shell_pid)" == "$l1" ]] || fail "the shell did not stay as a resident after the quit"
    echo "==> quit: shell $l1 resident, $(dev_session_pids | wc -l | tr -d ' ') sessions kept, settling ${IDLE_RESIDENT_SETTLE:-20}s"
    sleep "${IDLE_RESIDENT_SETTLE:-20}"
    "$ROOT/bin/_idle_wakeups.py" --label "marspot-dev quit, $N sessions kept, the shell alone" --window "$WINDOW" \
      --readings "$IDLE_READINGS" --budget "${IDLE_RESIDENT_BUDGET:-0.1}" \
      --pattern "$ROOT/target/(debug|release)/marspot-shell( |\$)" || rc=1
    "$ROOT/bin/_idle_wakeups.py" --label "marspot-dev quit, $N sessions kept, shell and sessions" --window "$WINDOW" \
      --readings "$IDLE_READINGS" ${IDLE_WAKEUP_BUDGET:+--budget "$IDLE_WAKEUP_BUDGET"} \
      --pattern "$ROOT/target/(debug|release)/marspot(-shell|-core|-session)?( |\$)" || rc=1
    echo "==> resident shell footprint: $(footprint "$l1" 2>/dev/null | sed -n 's/.*phys_footprint: *//p' | head -1)"
  fi
else
  "$ROOT/bin/measure-idle.sh" --app marspot-dev --sessions "$N" --window "$WINDOW"
  rc=$?
fi
if [[ -n ${IDLE_MEMORY:-} ]]; then
  for pid in $(dev_shell_pid) $(dev_core_pids) $(dev_session_pids); do
    echo "==> footprint of pid $pid ($(ps -o comm= -p "$pid" | sed 's|.*/||'))"
    footprint "$pid" 2>/dev/null | sed -n '/Dirty/,/TOTAL/p' | awk '$1 != "0" || /TOTAL/' | head -40
  done
fi
if [[ -n ${IDLE_STACKS:-} ]]; then
  for pid in $(dev_shell_pid) $(dev_core_pids | head -1) $(dev_session_pids | head -1); do
    out=/tmp/marspot-idle-stacks-$pid.sample
    sample "$pid" "$IDLE_STACKS" -file "$out" >/dev/null 2>&1
    echo "==> stacks of pid $pid ($(ps -o comm= -p "$pid" | sed 's|.*/||')), ${IDLE_STACKS}s"
    awk '/^Call graph:/{f=1;next} /^Total number in stack/{f=0} f' "$out" | cut -c1-170 | head -400
    rm -f "$out"
  done
fi

# Which layer it is: the result lists processes, this names them.
OUT="$ROOT/bench/results/idle-marspot-dev-$N-unknown.json"
if [[ -f $OUT ]]; then
  python3 - "$OUT" <<'PY'
import json, subprocess, sys
doc = json.load(open(sys.argv[1]))
layers = {}
for p in doc["per_process"]:
    cmd = subprocess.run(["ps", "-o", "command=", "-p", str(p["pid"])],
                         capture_output=True, text=True).stdout.strip().split(" ")[0]
    name = cmd.rsplit("/", 1)[-1] or "?"
    l = layers.setdefault(name, [0, 0.0, 0])
    l[0] += 1; l[1] += p["cpu_secs"]; l[2] += p["rss_kib"]
w = doc["window_secs"]
for name, (n, cpu, rss) in sorted(layers.items()):
    print(f"    {name:<16} x{n:<3} CPU {100 * cpu / w:6.3f}% of one core   RSS {rss // 1024} MiB")
PY
fi
if [[ -n ${IDLE_PROFILE:-} ]]; then
  for bin in "$SHELL_BIN" "$CORE_BIN"; do
    if [[ $bin == "$SHELL_BIN" ]]; then pid=$(dev_shell_pid); else pid=$(dev_core_pids | head -1); fi
    [[ -n $pid ]] || continue
    out=/tmp/marspot-idle-$(basename "$bin").sample
    sample "$pid" "$IDLE_PROFILE" -file "$out" >/dev/null 2>&1
    echo "==> $(basename "$bin") (pid $pid), ${IDLE_PROFILE}s: busiest frames of its own code"
    # self samples of our functions and of the syscalls they sit in
    awk '/Sort by top of stack/{f=1;next} /Binary Images/{f=0} f&&NF' "$out" \
      | grep -v -E 'semaphore_wait_trap|__workq_kernreturn|mach_msg2_trap|__semwait_signal|__psynch_cvwait|kevent|__select|__accept|__recvfrom|__read_nocancel|^ *read ' \
      | head -15 | cut -c1-150
  done
fi
exit $rc

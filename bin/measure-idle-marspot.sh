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

set -uo pipefail

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
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null &
disown

# Every pane has its shell: count shells whose parent is one of our L3s.
shells() {
  local l3
  l3=$(pgrep -f "$SESSION_BIN( |\$)" | paste -sd, -)
  [[ -n $l3 ]] || { echo 0; return; }
  ps -Ao ppid=,command= | awk -v want="$l3" '
    BEGIN { n = split(want, a, ","); for (i = 1; i <= n; i++) p[a[i]] = 1 }
    ($1 in p) { c++ } END { print c + 0 }'
}
for (( i = 0; i < 600; i++ )); do
  (( $(shells) >= N )) && break
  sleep 0.1
done
(( $(shells) >= N )) || fail "only $(shells) of $N panes have a shell after 60 s"
echo "==> $N panes up ($(pgrep -f "$SESSION_BIN( |\$)" | wc -l | tr -d ' ') L3), settling ${SETTLE}s"
sleep "$SETTLE"

"$ROOT/bin/measure-idle.sh" --app marspot-dev --sessions "$N" --window "$WINDOW"
rc=$?

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
    pid=$(pgrep -f "$bin( |\$)" | head -1)
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

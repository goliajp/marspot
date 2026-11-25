#!/usr/bin/env bash
# bin/measure-active.sh — what a terminal costs with one pane working and
# N panes idle: marspot's three processes (shell, core, every session) of
# a sandbox build against iTerm2's processes, the same workload, one
# after the other on the same host in one run.
#
# Opens real windows, so it runs on the runner, holding the host's lock
# exclusively for the whole run (four readings take up to ~2200 s):
#   E2E_LOCK=bench E2E_SUITE_TIMEOUT=2700 bin/test-e2e-remote.sh measure-active.sh
#
# Each reading: one window of N + 1 panes, 1000 x 1400 pt at the same
# place on screen for both terminals, laid out in rows of four (N = 4:
# four over one, N = 12: three rows of four over one).  Every pane runs
# the same wrapper: it writes its pid to the run's pane list, and once
# the layout stands its size (`stty size`).  The active pane is then
# chosen by geometry, of the same size in both terminals: a pane of a
# full row of four, the one of the lowest pid among them.  (The last
# row's single pane is no use: iTerm2 stretches it over the row, marspot
# keeps it to one cell of its grid.)  It plays bin/_active_stream.py (an
# agent CLI's screen output at a fixed pace, ~1.5 KiB/s); every pane
# then becomes an idle /bin/sh prompt.  After the panes settle the
# stream is started, and bin/measure-idle.sh reads the CPU of the
# terminal's processes over the window while it plays.  The panes are
# counted by their live pids, not trusted; the window must be on screen,
# alone and of the asked size before and after.
#
# The readings are refused (exit 2) unless the two terminals were given
# the same work: the bytes the stream wrote inside each window differ by
# at most 2%, and the active panes' sizes differ by no more than the two
# terminals' fonts force plus ACTIVE_SIZE_SLACK cells (padding, margins).
#
#   ACTIVE_IDLES     idle pane counts to read, default "4 12"
#   ACTIVE_TERMS     default "marspot iterm2"
#   ACTIVE_SETTLE    seconds between the panes coming up and the stream (20)
#   ACTIVE_WINDOW    measured seconds, a multiple of the stream's 64 s
#                    cycle (64)
#   ACTIVE_QUIET_WAIT  before each window, seconds to wait for load1 to
#                    fall under measure-idle's busy line (300)
#   ACTIVE_SIZE_SLACK  cells (3)
#
# Writes bench/results/active-<app>-<N>.json per reading and
# bench/results/active-vs-iterm.json with all of them.  iTerm2 must not
# be running when this starts: it is quit afterwards.  Its pids are kept
# in $MARSPOT_STATE_DIR/iterm.pids from the moment it is launched until
# it is gone, so a run cut short can be cleaned up from outside with
# `bin/_active_iterm.sh close <that file>`.
#
# Exit codes, the four of bin/measure-idle.sh: 0 measured, 2 the
# instrument failed (no reading), 3 the host was busy during a window.
# Of several readings the one that says most decides: 2, then 3.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

IDLES="${ACTIVE_IDLES:-4 12}"
TERMS="${ACTIVE_TERMS:-marspot iterm2}"
SETTLE="${ACTIVE_SETTLE:-20}"
WINDOW="${ACTIVE_WINDOW:-64}"
QUIET_WAIT="${ACTIVE_QUIET_WAIT:-300}"
export ACTIVE_SIZE_SLACK="${ACTIVE_SIZE_SLACK:-3}"
LEAD=5
RESULTS="$ROOT/bench/results"
ITERM_PIDS="$MARSPOT_STATE_DIR/iterm.pids"
# one frame for both: AppKit's bottom-left origin for marspot, iTerm2's
# top-left bounds are derived from the screen height
FX=40 FY=200 FW=1000 FH=1400
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"
ITERM_PATTERN='(iTerm\.app/Contents/|Application Support/iTerm2/[i]TermServer)'

(( WINDOW > 0 && WINDOW % 64 == 0 )) || { echo "FAIL: ACTIVE_WINDOW=$WINDOW is not a multiple of 64"; exit 2; }

worst() {
  local c
  for c in 2 3; do
    if [[ $1 == "$c" || $2 == "$c" ]]; then echo "$c"; return; fi
  done
  if [[ $1 == 0 && $2 == 0 ]]; then echo 0; else echo 2; fi
}

# The stream and the pane shells go first, while the terminal that
# started them still runs: their pids cannot have been reused yet, and
# each is checked to still run what it ran.
end_panes() {
  local p pid left=() i
  [[ -n $RUN ]] || return 0
  pid=$(cat "$RUN/stream.pid" 2>/dev/null)
  # timeout(1) passes TERM on to the stream
  [[ -n $pid ]] && ps -o command= -p "$pid" 2>/dev/null | grep -q _active_stream.py && kill -TERM "$pid" 2>/dev/null
  for p in "$RUN"/panes/*; do
    [[ -f $p ]] || continue
    pid=${p##*/}
    [[ $(ps -o comm= -p "$pid" 2>/dev/null) == *sh ]] && left+=("$pid")
  done
  (( ${#left[@]} )) || return 0
  # an interactive sh ignores TERM; HUP is what a closing terminal sends
  kill -HUP "${left[@]}" 2>/dev/null
  for (( i = 0; i < 30; i++ )); do
    kill -0 "${left[@]}" 2>/dev/null || return 0
    sleep 0.1
  done
  for pid in "${left[@]}"; do
    [[ $(ps -o comm= -p "$pid" 2>/dev/null) == *sh ]] && kill -KILL "$pid" 2>/dev/null
  done
  return 0
}

RUN="" WID="" CAF=""
close_term() {
  end_panes
  dev_kill_shell_core
  [[ -f $ITERM_PIDS ]] && "$ROOT/bin/_active_iterm.sh" close "$ITERM_PIDS" "$WID"
  WID=""
  if [[ -n $RUN ]]; then command rm -rf "${RUN:?}"; RUN=""; fi
}

cleanup() {
  close_term
  [[ -n $CAF ]] && kill "$CAF" 2>/dev/null
}
trap cleanup EXIT
trap 'exit 143' TERM INT HUP

for b in "$SHELL_BIN" "$CORE_BIN" "$SESSION_BIN"; do
  [[ -x $b ]] || { echo "FAIL: $b is not built"; exit 2; }
done
if [[ $TERMS == *iterm2* ]] && ps -Ao command= | grep -qE "$ITERM_PATTERN"; then
  echo "FAIL: iTerm2 is running; this run would quit it, so it refuses"; exit 2
fi
# a display that sleeps takes every window off screen and nothing renders
caffeinate -u -d -t 3600 & CAF=$!

front_pid() { lsappinfo info -only pid "$(lsappinfo front)" 2>/dev/null | sed -n 's/.*pid = \([0-9]*\).*/\1/p' | head -1; }
front_app() { lsappinfo info -only name "$(lsappinfo front)" 2>/dev/null | sed -n 's/^"\(.*\)".*/\1/p' | head -1; }
# "<count> <width>x<height>": the app's on-screen ordinary windows of
# more than a sliver, and the size of the largest
on_screen() {
  dev_windows_of "$1" | awk '$1 == 0 { split($2, a, "x"); if (a[1] >= 200 && a[2] >= 200) { n++; if (a[1] * a[2] > bw * bh) { bw = a[1]; bh = a[2] } } }
    END { printf "%d %dx%d\n", n, bw, bh }'
}
# exactly one window, within 5% of the asked frame both ways
window_ok() {
  local n=${1%% *} wh=${1#* }
  (( n == 1 )) && awk -v w="${wh%x*}" -v h="${wh#*x}" -v fw="$FW" -v fh="$FH" \
    'BEGIN { exit !(w >= fw * 0.95 && w <= fw * 1.05 && h >= fh * 0.95 && h <= fh * 1.05) }'
}
live_panes() { local p n=0; for p in "$RUN"/panes/*; do [[ -f $p ]] && kill -0 "${p##*/}" 2>/dev/null && n=$((n + 1)); done; echo "$n"; }
count_files() { local f n=0; for f in "$1"/*; do [[ -f $f ]] && n=$((n + 1)); done; echo "$n"; }

# A neighbour's work that ended just before the lock became ours still
# sits in load1 for a minute or two: wait for it to decay rather than
# take a window bin/measure-idle.sh will refuse anyway.
quiet() {
  local i l
  for (( i = 0; i < QUIET_WAIT / 10; i++ )); do
    l=$(uptime | sed 's/.*averages: //' | awk '{print $1}')
    python3 -c "import sys; sys.exit(0 if $l <= ${IDLE_BUSY_LOAD:-5.0} else 1)" && return 0
    (( i % 6 == 0 )) && echo "    load1 $l, waiting for it to fall to ${IDLE_BUSY_LOAD:-5.0}"
    sleep 10
  done
}

prepare() {
  RUN="/tmp/msp-active-$$-$1-$2"
  command rm -rf "${RUN:?}"
  command mkdir -p "$RUN/panes" "$RUN/sizes" "$RUN/sizes-after"
  mkfifo "$RUN/go"
  # the polling below lasts only until the layout stands, before any window
  cat > "$RUN/pane.sh" <<EOF
#!/bin/sh
echo \$\$ > "$RUN/panes/\$\$"
i=0; while [ ! -e "$RUN/laid-out" ] && [ \$i -lt 600 ]; do sleep 0.2; i=\$((i + 1)); done
stty size > "$RUN/sizes/.\$\$" && mv "$RUN/sizes/.\$\$" "$RUN/sizes/\$\$"
i=0; while [ ! -e "$RUN/active" ] && [ \$i -lt 600 ]; do sleep 0.2; i=\$((i + 1)); done
if [ "\$(cat "$RUN/active" 2>/dev/null)" = "\$\$" ]; then
  /usr/bin/timeout 600 /usr/bin/python3 "$ROOT/bin/_active_stream.py" --go "$RUN/go" --stats "$RUN/stream.json" &
  echo \$! > "$RUN/stream.pid"
  wait \$!
fi
exec /bin/sh
EOF
  chmod +x "$RUN/pane.sh"
}

open_marspot() { # rows, panes
  dev_wipe_state
  command rm -rf "${MARSPOT_STATE_DIR:?}/sessions" "${MARSPOT_STATE_DIR:?}/retired"
  mkdir -p "$MARSPOT_STATE_DIR/logs"
  python3 - "$MARSPOT_STATE_DIR" 4 "$1" "$2" "$FX" "$FY" "$FW" "$FH" <<'PY' || return 2
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1]); cols, rows, n = (int(x) for x in sys.argv[2:5])
x, y, w, h = (float(v) for v in sys.argv[5:9])
def s(t): b = t.encode(); return struct.pack('<H', len(b)) + b
body = struct.pack('<II', 0xA5505010, 2) + struct.pack('<HH', 0, 1)
body += struct.pack('<HHHH', cols, rows, 0, n) + b''.join(struct.pack('<Q', 0) + s('') + s('') for _ in range(n))
(d / 'shell-state.bin').write_bytes(body)
(d / 'window-state.bin').write_bytes(struct.pack('<IIH', 0xA5505011, 2, 1) + struct.pack('<Idddd', 0, x, y, w, h))
PY
  env SHELL="$RUN/pane.sh" MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
    nohup "$SHELL_BIN" >"$(dev_run_log measure-active)" 2>&1 < /dev/null & dev_record_pid $!
  disown
}

# iTerm2 is brought forward by AppleScript; marspot the way a person's
# second launch does it: the new shell stands aside and asks the running
# one to come to the front
front_marspot() {
  env SHELL="$RUN/pane.sh" /usr/bin/timeout 20 "$SHELL_BIN" >/dev/null 2>&1 < /dev/null
  sleep 1
}

open_iterm() { # rows, panes
  local sh out
  sh=$(osascript -l JavaScript -e "ObjC.import('AppKit'); \$.NSScreen.mainScreen.frame.size.height")
  out=$("$ROOT/bin/_active_iterm.sh" open "$ITERM_PIDS" "$RUN" "$RUN/pane.sh" "$1" $(( $2 - 4 * ($1 - 1) )) \
    "$FX" $(( ${sh%.*} - FY - FH )) $(( FX + FW )) $(( ${sh%.*} - FY ))) || return 2
  WID=${out%% *}
  echo "$out" | cut -d' ' -f2- > "$RUN/grid"
}

app_pid() { if [[ $1 == marspot ]]; then dev_shell_pid; else ps -Ao pid=,command= | grep -E 'iTerm\.app/Contents/MacOS/iTerm2' | awk '{print $1}' | head -1; fi; }

# Every pane's size once the layout stands; of the size most panes have
# (a pane of a full row of four), the lowest pid becomes the active one.
choose_active() {
  local i a
  touch "$RUN/laid-out"
  for (( i = 0; i < 100; i++ )); do (( $(count_files "$RUN/sizes") >= $1 )) && break; sleep 0.1; done
  (( $(count_files "$RUN/sizes") == $1 )) || { echo "FAIL: $(count_files "$RUN/sizes") of $1 panes told their size"; return 2; }
  a=$(for f in "$RUN"/sizes/*; do echo "${f##*/} $(cat "$f")"; done | awk '
    { k = $2 " " $3; n[k]++; if (!(k in lo) || $1 + 0 < lo[k]) lo[k] = $1 + 0 }
    END { for (k in n) { split(k, s, " "); if (n[k] > bn || (n[k] == bn && s[1] * s[2] > ba)) { bn = n[k]; ba = s[1] * s[2]; b = k } }
          if (bn > 1) print lo[b] }')
  [[ -n $a ]] || { echo "FAIL: no two panes of one size: $(cat "$RUN"/sizes/* | paste -sd, -)"; return 2; }
  echo "$a" > "$RUN/.active" && mv "$RUN/.active" "$RUN/active"
  echo "==> active pane $a, $(cat "$RUN/sizes/$a") (rows cols)"
}

# each pane's size read from outside after the window, by its tty
sizes_after() {
  local p pid t
  for p in "$RUN"/panes/*; do
    pid=${p##*/}
    t=$(ps -o tty= -p "$pid" 2>/dev/null | tr -d ' ')
    [[ -n $t && $t != "??" ]] && stty -f "/dev/$t" size > "$RUN/sizes-after/$pid" 2>/dev/null
  done
}

reading() { # term, idle panes
  local rc
  reading_body "$@"
  rc=$?
  close_term
  sleep 5
  return "$rc"
}

reading_body() { # term, idle panes
  local term=$1 idle=$2 panes=$(( $2 + 1 )) rows app rc pid i
  rows=$(( (panes + 3) / 4 ))
  case "$rows:$(( panes - 4 * (rows - 1) ))" in 1:[124]|2:[124]|4:[124]) ;; *)
    echo "FAIL: $panes panes do not lay out as rows of 4 with 1, 2 or 4 in the last"; return 2 ;; esac
  prepare "$term" "$idle"
  echo "════ $term: $idle idle + 1 active ($panes panes, $rows rows) ════"
  if [[ $term == marspot ]]; then app=marspot-dev; open_marspot "$rows" "$panes" || return 2
  else app=iterm2; open_iterm "$rows" "$panes" || return 2; fi
  for (( i = 0; i < 600; i++ )); do (( $(live_panes) >= panes )) && break; sleep 0.1; done
  (( $(live_panes) == panes )) || { echo "FAIL: $(live_panes) of $panes panes have a shell"; return 2; }
  pid=$(app_pid "$term")
  [[ $term == marspot ]] && front_marspot
  sleep 2
  choose_active "$panes" || return 2
  python3 "$ROOT/bin/_active_result.py" font "$term" "$ROOT" > "$RUN/font" || { echo "FAIL: no font for $term"; return 2; }
  echo "==> $panes pane shells up, font $(cat "$RUN/font"), settling ${SETTLE}s"
  sleep "$SETTLE"
  quiet
  /usr/bin/timeout 10 sh -c "echo $(( LEAD + WINDOW + 10 )) > '$RUN/go'" || { echo "FAIL: the active pane is not waiting"; return 2; }
  sleep "$LEAD"
  local on0 front0 on1 front1
  on0=$(on_screen "$pid"); front0=$(front_app)
  window_ok "$on0" || { echo "FAIL: $term windows on screen: $on0 (want 1 of ${FW}x${FH}; front: $front0)"; return 2; }
  # a focused terminal draws its cursor and an unfocused one may not
  [[ $(front_pid) == "$pid" ]] || { echo "FAIL: $term is not the front app ($front0 is)"; return 2; }
  "$ROOT/bin/measure-idle.sh" --app "$app" --sessions "$panes" --pane-pids "$RUN/panes" \
    --window "$WINDOW" --out "$RUN/idle.json"
  rc=$?
  on1=$(on_screen "$pid"); front1=$(front_app)
  [[ $(front_pid) == "$pid" ]] || { echo "FAIL: $term lost the front during the window ($front1 has it)"; rc=2; }
  window_ok "$on1" || { echo "FAIL: $term windows on screen after the window: $on1"; rc=2; }
  [[ -f $RUN/idle.json ]] || return 2
  sizes_after
  ps -Ao pid=,ppid=,command= > "$RUN/ps.txt"
  for (( i = 0; i < 300; i++ )); do [[ -f $RUN/stream.json ]] && break; sleep 0.1; done
  [[ -f $RUN/stream.json ]] || { echo "FAIL: the stream did not finish"; rc=2; }
  local ver=""
  [[ $term == iterm2 ]] && ver=$(defaults read /Applications/iTerm.app/Contents/Info.plist CFBundleShortVersionString)
  python3 "$ROOT/bin/_active_result.py" one "$RUN" "$term" "$idle" "$rc" "$ver" "$on0/$on1" "$front0/$front1" \
    "$RESULTS/active-$app-$idle.json" || rc=2
  return "$rc"
}

mkdir -p "$RESULTS"
# the summary is of this run only
for n in $IDLES; do
  command rm -f "${RESULTS:?}/active-marspot-dev-${n:?}.json" "${RESULTS:?}/active-iterm2-${n:?}.json"
done
command rm -f "${RESULTS:?}/active-vs-iterm.json"
rc=0
for n in $IDLES; do
  for t in $TERMS; do
    reading "$t" "$n"
    rc=$(worst "$rc" $?)
  done
done
python3 "$ROOT/bin/_active_result.py" summary "$RESULTS" "$IDLES" || rc=2
exit "$rc"

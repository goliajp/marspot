#!/usr/bin/env bash
# bin/test-frame-out-of-sight.sh — a window nobody can see is not drawn
# for, and shows the newest screen the moment it is seen again.
#
# The core draws a window's next frame when the shell reports the last
# one shown, and the shell reports at the display's refresh.  A hidden
# or minimised window has no refresh.  Its pane goes on printing; the
# core must neither draw those frames nor wait in a way that stops it
# doing anything else, and when the window comes back the first thing
# on it must be the pane as it is now.
#
# For each of `hidden` and `minimised`: a sandbox whose pane prints in
# bursts, put out of sight three seconds after its first frame and
# brought back twelve seconds later (MARSPOT_DEV_WINDOW_STATE).  Checks
#
#   - out of sight, the count of frames shown stops growing (one burst
#     may still be in flight when the window goes),
#   - the core keeps answering: its pings are answered throughout, and
#   - back in sight, frames arrive again and the window as composited
#     is the surface last shown (SHOWN_CHECK).
#
# Needs a build with the measuring compiled in (--features
# latency-trace).  Opens real windows: bin/test-e2e-remote.sh.  Two
# sandboxes of about half a minute each, stopped through the helpers.
set -uo pipefail
trap 'exit 1' HUP INT TERM

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

BIN="${OUT_OF_SIGHT_BIN:-$ROOT/target/release}"
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
RUNLOG="$(dev_run_log test-frame-out-of-sight)"
AWAY_SECS=12

fail() {
  echo "FAIL: $*"
  grep -E $'\t(SHOWN_CHECK|SHOWN_CHECK_NO_CAPTURE|DEV_WINDOW_STATE|FRAME_PACE)\t' "$APPLOG" 2>/dev/null | tail -12 | cut -c1-300 | sed 's/^/    /'
  exit 1
}
trap dev_kill_shell_core EXIT

for b in marspot-shell marspot-core marspot-session; do
  [[ -x $BIN/$b ]] || fail "$BIN/$b is not built"
done

PANE_SH="$MARSPOT_STATE_DIR/out-of-sight-pane.sh"
mkdir -p "$MARSPOT_STATE_DIR"
cat > "$PANE_SH" <<'EOF'
#!/bin/bash
n=0
while :; do
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    n=$((n + 1))
    printf '\033[3%dm%06d\033[0m the quick brown fox jumps over the lazy dog %s\n' $((n % 7 + 1)) "$n" "$(printf '=%.0s' $(seq 1 $((n % 40 + 1))))"
    sleep 0.1
  done
  sleep 1.5
done
EOF
chmod +x "$PANE_SH"

field() { # line, name
  sed -n "s/.*[[:space:]]$2=\([^[:space:]]*\).*/\1/p" <<<"$1"
}

wait_line() { # pattern, how many, seconds
  local i n
  for (( i = 0; i < $3 * 5; i++ )); do
    n=$(grep -c "$1" "$APPLOG" 2>/dev/null)
    (( ${n:-0} >= $2 )) && return 0
    sleep 0.2
  done
  return 1
}

one_state() {
  local state=$1 before away_line back_line gone back last up flip
  dev_kill_shell_core
  dev_wipe_state
  mkdir -p "$MARSPOT_STATE_DIR/logs"
  env SHELL="$PANE_SH" MARSPOT_SESSION_BIN="$BIN/marspot-session" MARSPOT_CORE_BIN="$BIN/marspot-core" \
    MARSPOT_DEV_WINDOW_STATE="$state:$AWAY_SECS" \
    nohup "$BIN/marspot-shell" >"$RUNLOG" 2>&1 < /dev/null &
  dev_record_pid $!
  disown

  wait_line $'\tFRAME_PACE\t.*paced=true' 1 30 || fail "$state: the core's frames are not shown directly"
  wait_line $'\tDEV_WINDOW_STATE\t.*away=true' 1 30 || fail "$state: the window was not put out of sight"
  wait_line $'\tDEV_WINDOW_STATE\t.*away=false' 1 $((AWAY_SECS + 10)) || fail "$state: the window was not brought back"
  # two bursts after it is back
  sleep 6

  away_line=$(grep -n $'\tDEV_WINDOW_STATE\t.*away=true' "$APPLOG" | head -1 | cut -d: -f1)
  back_line=$(grep -n $'\tDEV_WINDOW_STATE\t.*away=false' "$APPLOG" | head -1 | cut -d: -f1)
  before=$(field "$(sed -n "1,${away_line}p" "$APPLOG" | grep $'\tSHOWN_CHECK' | tail -1)" frames)
  gone=$(field "$(sed -n "${away_line},${back_line}p" "$APPLOG" | grep $'\tSHOWN_CHECK' | tail -1)" frames)
  last=$(grep $'\tSHOWN_CHECK\t' "$APPLOG" | tail -1)
  back=$(field "$last" frames)
  [[ -n $before && -n $back ]] || fail "$state: no frame was checked before the window went or after it came back"
  gone=${gone:-$before}
  # the pane printed about fifty lines while the window was away; a
  # burst is ten
  (( gone - before <= 14 )) || fail "$state: $((gone - before)) frames were drawn for a window out of sight"
  (( back > gone )) || fail "$state: no frame after the window came back ($back shown, $gone before)"
  [[ $(sed -n "$((back_line + 1)),\$p" "$APPLOG" | grep -c $'\tSHOWN_CHECK\t') -ge 1 ]] \
    || fail "$state: the window back in sight was never compared with its surface"
  up=$(field "$last" upright)
  flip=$(field "$last" flipped)
  awk -v u="$up" -v f="$flip" 'BEGIN { exit !(u >= 0.9 && u - f >= 0.2) }' \
    || fail "$state: back in sight, the window does not show the surface (upright $up, upside down $flip)"
  grep -q $'\tERROR\t' "$APPLOG" && fail "$state: an error was logged"
  grep -q $'\tPONG_TIMEOUT\t' "$APPLOG" && fail "$state: the core stopped answering"
  kill -0 "$(dev_core_pids | head -1)" 2>/dev/null || fail "$state: the core is gone"
  echo "  ok: $state: $before frames before, $gone when out of sight ended, $back after; upright $up"
  dev_kill_shell_core
}

echo "==> $(sw_vers -productVersion) $(sw_vers -buildVersion), load1 $(sysctl -n vm.loadavg | awk '{print $2}')"
one_state hidden
one_state minimised
echo "ALL PASS"

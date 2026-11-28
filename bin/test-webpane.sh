#!/usr/bin/env bash
# bin/test-webpane.sh — a web pane shows a page as the engine draws it,
# at 1x and 2x, and an idle web pane costs no CPU in any process.
#
# For each scale: a sandbox of one window and one pane opens one web
# pane on a fixed local page (tests/fixtures/webpane/page.html) through
# its dev seam (MARSPOT_DEV_WEB_PANE), with madcawl's host built from the
# commit Cargo.toml pins (bin/_madcawl-host.sh) and the window drawn at
# that scale whatever the runner's screen is (MARSPOT_DEV_BACKING_SCALE).
# Once the core logs a finished frame showing the page (WEB_PANE_DRAWN),
# the pane's region is read back from that frame's surface and compared
# with `madcawl-shell shot` of the same page at the same scale; then, 10 s
# later, the CPU time and wakeups of the core, the host, the page's
# content process and the network process, when the host has one (the
# shell starts it without --net, so it has none yet), are read over 60 s.
#
# Passes when at most WEBPANE_MAX_DIFFER pixels of the region differ
# (default 0: the pane shows the engine's pixels unchanged), and over the
# 60 s madcawl's processes and the core's web pane thread used no CPU
# time and woke no time, and the core as a whole stayed within the app's
# idle wakeup budget (it has clocks of its own, web pane or not).
# The pane regions go to target/webpane-shots/.
#
# Opens real windows: bin/test-e2e-remote.sh only.
set -uo pipefail
trap 'exit 1' HUP INT TERM

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

BIN="${WEBPANE_BIN:-$ROOT/target/release}"
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
RUNLOG="$(dev_run_log test-webpane)"
PAGE="$ROOT/tests/fixtures/webpane/page.html"
SHOTS="$ROOT/target/webpane-shots"
MAX_DIFFER="${WEBPANE_MAX_DIFFER:-0}"
IDLE_SECS="${WEBPANE_IDLE_SECS:-60}"
# wakeups a second, the line bin/test-idle-wakeups.sh draws for the app at rest
CORE_BUDGET="${WEBPANE_CORE_WAKEUPS:-1.2}"
CHECK="$ROOT/bin/_webpane_check.py"

fail() {
  echo "FAIL: $*"
  grep -aE $'\t(WEB_PANE_[A-Z]+|web_pane[.a-z_]*|madcawl[.a-z_]*)\t' "$APPLOG" 2>/dev/null | tail -12 | cut -c1-300 | sed 's/^/    /'
  tail -5 "$MARSPOT_STATE_DIR/logs/madcawl-host.log" 2>/dev/null | sed 's/^/    host: /'
  exit 1
}
trap dev_kill_shell_core EXIT

for b in marspot-shell marspot-core marspot-session; do
  [[ -x $BIN/$b ]] || fail "$BIN/$b is not built"
done
echo "==> madcawl's host"
MC="$("$ROOT/bin/_madcawl-host.sh")" || fail "madcawl-shell could not be built"
echo "    $MC"
mkdir -p "$SHOTS"

field() { # line, name
  sed -n "s/.*[[:space:]]$2=\([^[:space:]]*\).*/\1/p" <<<"$1"
}

wait_line() { # pattern, seconds
  local i
  for (( i = 0; i < $2 * 5; i++ )); do
    grep -aq "$1" "$APPLOG" 2>/dev/null && return 0
    sleep 0.2
  done
  return 1
}

# one window of 1000 by 700 points with one pane, as a person's first start
write_layout() {
  python3 - "$MARSPOT_STATE_DIR" <<'PY' || fail "could not write the layout"
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1])
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
body = struct.pack('<II', 0xA5505010, 2) + struct.pack('<HH', 0, 1)
body += struct.pack('<HHHH', 1, 1, 0, 1) + struct.pack('<Q', 0) + s('') + s('')
(d / 'shell-state.bin').write_bytes(body)
frames = struct.pack('<IIH', 0xA5505011, 2, 1) + struct.pack('<Idddd', 0, 80.0, 200.0, 1000.0, 700.0)
(d / 'window-state.bin').write_bytes(frames)
PY
}

usage_of() { # name=pid… → "name cpu_ns wakeups" lines
  python3 "$CHECK" usage "$@" || fail "a process could not be read: $*"
}

one_scale() {
  local scale=$1 drawn sid x y w h out diffline differ host content netd before after
  dev_kill_shell_core
  dev_wipe_state
  mkdir -p "$MARSPOT_STATE_DIR/logs"
  write_layout
  env MARSPOT_SESSION_BIN="$BIN/marspot-session" MARSPOT_CORE_BIN="$BIN/marspot-core" \
    MARSPOT_DEV_WEB_PANE="file://$PAGE" MARSPOT_DEV_MADCAWL_SHELL="$MC" MARSPOT_DEV_BACKING_SCALE="$scale" \
    nohup "$BIN/marspot-shell" >"$RUNLOG" 2>&1 < /dev/null &
  dev_record_pid $!
  disown

  wait_line $'\tWEB_PANE_DRAWN\t' 60 || fail "${scale}x: no frame showed the page"
  drawn=$(grep -a $'\tWEB_PANE_DRAWN\t' "$APPLOG" | tail -1)
  sid=$(field "$drawn" surface_id)
  x=$(field "$drawn" x); y=$(field "$drawn" y); w=$(field "$drawn" width); h=$(field "$drawn" height)
  awk -v a="$(field "$drawn" scale)" -v b="$scale" 'BEGIN { exit !(a + 0 == b + 0) }' || fail "${scale}x: the core drew at scale $(field "$drawn" scale)"
  out="$SHOTS/${scale}x"
  python3 "$ROOT/bin/_surface-png.py" "$sid" "$out-surface.png" || fail "${scale}x: surface $sid unread"
  "$MC" shot "$PAGE" -o "$out-shot.png" --scale "$scale" >/dev/null || fail "${scale}x: shot failed"
  diffline=$(python3 "$CHECK" diff "$out-surface.png" "$out-shot.png" "${x%.*}" "${y%.*}" "${w%.*}" "${h%.*}" "$out-pane.png") \
    || fail "${scale}x: the regions could not be compared"
  echo "  ${scale}x: pane ${w%.*}x${h%.*} at ${x%.*},${y%.*} of surface $sid: $diffline"
  differ=$(field " $diffline" differ)
  (( $(field " $diffline" colors) > 16 )) || fail "${scale}x: the pane is nearly one colour, not the page"
  (( differ <= MAX_DIFFER )) || fail "${scale}x: $differ pixels differ from shot (at most $MAX_DIFFER)"

  host=$(awk '{ print $1 }' "$MARSPOT_STATE_DIR/madcawl-host.pid" 2>/dev/null)
  content=$(field "$(grep -a $'\tWEB_PANE_OPENED\t' "$APPLOG" | tail -1)" content_pid)
  netd=$(pgrep -P "$host" 2>/dev/null | grep -vx "$content" | head -1)
  [[ -n $host && -n $content ]] || fail "${scale}x: host '$host' or content '$content' not found"
  sleep 10
  local core thread0 thread1
  core=$(dev_core_pids | head -1)
  local pids=("core=$core" "host=$host" "content=$content")
  # a host started without --net has no network process
  if [[ -n $netd ]]; then pids+=("netd=$netd"); else echo "    ${scale}x: the host has no network process"; fi
  before=$(usage_of "${pids[@]}")
  thread0=$(python3 "$CHECK" thread "$core" web-pane-) || fail "${scale}x: the core has no web pane thread"
  sleep "$IDLE_SECS"
  after=$(usage_of "${pids[@]}")
  thread1=$(python3 "$CHECK" thread "$core" web-pane-) || fail "${scale}x: the web pane thread is gone"
  local busy="" name ns wk ns0 wk0
  while read -r name ns wk; do
    read -r _ ns0 wk0 < <(grep "^$name " <<<"$before")
    echo "    ${scale}x idle ${IDLE_SECS}s: $name cpu $((ns - ns0)) ns, $((wk - wk0)) wakeups"
    if [[ $name == core ]]; then
      # the core as a whole wakes on clocks of its own (the graphics
      # driver's memory-pool timer among them) with or without a web
      # pane: held to the idle budget the app's own idle suite uses
      awk -v w="$((wk - wk0))" -v s="$IDLE_SECS" -v b="$CORE_BUDGET" 'BEGIN { exit !(w / s <= b) }' || busy+=" core-wakeups"
    elif (( ns != ns0 || wk != wk0 )); then
      busy+=" $name"
    fi
  done <<<"$after"
  local t0 t1
  t0=$(field " $thread0" cpu_ns); t1=$(field " $thread1" cpu_ns)
  echo "    ${scale}x idle ${IDLE_SECS}s: the core's web pane thread cpu $((t1 - t0)) ns"
  (( t1 == t0 )) || busy+=" web-pane-thread"
  [[ -z $busy ]] || fail "${scale}x: busy while the web pane was idle:$busy"
  echo "  ok: ${scale}x"
  dev_kill_shell_core
}

echo "==> $(sw_vers -productVersion) $(sw_vers -buildVersion), load1 $(sysctl -n vm.loadavg | awk '{print $2}')"
one_scale 1
one_scale 2
echo "ALL PASS"

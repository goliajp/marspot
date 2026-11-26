# Sourced by bin/test-helper-strip.sh: how it drives the sandbox, reads
# the parent's sizes and the strip, and takes pictures of the window.
# Expects SD, ROOT, PEER, CLI, BIN, LOG, SB, REGISTRY, SIZES and
# SETTING set, and bin/_dev-sandbox.sh and bin/_sandbox-panes.sh sourced.

pc() { in_sd "$SD" "$PEER" pane "$@"; }
hc() { in_sd "$SD" "$PEER" helper "$@"; }
field() { python3 -c 'import json,sys; v=json.load(sys.stdin); r=v.get("result",v); print(r.get(sys.argv[1], ""))' "$1"; }
start_helper() { # agent, task, more flags… → the answer's JSON
  hc start --json --agent "$1" --parent "$P1" --cwd "$SB/proj" --budget 30 "${@:3}" "$2"
}
started() { # agent, task, more flags… → its name, once it runs
  local a n
  a=$(start_helper "$@")
  n=$(field name <<<"$a")
  [[ -n $n ]] || { echo "a helper did not start: $a" >&2; return 1; }
  echo "$n"
}

sizes() { grep -c . "$SIZES" 2>/dev/null || true; }
rows_now() { awk 'END { print $3 }' "$SIZES"; }
wait_rows() { # rows, seconds
  local i
  for (( i = 0; i < $2 * 5; i++ )); do
    [[ $(rows_now) == "$1" ]] && return 0
    sleep 0.2
  done
  return 1
}
# One more size since `$1` of them, and it is `$2` rows: nothing in
# between, and nothing for two seconds after.
one_resize_to() { # how many there were, rows, seconds
  wait_rows "$2" "$3" || { echo "    rows are $(rows_now), wanted $2"; return 1; }
  sleep 2
  [[ $(sizes) == $(( $1 + 1 )) && $(rows_now) == "$2" ]] || { echo "    $(( $(sizes) - $1 )) resizes, rows now $(rows_now)"; return 1; }
}
# `$3` more sizes since `$1` of them, each taller than the one before,
# the last `$2` rows.
grows_back_to() { # how many there were, rows, how many more, seconds
  wait_rows "$2" "$4" || { echo "    rows are $(rows_now), wanted $2"; return 1; }
  sleep 2
  [[ $(sizes) == $(( $1 + $3 )) ]] || { echo "    $(( $(sizes) - $1 )) resizes, rows now $(rows_now)"; return 1; }
  tail -n "$3" "$SIZES" | awk 'NR > 1 && $3 <= last { bad = 1 } { last = $3 } END { exit bad }' \
    || { echo "    not each taller: $(tail -n "$3" "$SIZES" | awk '{ print $3 }' | tr '\n' ' ')"; return 1; }
}
strip_now() { grep -a $'\tDEV_STRIP\t' "$LOG" | tail -1 | grep -o $'\tlines=.*' | tr '_' ' '; } # the log writes a space as _
strip_has() { # text…
  local t now
  now=$(strip_now)
  for t in "$@"; do [[ $now == *"$t"* ]] || { echo "    not in the strip: $t — $now"; return 1; }; done
}
wait_strip() { # seconds, text…
  local i s=$1
  shift
  for (( i = 0; i < s * 2; i++ )); do
    strip_has "$@" >/dev/null && return 0
    sleep 0.5
  done
  strip_has "$@"
}
not() { ! "$@"; }
quiet() { "$@" > /dev/null; }
# The rows of the last strip, one a line.  The dump keeps a gap between
# columns as two spaces.
strip_rows() { strip_now | sed $'s/^\tlines=//' | tr '|' '\n'; }
row_is() { # extended regex one row matches
  strip_rows | grep -qE -- "$1" || { echo "    no row matches $1: $(strip_now)"; return 1; }
}
wait_row_is() { # seconds, regex
  local i
  for (( i = 0; i < $1 * 5; i++ )); do row_is "$2" > /dev/null && return 0; sleep 0.2; done
  row_is "$2"
}
# Whether strip text `$1` (its rows joined by |) has `$2` rows, the last
# of them drawn, and first a blank row and main's (`$3` head) or no
# main (under).  A blank last row is an empty field at the end.
rows_drawn() { # strip text, rows, head|under
  local -a r
  IFS='|' read -r -a r <<<"$1|"
  local last=${r[${#r[@]} - 1]}
  (( ${#r[@]} == $2 )) && [[ $last =~ [^[:space:]] ]] || return 1
  if [[ $3 == head ]]; then [[ ! ${r[0]} =~ [^[:space:]] && ${r[1]} == *main* ]]; else [[ $1 != *main* ]]; fi
}
drawn() { # rows, head|under
  local s
  s=$(strip_now)
  s=${s#$'\t'lines=}
  rows_drawn "$s" "$1" "$2" || { echo "    rows: $s"; return 1; }
}
now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
# When a stand-in helper drew the end of its turn (IDLE) or its prompt
# (APPROVE), as it logged it.
drawn_at() { awk -F'\t' -v w="$1" '$1 == "drawn" && $2 == w { t = $3 } END { print t }' "$FAKE_HELPER_LOG"; }
within() { # seconds, since (epoch seconds)
  local took
  [[ -n $2 ]] || { echo "    no time to count from"; return 1; }
  took=$(awk -v a="$2" -v b="$(now)" 'BEGIN { printf "%.1f", b - a }')
  echo "    in ${took}s"
  awk -v t="$took" -v m="$1" 'BEGIN { exit !(t <= m) }'
}
setting() { printf 'appearance.helper_strip = %s\nreclaim.enabled = false\n' "$1" > "$SD/settings.toml"; }

launch_app() { # environment…
  env MARSPOT_SESSION_BIN="$BIN/marspot-session" MARSPOT_CORE_BIN="$BIN/marspot-core" "$@" \
    nohup "$CLI" >> "$(dev_run_log helper-strip)" 2>&1 < /dev/null &
  dev_record_pid $!
}
start_app() { # environment…
  local i
  dev_kill_shell_core
  [[ -f $LOG ]] && cp "$LOG" "$SB/marspot.$(date +%s).log"
  dev_wipe_state
  rm -rf "${SD:?}/sessions" "${SD:?}/retired" "${SIZES:?}"
  mkdir -p "$SD/logs"
  # one window, one pane: no title strip to count in the rows
  python3 - "$SD" <<'PY' || die "could not write the layout"
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1])
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
body = struct.pack('<II', 0xA5505010, 2) + struct.pack('<HH', 0, 1)
body += struct.pack('<HHHH', 1, 1, 0, 1) + struct.pack('<Q', 0) + s('') + s('')
(d / 'shell-state.bin').write_bytes(body)
frames = struct.pack('<IIH', 0xA5505011, 2, 1) + struct.pack('<Idddd', 0, 80.0, 200.0, 1000.0, 700.0)
(d / 'window-state.bin').write_bytes(frames)
PY
  setting "$SETTING"
  launch_app "$@"
  wait_count "$SD" HELLO_ACK 0 90 || die "the core never said hello"
  wait_count "$SD" PANE_CONTROL_CAPS_SEEN 0 30 || die "the shell never heard what the core does with pane control"
  P1=""
  for (( i = 0; i < 100; i++ )); do
    P1=$(pc list --json 2>/dev/null | python3 -c 'import json,sys; ps=json.load(sys.stdin)["result"]["panes"]; print(ps[0]["id"] if ps else "")' 2>/dev/null)
    [[ -n $P1 ]] && break
    sleep 0.5
  done
  [[ -n $P1 ]] || die "no pane came up"
  for (( i = 0; i < 30; i++ )); do pc run "$P1" --timeout 10 true >/dev/null 2>&1 && break; sleep 0.5; done
}
start_parent() { # environment of the session
  local i
  pc send "$P1" "cd $SB/proj && FAKE_SIZE_LOG=$SIZES $* python3 $ROOT/bin/_peerbus_fake_claude.py $REGISTRY 1500 $SB/inbox.log" >/dev/null \
    || die "could not start the stand-in Claude session"
  for (( i = 0; i < 60; i++ )); do
    pc list --json | python3 -c "import json,sys; ps=json.load(sys.stdin)['result']['panes']; sys.exit(0 if any(p['id']==$P1 and p['agent']=='claude' for p in ps) else 1)" && break
    sleep 0.5
  done
  # the parent's pane is settled: nothing resizes it for another reason
  sleep 3
  FULL=$(rows_now)
  [[ -n $FULL ]] || die "the parent logged no size"
  echo "==> the parent's terminal has $FULL rows"
}

# The window server's number for the sandbox's window: the largest one
# its shell has on screen.
window_id() {
  /usr/bin/osascript -l JavaScript -e 'function run(argv) {
  ObjC.import("CoreGraphics");
  var all = ObjC.deepUnwrap(ObjC.castRefToObject($.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly, 0)));
  var mine = all.filter(function (w) { return String(w.kCGWindowOwnerPID) === argv[0] && w.kCGWindowLayer === 0; });
  var area = function (w) { return w.kCGWindowBounds.Width * w.kCGWindowBounds.Height; };
  mine.sort(function (a, b) { return area(b) - area(a); });
  return mine.length ? String(mine[0].kCGWindowNumber) : "";
}' "$(dev_shell_pid)"
}

# With STRIP_SHOTS naming a directory, what the strip and the pane hold,
# and pictures of the sandbox's window only, as `<name>-*`: the window as
# the window server composes it, and the two buffers the shell draws it
# into (a host whose screen is locked gives no picture of a window).  The
# buffers' ids are logged at MARSPOT_LOG=debug.
shot() { # name
  [[ -n ${STRIP_SHOTS:-} ]] || return 0
  local wid id out="$STRIP_SHOTS/$1"
  mkdir -p "$STRIP_SHOTS"
  strip_now > "$out-strip.txt"
  screen "$SD" "$P1" > "$out-screen.txt"
  wid=$(window_id)
  if [[ -n $wid ]]; then
    screencapture -l "$wid" -o -x "$out-window.png" 2> "$out-window.err" || echo "    no picture of window $wid: $(cat "$out-window.err")"
  else
    echo "    the sandbox has no window on screen"
  fi
  for id in $(grep -a 'create_pair' "$LOG" | tail -1 | grep -oE '(front|back)_id=[0-9]+' | cut -d= -f2); do
    python3 "$ROOT/bin/_surface-png.py" "$id" "$out-surface-$id.png" || echo "    buffer $id unread"
  done
}

#!/usr/bin/env bash
#
# The four floating panels (settings, layout, process monitor, agent
# accounts) follow one set of rules for opening, closing, owning input
# and being dragged.  The rules have unit tests; this checks that real
# presses and keys, routed the way a window's are, reach them.
#
# A script cannot press a toolbar button in the sandbox app, so the
# core plays the input itself (`MARSPOT_DEV_PANELS`, sandbox only):
# each step is queued as the events the window would have sent, at
# points read off the live layout, and handled like any others.
#
# What this asserts, from the structured log and from what the panes'
# programs were sent:
#   1. Each panel opens by its toolbar button.
#   2. A panel closes by its close button, Esc, a press outside it,
#      Cmd-W and its own button, and the log names which.
#   3. Opening a second panel closes the first; never two at once.
#   4. A header drag moves the panel by what was asked and leaves it
#      open; reopened, it is back where it started.
#   5. While a panel is open nothing reaches the panes: not a press
#      inside its frame, not the wheel, not a key, not the press that
#      dismisses it.  Each pane runs a recorder that writes down every
#      byte it is sent and asks for mouse reports; after the run it
#      holds exactly the click, wheel and key sent once the last panel
#      had closed -- which also shows the recorder records.
#   6. The app is still up, with no panic and no error logged.
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app
# is never touched.  Exits 0 on pass, non-zero on the first failure.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

RUNLOG=/tmp/marspot-test-panel-behaviour.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
REC_DIR="$MARSPOT_STATE_DIR/recorded"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"

fail() {
  echo "FAIL: $*"
  echo "  --- panel and script events in $APPLOG:"
  grep -E $'\t(PANEL_|DEV_PANELS_)' "$APPLOG" 2>/dev/null | cut -f7- | sed 's/^/    /'
  echo "  --- last 20 lines of $APPLOG:"
  tail -20 "$APPLOG" 2>/dev/null | sed 's/^/    /'
  echo "  --- last 20 lines of $RUNLOG:"
  tail -20 "$RUNLOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() { dev_kill_shell_core; }
trap cleanup EXIT

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" || ! -x "$SESSION_BIN" ]]; then
  echo "==> building release binaries"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 )
fi

cleanup
dev_wipe_state
rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired" "$REC_DIR"
mkdir -p "$MARSPOT_STATE_DIR/logs" "$REC_DIR"
rm -f "$APPLOG"
> "$RUNLOG"

# One window, two panes side by side.  sid 0 = every pane spawns fresh.
python3 - "$MARSPOT_STATE_DIR" <<'PY' || exit 1
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1]); d.mkdir(parents=True, exist_ok=True)
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
def pane(sid=0): return struct.pack('<Q', sid) + s('') + s('')
body  = struct.pack('<II', 0xA5505010, 2)
body += struct.pack('<HH', 0, 1)
body += struct.pack('<HHHH', 2, 1, 0, 2) + pane() + pane()
(d / 'shell-state.bin').write_bytes(body)
frames  = struct.pack('<IIH', 0xA5505011, 2, 1)
frames += struct.pack('<Idddd', 0, 100.0, 200.0, 1400.0, 800.0)
(d / 'window-state.bin').write_bytes(frames)
PY

# What each pane runs instead of a shell: it asks for mouse reports
# and writes down every byte it is sent, unbuffered and unechoed.
RECORDER="$MARSPOT_STATE_DIR/recorder.sh"
cat > "$RECORDER" <<EOF
#!/bin/sh
out="$REC_DIR/pane.\$\$.bytes"
: > "\$out"
stty raw -echo
printf '\\033[?1000h\\033[?1006h'
exec cat >> "\$out"
EOF
chmod +x "$RECORDER"

# The input.  Every panel-closing rule once, a replace, a drag, then
# the three things that must not get past an open panel, then the same
# three with nothing open.  The presses that could reach a program are
# made with shift held: a plain press on a pane selects text and is
# never the program's, so it could not show a leak.
STEPS=(
  wait:3000
  button:settings click:close
  button:layout esc
  button:process shift-click:outside
  button:accounts key:cmd-w
  button:settings button:settings
  button:settings button:layout
  drag:header:120,40
  esc button:layout
  button:process
  scroll:3 shift-click:pane-under type:L click:header
  esc
  shift-click:pane scroll:3 type:x
)
SCRIPT="$(IFS=';'; echo "${STEPS[*]}")"

EXPECTED="OPEN settings offset=0,0
CLOSE settings by=close_button
OPEN layout offset=0,0
CLOSE layout by=escape
OPEN process offset=0,0
CLOSE process by=outside
OPEN accounts offset=0,0
CLOSE accounts by=cmd_w
OPEN settings offset=0,0
CLOSE settings by=toggle
OPEN settings offset=0,0
CLOSE settings by=replaced
OPEN layout offset=0,0
MOVE layout dx=120 dy=40
CLOSE layout by=escape
OPEN layout offset=0,0
CLOSE layout by=replaced
OPEN process offset=0,0
CLOSE process by=escape"

echo "==> launching with the script: $SCRIPT"
SHELL="$RECORDER" MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  MARSPOT_DEV_PANELS="$SCRIPT" \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
disown

wait_for() { # tag, seconds
  local tag="$1" secs="$2" i
  for (( i = 0; i < secs * 10; i++ )); do
    grep -q "$tag" "$APPLOG" 2>/dev/null && return 0
    sleep 0.1
  done
  return 1
}

wait_for CORE_BOOT 60 || fail "the core never booted"
# Before the script has opened anything: what a first launch puts on
# screen is the main window and nothing beside it.
for (( i = 0; i < 50; i++ )); do dev_one_main_window && break; sleep 0.2; done
dev_one_main_window || fail "on screen at launch, as '<layer> <size>' (want one line, layer 0): $(dev_windows_of "$(dev_shell_pid)" | tr '\n' ';')"
echo "    one window on screen, the main one: $(dev_windows_of "$(dev_shell_pid)")"

wait_for DEV_PANELS_DONE 90 || fail "the script never finished"
# The last key has been queued, not yet delivered through the PTY.
sleep 2

# --- the script itself ran, whole ------------------------------------
grep -q $'\tDEV_PANELS_REJECTED\t' "$APPLOG" && fail "the script did not parse"
grep -q $'\tDEV_PANELS_STEP_FAILED\t' "$APPLOG" && fail "a step had nowhere to go"
played=$(grep -c $'\tDEV_PANELS_STEP\t' "$APPLOG")
(( played == ${#STEPS[@]} )) || fail "played $played of ${#STEPS[@]} steps"

# --- 1-4. what opened, what closed it, what moved ---------------------
events="$(python3 - "$APPLOG" <<'PY'
import sys
for line in open(sys.argv[1], errors='replace'):
    cols = line.rstrip('\n').split('\t')
    if len(cols) < 8 or not cols[6].startswith('PANEL_'):
        continue
    f = dict(c.split('=', 1) for c in cols[8:] if '=' in c)
    what = cols[6][len('PANEL_'):]
    tail = {'OPEN': f"offset={f.get('offset')}",
            'CLOSE': f"by={f.get('by')}",
            'MOVE': f"dx={f.get('dx')} dy={f.get('dy')}"}[what]
    print(what, f.get('kind'), tail)
PY
)"
echo "==> panel events, in order:"
printf '%s\n' "$events" | sed 's/^/    /'

for kind in settings layout process accounts; do
  printf '%s\n' "$events" | grep -q "^OPEN $kind " \
    || fail "the $kind panel never opened by its toolbar button"
done
for by in close_button escape outside cmd_w toggle replaced; do
  printf '%s\n' "$events" | grep -q "^CLOSE .* by=$by\$" \
    || fail "no panel was closed by=$by"
done

# Never two at once, and nothing closes that is not the one open.
printf '%s\n' "$events" | awk '
  $1 == "OPEN"  { if (open != "") { print "opened " $2 " over " open; exit 1 } open = $2 }
  $1 == "CLOSE" { if (open != $2) { print "closed " $2 " while [" open "] was open"; exit 1 } open = "" }
  $1 == "MOVE"  { if (open != $2) { print "moved " $2 " while [" open "] was open"; exit 1 } }
  END { if (open != "") { print open " is still open"; exit 1 } }
' || fail "the panels were not one at a time"

printf '%s\n' "$events" | grep -q '^MOVE layout dx=120 dy=40$' \
  || fail "the header drag did not move the layout panel by 120,40"
(( $(printf '%s\n' "$events" | grep -c '^MOVE ') == 1 )) \
  || fail "a press on the header that went nowhere was logged as a move"
printf '%s\n' "$events" | grep -A2 '^MOVE layout' | tail -1 | grep -q '^OPEN layout offset=0,0$' \
  || fail "the layout panel did not reopen where it started"

[[ "$events" == "$EXPECTED" ]] || {
  echo "  --- expected:"
  printf '%s\n' "$EXPECTED" | sed 's/^/    /'
  fail "the panel events are not the ones the script should produce"
}

# --- 5. nothing got past an open panel --------------------------------
python3 - "$REC_DIR" <<'PY' || fail "input reached a pane it should not have, or the recorder missed what it should have"
import pathlib, re, sys
files = sorted(pathlib.Path(sys.argv[1]).glob('pane.*.bytes'))
got = [f.read_bytes() for f in files]
print(f"==> recorded by the panes: {got}")
if len(got) != 2:
    sys.exit(f"expected 2 recorders, found {len(got)}")
heard = [g for g in got if g]
if len(heard) != 1:
    sys.exit("exactly one pane should have been sent anything")
# With nothing open: a press and its release on one cell, the wheel,
# then the key.  Anything before it came through a panel.
want = re.compile(rb'\x1b\[<0;(\d+);(\d+)M\x1b\[<0;\1;\2m(\x1b\[<6[45];\d+;\d+M)+x')
if not want.fullmatch(heard[0]):
    sys.exit("not exactly: press, release, wheel reports, `x`")
PY

# --- 6. still up, and quiet --------------------------------------------
[[ -n "$(dev_shell_pid)" ]] || fail "the shell is gone"
[[ -n "$(dev_core_pids)" ]] || fail "the core is gone"
grep -qi 'panicked' "$APPLOG" "$RUNLOG" && fail "something panicked"
errors="$(grep -c $'\tERROR\t' "$APPLOG")"
(( errors == 0 )) || {
  grep $'\tERROR\t' "$APPLOG" | cut -f7- | sed 's/^/    /'
  fail "$errors error line(s) in the log"
}

echo
echo "PASS — every panel opens by its button and closes by each of its"
echo "       rules, one at a time; a drag moves it and a reopen forgets"
echo "       the move; nothing reaches the panes while one is open."

#!/usr/bin/env bash
#
# The keyboard commands, pressed in a real window: which chords are
# commands, when each answers, and that every key that is not a command
# reaches the program in the pane exactly as it did before there was a
# command table.  Then the same commands chosen from the menu bar.
#
# A script cannot press keys in the sandbox app, so the core plays them
# itself (`MARSPOT_DEV_PANELS`, sandbox only): each `key:<chord>` step
# is queued as the press and release the window would have sent, and
# handled like any other.  That is the path from the core inward.
#
# Nor can a script pull a menu down on a machine with nobody at it, so
# in a second launch the shell chooses its own items
# (`MARSPOT_DEV_MENU`, sandbox only): each step finds the item in the
# application's installed main menu and has AppKit perform it, which
# sends the item's action to its target as a choice by pointer does.
# Covered: the menu item, its target and action, `runCommand:`, the
# chord replayed as a key, and that key's dispatch to a window.  Not
# covered: the pointer, the menu opening and its tracking, and finding
# the key window -- the app is not the active one on a machine nobody
# is at, so it has no key window and a script's choice is sent to the
# first window instead.
#
# What this asserts, from the structured log and from what the panes'
# programs were sent (each pane runs a recorder that writes down every
# byte):
#   1. Keys the terminal's programs own -- Ctrl-C, Ctrl-L, Option-b,
#      Option-f, Escape, F5, the arrows -- arrive byte for byte as they
#      did before the command table existed.  The expected bytes below
#      were recorded from that build with these same steps.
#   2. Each panel's chord opens its panel, and the next one's replaces
#      it; a panel's own chord, Cmd-W and Esc close it.
#   3. While a panel is open the pane commands do nothing and nothing
#      reaches a pane: Cmd-V does not paste, Cmd-2 / Cmd-] / Cmd-[ do
#      not move focus, Cmd-T adds no pane, a letter and Ctrl-C go
#      nowhere.
#   4. With no panel open, Cmd-2 / Cmd-] / Cmd-[ / Cmd-1 move focus in
#      sidebar order and round the ends, Cmd-9 with two panes does
#      nothing, Cmd-V pastes, Cmd-W does nothing, Cmd-F opens the
#      search bar in place of an open panel, and Cmd-T adds a pane that
#      Cmd-3 then focuses.  Where typing lands shows where focus is.
#   5. The app is still up, with no panic and no error logged.
#   6. The installed menu bar is the command table: every row has one
#      item a person can see, under its own title in its own menu, and
#      no item that runs a command lacks a row.  Items AppKit keeps
#      hidden beside one of ours are printed, not counted.
#   7. Every item that runs a command of ours, chosen from the menu,
#      reaches a window and logs what its chord logged in 2-4.  The
#      seven items that are AppKit's own actions (About, Hide, Hide
#      Others, Show All, Quit, Minimize, Full Screen) are held to the
#      table in 6 and not chosen: they act on the machine's whole
#      session, which other suites share.
#
# The paste check puts a marker on a pasteboard of this run's own, which
# the sandbox app is told to copy and paste through.
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app
# is never touched.  Exits 0 on pass, non-zero on the first failure.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

RUNLOG=/tmp/marspot-test-commands.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
REC_DIR="$MARSPOT_STATE_DIR/recorded"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"
MARKER="marspot-paste-$$"
EVENTS=$'\t(PANEL_|PANE_FOCUS|PANE_ADDED|L2_SEARCH_OPEN|DEV_PANELS_|DEV_MENU_|MENU_COMMAND)'

fail() {
  echo "FAIL: $*"
  echo "  --- command and script events in $APPLOG:"
  grep -E "$EVENTS" "$APPLOG" 2>/dev/null | cut -f7- | sed 's/^/    /'
  echo "  --- last 20 lines of $APPLOG:"
  tail -20 "$APPLOG" 2>/dev/null | sed 's/^/    /'
  echo "  --- last 20 lines of $RUNLOG:"
  tail -20 "$RUNLOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

dev_own_pasteboard
cleanup() {
  dev_kill_shell_core
  dev_pasteboard_release
}
trap cleanup EXIT

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" || ! -x "$SESSION_BIN" ]]; then
  echo "==> building release binaries"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 )
fi

dev_kill_shell_core
dev_wipe_state
rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired" "$REC_DIR"
mkdir -p "$MARSPOT_STATE_DIR/logs" "$REC_DIR"
rm -f "$APPLOG"
> "$RUNLOG"

# One window, two panes side by side, the first focused.  sid 0 = every
# pane spawns fresh.
seed_state() {
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
}
seed_state

# What each pane runs instead of a shell: it writes down every byte it
# is sent, unbuffered and unechoed.
RECORDER="$MARSPOT_STATE_DIR/recorder.sh"
cat > "$RECORDER" <<EOF
#!/bin/sh
out="$REC_DIR/pane.\$\$.bytes"
: > "\$out"
stty raw -echo
exec cat >> "\$out"
EOF
chmod +x "$RECORDER"

dev_pasteboard_set "$MARKER" || fail "could not set the pasteboard"
[[ "$(dev_pasteboard_get)" == "$MARKER" ]] || fail "the pasteboard does not hold what was put on it"

STEPS=(
  wait:3000
  # 1. the program's own keys, then a letter to mark where they end
  key:ctrl-c key:ctrl-l key:alt-b key:alt-f esc key:f5 key:up key:down key:left key:right
  type:a
  # 2. each panel by its chord, each over the one before
  key:cmd-, key:cmd-shift-l key:cmd-shift-p key:cmd-shift-c key:cmd-/
  # 3. pane commands and plain keys under a panel
  key:cmd-v key:cmd-2 key:cmd-] key:cmd-[ key:cmd-b key:cmd-t type:z key:ctrl-c key:f5
  # 2. closed by its own chord, by Cmd-W, by Esc
  key:cmd-/
  key:cmd-, key:cmd-w
  key:cmd-shift-l esc
  # 4. focus, shown by where the next letter lands
  key:cmd-2 type:b
  key:cmd-] type:c
  key:cmd-[ type:d
  key:cmd-1 type:e
  key:cmd-9 type:f
  key:cmd-v
  key:cmd-w type:g
  # the search bar takes a panel's place; Esc closes it, not the pane's
  key:cmd-shift-p key:cmd-f esc type:i
  key:cmd-t wait:6000
  key:cmd-3 type:h
)
SCRIPT="$(IFS=';'; echo "${STEPS[*]}")"

EXPECTED="PANEL_OPEN settings
PANEL_CLOSE settings replaced
PANEL_OPEN layout
PANEL_CLOSE layout replaced
PANEL_OPEN process
PANEL_CLOSE process replaced
PANEL_OPEN accounts
PANEL_CLOSE accounts replaced
PANEL_OPEN shortcuts
PANEL_CLOSE shortcuts toggle
PANEL_OPEN settings
PANEL_CLOSE settings cmd_w
PANEL_OPEN layout
PANEL_CLOSE layout escape
PANE_FOCUS 1
PANE_FOCUS 0
PANE_FOCUS 1
PANE_FOCUS 0
PANEL_OPEN process
PANEL_CLOSE process replaced
L2_SEARCH_OPEN 0
PANE_ADDED 3
PANE_FOCUS 2"

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

wait_for DEV_PANELS_DONE 120 || fail "the script never finished"
# The last key has been queued, not yet delivered through the PTY.
sleep 2

# --- the script itself ran, whole ------------------------------------
grep -q $'\tDEV_PANELS_REJECTED\t' "$APPLOG" && fail "the script did not parse"
grep -q $'\tDEV_PANELS_STEP_FAILED\t' "$APPLOG" && fail "a step had nowhere to go"
played=$(grep -c $'\tDEV_PANELS_STEP\t' "$APPLOG")
(( played == ${#STEPS[@]} )) || fail "played $played of ${#STEPS[@]} steps"

# --- 2-4. what opened, closed, took focus and was added, in order -----
command_events() {
python3 - "$APPLOG" <<'PY'
import sys
for line in open(sys.argv[1], errors='replace'):
    cols = line.rstrip('\n').split('\t')
    if len(cols) < 8:
        continue
    tag = cols[6]
    f = dict(c.split('=', 1) for c in cols[8:] if '=' in c)
    if tag == 'PANEL_OPEN':
        print(tag, f.get('kind'))
    elif tag == 'PANEL_CLOSE':
        print(tag, f.get('kind'), f.get('by'))
    elif tag == 'PANE_FOCUS':
        print(tag, f.get('pane'))
    elif tag == 'PANE_ADDED':
        print(tag, f.get('panes'))
    elif tag == 'L2_SEARCH_OPEN':
        print(tag, f.get('pane_idx'))
PY
}
events="$(command_events)"
echo "==> command events, in order:"
printf '%s\n' "$events" | sed 's/^/    /'

for kind in settings layout process accounts shortcuts; do
  printf '%s\n' "$events" | grep -q "^PANEL_OPEN $kind\$" \
    || fail "the $kind panel never opened by its chord"
done
(( $(printf '%s\n' "$events" | grep -c '^PANE_ADDED ') == 1 )) \
  || fail "Cmd-T under a panel added a pane, or Cmd-T with none added no pane"
[[ "$events" == "$EXPECTED" ]] || {
  echo "  --- expected:"
  printf '%s\n' "$EXPECTED" | sed 's/^/    /'
  fail "the command events are not the ones the script should produce"
}

# --- 1, 3, 4. what each pane's program was sent ------------------------
python3 - "$REC_DIR" "$MARKER" <<'PY' || fail "a pane was sent something other than what the keys should have sent it"
import pathlib, sys
files = sorted(pathlib.Path(sys.argv[1]).glob('pane.*.bytes'))
marker = sys.argv[2].encode()
got = sorted(f.read_bytes() for f in files)
for g in got:
    print(f"==> recorded by a pane: {g!r}")
# Ctrl-C, Ctrl-L, Option-b, Option-f, Escape, F5, up, down, left, right:
# recorded with these steps from the build before the command table.
own = bytes.fromhex('030c1b621b661b1b5b31357e1b5b411b5b421b5b441b5b43')
want = sorted([
    # the first pane: the program's keys, then every letter typed while
    # it had focus, the paste, and nothing from under a panel
    own + b'a' + b'c' + b'e' + b'f' + marker + b'g' + b'i',
    # the second pane: focused by Cmd-2 and by Cmd-[
    b'b' + b'd',
    # the pane Cmd-T added, focused by Cmd-3
    b'h',
])
if got != want:
    for w in want:
        print(f"    wanted: {w!r}")
    sys.exit(1)
PY

# --- 5. still up, and quiet --------------------------------------------
[[ -n "$(dev_shell_pid)" ]] || fail "the shell is gone"
[[ -n "$(dev_core_pids)" ]] || fail "the core is gone"
grep -qi 'panicked' "$APPLOG" "$RUNLOG" && fail "something panicked"
errors="$(grep -c $'\tERROR\t' "$APPLOG")"
(( errors == 0 )) || {
  grep $'\tERROR\t' "$APPLOG" | cut -f7- | sed 's/^/    /'
  fail "$errors error line(s) in the log"
}

# --- 6, 7. the same commands, chosen from the menu bar ------------------
dev_kill_shell_core
dev_wipe_state
rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired" "$REC_DIR"
mkdir -p "$MARSPOT_STATE_DIR/logs" "$REC_DIR"
rm -f "$APPLOG"
seed_state
dev_pasteboard_set "$MARKER" || fail "could not set the pasteboard"

MENU=(
  # the script starts with the menu bar, well before the panes are up
  'wait:8000'
  # each panel over the one before, the last closed by its own item
  'Settings' 'Layout' 'Processes' 'Accounts' 'Shortcuts' 'Shortcuts'
  'Settings' 'ClosePanel'
  # focus: the second pane, round the end, back, the first
  'FocusPane(1)' 'NextPane' 'PrevPane' 'FocusPane(0)'
  'Paste' 'Copy' 'ToggleSidebar' 'ToggleSidebar'
  'NewPane' 'wait:6000' 'FocusPane(2)'
  # there is no fourth pane or later
  'FocusPane(3)' 'FocusPane(4)' 'FocusPane(5)' 'FocusPane(6)' 'FocusPane(7)' 'FocusPane(8)'
  'Find' 'NewWindow' 'wait:3000'
)
MENU_SCRIPT="$(IFS=';'; echo "${MENU[*]}")"
MENU_EXPECTED="PANEL_OPEN settings
PANEL_CLOSE settings replaced
PANEL_OPEN layout
PANEL_CLOSE layout replaced
PANEL_OPEN process
PANEL_CLOSE process replaced
PANEL_OPEN accounts
PANEL_CLOSE accounts replaced
PANEL_OPEN shortcuts
PANEL_CLOSE shortcuts toggle
PANEL_OPEN settings
PANEL_CLOSE settings cmd_w
PANE_FOCUS 1
PANE_FOCUS 0
PANE_FOCUS 1
PANE_FOCUS 0
PANE_ADDED 3
PANE_FOCUS 2
L2_SEARCH_OPEN 2"

echo "==> launching with the menu script: $MENU_SCRIPT"
SHELL="$RECORDER" MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  MARSPOT_DEV_MENU="$MENU_SCRIPT" \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
disown

wait_for DEV_MENU_DONE 120 || fail "the menu script never finished"
sleep 2

field() { # tag, key -- the value of key in the first line tagged tag
  grep -m1 $'\t'"$1"$'\t' "$APPLOG" | tr '\t' '\n' | sed -n "s/^$2=//p"
}
[[ -n "$(field DEV_MENU_AUDIT rows)" ]] || fail "the menu bar was never held to the table"
echo "==> the menu bar against the table: $(grep -m1 $'\tDEV_MENU_AUDIT\t' "$APPLOG" | cut -f9- | tr '\t' ' ')"
echo "==> hidden items AppKit added: $(field DEV_MENU_AUDIT hidden)"
[[ -z "$(field DEV_MENU_AUDIT wrong)" ]] || fail "the menu bar is not the table: $(field DEV_MENU_AUDIT wrong)"
[[ "$(field DEV_MENU_AUDIT strays)" == 0 ]] || fail "an item runs a command that has no row"
commands="$(field DEV_MENU_AUDIT commands)"
(( $(field DEV_MENU_AUDIT rows) == commands + 7 )) || fail "the table is not $commands commands of ours and AppKit's seven"

grep -q $'\tDEV_MENU_STEP_FAILED\t' "$APPLOG" && fail "a step named an item the menu bar does not have"
grep $'\tDEV_MENU_STEP\t' "$APPLOG" | grep -Eqv $'\tenabled=1(\t|$)' && fail "an item was greyed out when chosen"
chosen="$(grep $'\tMENU_COMMAND\t' "$APPLOG" | tr '\t' '\n' | sed -n 's/^command=//p')"
want_chosen="$(printf '%s\n' "${MENU[@]}" | grep -v '^wait:')"
[[ "$chosen" == "$want_chosen" ]] || fail "the items that ran are not the ones chosen: $(printf '%s' "$chosen" | tr '\n' ' ')"
grep $'\tMENU_COMMAND\t' "$APPLOG" | grep -Eqv $'\tdelivered=1(\t|$)' && fail "a chosen item reached no window"
echo "==> chosen with a key window: $(grep $'\tDEV_MENU_STEP\t' "$APPLOG" | grep -c $'\tkey_window=1') of $(grep -c $'\tDEV_MENU_STEP\t' "$APPLOG") (without one the key-window lookup is not exercised: a script's choice goes to the first window)"
(( $(printf '%s\n' "$chosen" | sort -u | wc -l) == commands )) \
  || fail "chose $(printf '%s\n' "$chosen" | sort -u | wc -l | tr -d ' ') different commands; the menu bar has $commands"

menu_events="$(command_events)"
echo "==> command events from the menu, in order:"
printf '%s\n' "$menu_events" | sed 's/^/    /'
[[ "$menu_events" == "$MENU_EXPECTED" ]] || {
  echo "  --- expected:"
  printf '%s\n' "$MENU_EXPECTED" | sed 's/^/    /'
  fail "choosing from the menu did not do what the chords do"
}
pasted="$(cat "$REC_DIR"/pane.*.bytes | grep -c "$MARKER")"
(( pasted == 1 )) || fail "Paste from the menu reached a pane $pasted times"
windows="$(dev_windows_of "$(dev_shell_pid)" | grep -c '^0 ')"
(( windows == 2 )) || fail "New Window from the menu left $windows windows on screen"

[[ -n "$(dev_shell_pid)" ]] || fail "the shell is gone after the menu script"
[[ -n "$(dev_core_pids)" ]] || fail "the core is gone after the menu script"
grep -qi 'panicked' "$APPLOG" "$RUNLOG" && fail "something panicked during the menu script"
errors="$(grep -c $'\tERROR\t' "$APPLOG")"
(( errors == 0 )) || fail "$errors error line(s) in the log during the menu script"

echo
echo "PASS — the program's own keys arrive unchanged; every panel opens"
echo "       by its chord and is replaced by the next; pane commands do"
echo "       nothing under a panel and move focus, paste and add a pane"
echo "       with none open; the menu bar is the command table, and each"
echo "       of its $commands commands chosen there does what its chord does."

#!/usr/bin/env bash
#
# A font change in a real window: the settings file is edited while
# the app runs, and the core rebuilds its fonts, relays every pane out
# and tells the pane's program its new size, without anything
# restarting.
#
# The suite changes `appearance.font_size` and `appearance.font_family`
# by writing the settings file in the sandbox's state directory, which
# is what a hand edit does and what the settings panel's click ends in.
# The click itself is not driven here.
#
# What this asserts, from the structured log and from the size the
# pane's program reads from its terminal (`stty size`, twice a second,
# written down):
#   1. At start no font change is logged.
#   2. font_size 12 -> 24: the core logs FONTS_ADOPTED with size_pt=24.0
#      within 2 s of the write, and the program's columns and rows drop
#      to about half.
#   3. font_family Menlo (size back to 12): FONTS_ADOPTED family=Menlo
#      within 2 s, and the program reads more rows, Menlo's line being
#      shorter than Monaco's.
#   4. A family this Mac does not have: logged as chosen, drawn in the
#      default, and the grid comes back to the size it had at start.
#   5. Back to the defaults: the program reads exactly the size it had
#      at start.
#   6. The pane's session process is the same pid from start to end, and
#      the app is still up with no panic and no error logged.
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app
# is never touched.  Exits 0 on pass, non-zero on the first failure.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

RUNLOG="$(dev_run_log test-font-switch)"
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
REC_DIR="$MARSPOT_STATE_DIR/recorded"
SETTINGS="$MARSPOT_STATE_DIR/settings.toml"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"

fail() {
  echo "FAIL: $*"
  echo "  --- font and settings events in $APPLOG:"
  grep -E $'\t(FONTS_ADOPTED|SETTINGS_RELOADED)\t' "$APPLOG" 2>/dev/null | cut -f1,4,7- | sed 's/^/    /'
  echo "  --- the program's last sizes:"
  tail -5 "$REC_DIR"/pane.*.sizes 2>/dev/null | sed 's/^/    /'
  echo "  --- last 20 lines of $RUNLOG:"
  tail -20 "$RUNLOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

trap dev_kill_shell_core EXIT

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" || ! -x "$SESSION_BIN" ]]; then
  echo "==> building release binaries"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 )
fi

dev_kill_shell_core
dev_wipe_state
rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired" "$REC_DIR"
mkdir -p "$MARSPOT_STATE_DIR/logs" "$REC_DIR"
rm -f "$APPLOG" "$SETTINGS"
> "$RUNLOG"

# One window, one pane.  sid 0 = the pane spawns fresh.
python3 - "$MARSPOT_STATE_DIR" <<'PY' || exit 1
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1]); d.mkdir(parents=True, exist_ok=True)
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
def pane(sid=0): return struct.pack('<Q', sid) + s('') + s('')
body  = struct.pack('<II', 0xA5505010, 2)
body += struct.pack('<HH', 0, 1)
body += struct.pack('<HHHH', 1, 1, 0, 1) + pane()
(d / 'shell-state.bin').write_bytes(body)
frames  = struct.pack('<IIH', 0xA5505011, 2, 1)
frames += struct.pack('<Idddd', 0, 100.0, 200.0, 1000.0, 600.0)
(d / 'window-state.bin').write_bytes(frames)
PY

# What the pane runs instead of a shell: twice a second it reads the
# size of its terminal and writes it down, `rows cols` a line.
SIZER="$MARSPOT_STATE_DIR/sizer.sh"
cat > "$SIZER" <<EOF
#!/bin/sh
out="$REC_DIR/pane.\$\$.sizes"
: > "\$out"
while :; do
  stty size >> "\$out"
  sleep 0.5
done
EOF
chmod +x "$SIZER"

echo "==> launching"
SHELL="$SIZER" MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
disown

last_size() { tail -1 "$REC_DIR"/pane.*.sizes 2>/dev/null; }

# Wait until the program's latest size satisfies the python expression
# $1 over `rows` and `cols`; $2 seconds.
wait_size() {
  local i r c
  for (( i = 0; i < $2 * 10; i++ )); do
    read -r r c <<< "$(last_size)"
    if [[ -n "${c:-}" ]] && python3 -c "import sys; rows, cols = $r, $c; sys.exit(0 if ($1) else 1)"; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

# Wait until the core logs FONTS_ADOPTED with field $1 = $2 after line
# $3 of the log; prints how long it took, in milliseconds since $4.
wait_adopted() { # key, value, after_line, since_ms, seconds
  python3 - "$APPLOG" "$1" "$2" "$3" "$4" "$5" <<'PY'
import sys, time
path, key, want, after, since, secs = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5]), float(sys.argv[6])
deadline = time.time() + secs
while time.time() <= deadline:
    try:
        lines = open(path, errors='replace').read().split('\n')[after:]
    except FileNotFoundError:
        lines = []
    for line in lines:
        cols = line.split('\t')
        if len(cols) < 8 or cols[6] != 'FONTS_ADOPTED' or cols[3] != 'core':
            continue
        f = dict(c.split('=', 1) for c in cols[8:] if '=' in c)
        if f.get(key) == want:
            print(f"{int(cols[1]) - since} ms, cell {f.get('cell_w')}x{f.get('cell_h')}")
            sys.exit(0)
    time.sleep(0.05)
print("not logged")
sys.exit(1)
PY
}

log_lines() { wc -l < "$APPLOG" | tr -d ' '; }
now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }

# Put the lines given in force the way an editor saving the file does:
# a whole new file under the same name.
set_settings() {
  printf '%s\n' "$@" > "$SETTINGS.new" && mv "$SETTINGS.new" "$SETTINGS"
}

# --- 1. at start --------------------------------------------------------
wait_size 'rows > 0 and cols > 0' 60 || fail "the pane's program never read its size"
sleep 1
read -r rows0 cols0 <<< "$(last_size)"
sessions_at_start="$(dev_session_pids | sort | tr '\n' ' ')"
[[ -n "$sessions_at_start" ]] || fail "no session process is registered"
echo "==> at start the program reads $rows0 rows x $cols0 cols; session pid(s): $sessions_at_start"
grep -q $'\tFONTS_ADOPTED\t' "$APPLOG" && fail "a font change was logged before anything changed"

# --- 2. a larger size ---------------------------------------------------
after=$(log_lines); since=$(now_ms)
set_settings 'appearance.font_size = 24'
took="$(wait_adopted size_pt 24.0 "$after" "$since" 2)" || fail "size 24: the core did not adopt it ($took)"
wait_size "cols <= $cols0 * 0.55 + 1 and rows <= $rows0 * 0.6 + 1" 5 \
  || fail "size 24: the program still reads $(last_size), from $rows0 $cols0"
echo "==> size 24 adopted in $took; the program reads $(last_size)"

# --- 3. another family --------------------------------------------------
after=$(log_lines); since=$(now_ms)
set_settings 'appearance.font_family = Menlo'
took="$(wait_adopted family Menlo "$after" "$since" 2)" || fail "Menlo: the core did not adopt it ($took)"
# back at 12 points, and Menlo's line is shorter than Monaco's
wait_size "rows > $rows0" 5 || fail "Menlo: the program reads $(last_size), not more rows than $rows0"
echo "==> Menlo adopted in $took; the program reads $(last_size)"

# --- 4. a family this Mac does not have ---------------------------------
after=$(log_lines); since=$(now_ms)
set_settings 'appearance.font_family = No Such Family 7f3a'
# the log writes a field's spaces as underscores
took="$(wait_adopted family 'No_Such_Family_7f3a' "$after" "$since" 2)" \
  || fail "a missing family: the core did not log it ($took)"
wait_size "rows == $rows0 and cols == $cols0" 5 \
  || fail "a missing family is not drawn in the default: the program reads $(last_size), not $rows0 $cols0"
echo "==> a missing family is drawn in the default ($took); the program reads $(last_size)"

# --- 5. back to the defaults --------------------------------------------
after=$(log_lines); since=$(now_ms)
set_settings 'appearance.font_size = 24'
wait_adopted size_pt 24.0 "$after" "$since" 2 > /dev/null || fail "size 24 again: not adopted"
after=$(log_lines); since=$(now_ms)
set_settings '# defaults'
took="$(wait_adopted size_pt 12.0 "$after" "$since" 2)" || fail "defaults: the core did not adopt them ($took)"
wait_size "rows == $rows0 and cols == $cols0" 5 \
  || fail "defaults: the program reads $(last_size), not $rows0 $cols0"
echo "==> defaults adopted in $took; the program reads $(last_size) again"

# --- 6. nothing restarted, still up, quiet ------------------------------
sessions_at_end="$(dev_session_pids | sort | tr '\n' ' ')"
[[ "$sessions_at_end" == "$sessions_at_start" ]] \
  || fail "the session process changed: $sessions_at_start -> $sessions_at_end"
(( $(ls "$REC_DIR"/pane.*.sizes | wc -l) == 1 )) || fail "the pane's program was started more than once"
[[ -n "$(dev_shell_pid)" ]] || fail "the shell is gone"
[[ -n "$(dev_core_pids)" ]] || fail "the core is gone"
grep -qi 'panicked' "$APPLOG" "$RUNLOG" && fail "something panicked"
errors="$(grep -c $'\tERROR\t' "$APPLOG")"
(( errors == 0 )) || {
  grep $'\tERROR\t' "$APPLOG" | cut -f7- | sed 's/^/    /'
  fail "$errors error line(s) in the log"
}

echo
echo "PASS — five font changes by the settings file reached the grid and"
echo "       the pane's program with nothing restarted."

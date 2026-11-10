#!/usr/bin/env bash
#
# A theme change in a real window: the settings file is edited while
# the app runs, and the shell, the core and the pane's session all
# follow it without anything restarting.
#
# The suite changes `appearance.theme` by writing the settings file in
# the sandbox's state directory, which is what a hand edit does and
# what the settings panel's click ends in.  The click itself is not
# driven here.
#
# What this asserts, from the structured log and from what the pane's
# program is told when it asks the terminal for its background colour
# (`OSC 11 ; ?`, asked twice a second and written down):
#   1. Dark at start: no theme change is logged and the answer is the
#      dark background.
#   2. dark -> light -> contrast -> dark, each by editing the file: the
#      shell and the core each log THEME_CHANGED with that theme and
#      by=setting within 2 s of the write, the session within 4 s, and
#      the program's next answers are the new background.
#   3. `system`: the setting is switched to follow the system, and the
#      system's appearance is changed.  The real appearance of the
#      machine is not touched: the shell reads a stand-in file instead
#      (MARSPOT_DEV_SYSTEM_APPEARANCE, sandbox only), so what is proved
#      is that a change of the resolved appearance reaches every
#      process, not that AppKit reports one.  The shell and the core
#      log THEME_CHANGED by=system within 3 s (the shell's tick, then
#      the core's) and the program's answer follows.
#   4. The pane's session process is the same pid from start to end.
#   5. The app is still up, with no panic and no error logged.
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app
# is never touched.  Exits 0 on pass, non-zero on the first failure.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

RUNLOG=/tmp/marspot-test-theme-switch.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
REC_DIR="$MARSPOT_STATE_DIR/recorded"
SETTINGS="$MARSPOT_STATE_DIR/settings.toml"
APPEARANCE="$MARSPOT_STATE_DIR/dev-system-appearance"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"

# The terminal's background in each theme, as OSC 11 answers it.
bg_of() {
  case "$1" in
    dark) echo 'rgb:0189/020c/0395' ;;
    light) echo 'rgb:ffff/ffff/ffff' ;;
    contrast) echo 'rgb:0000/0000/0000' ;;
  esac
}

fail() {
  echo "FAIL: $*"
  echo "  --- theme and settings events in $APPLOG:"
  grep -E $'\t(THEME_CHANGED|SETTINGS_RELOADED)\t' "$APPLOG" 2>/dev/null | cut -f1,4,7- | sed 's/^/    /'
  echo "  --- the program's last answers:"
  tail -5 "$REC_DIR"/pane.*.answers 2>/dev/null | sed 's/^/    /'
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
rm -f "$APPLOG" "$SETTINGS" "$MARSPOT_STATE_DIR/system-theme"
> "$RUNLOG"
echo dark > "$APPEARANCE"

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

# What the pane runs instead of a shell: twice a second it asks the
# terminal for its background colour and writes the answer down, one
# line each, with the time it was given.
ASKER="$MARSPOT_STATE_DIR/asker.sh"
cat > "$ASKER" <<EOF
#!/bin/sh
out="$REC_DIR/pane.\$\$.answers"
: > "\$out"
stty raw -echo min 0 time 3
while :; do
  printf '\\033]11;?\\007'
  answer=\$(dd bs=64 count=1 2>/dev/null | cat -v)
  printf '%s %s\\n' "\$(date +%s)" "\$answer" >> "\$out"
  sleep 0.5
done
EOF
chmod +x "$ASKER"

echo "==> launching"
SHELL="$ASKER" MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  MARSPOT_DEV_SYSTEM_APPEARANCE="$APPEARANCE" \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
disown

# The answers file's last line, without its timestamp.
last_answer() {
  tail -1 "$REC_DIR"/pane.*.answers 2>/dev/null | cut -d' ' -f2-
}

# Wait until the program's latest answer is theme $1's background.
wait_answer() { # theme, seconds
  local want i
  want="^[]11;$(bg_of "$1")^G"
  for (( i = 0; i < $2 * 10; i++ )); do
    [[ "$(last_answer)" == "$want" ]] && return 0
    sleep 0.1
  done
  return 1
}

# Wait until the shell, the core and the session have each logged
# THEME_CHANGED for theme $1 chosen by $2, after line $3 of the log;
# print how long each took, in milliseconds since $4 (epoch ms).  The
# shell and the core have $5 seconds; the session two more.
wait_theme() { # theme, by, after_line, since_ms, seconds
  python3 - "$APPLOG" "$1" "$2" "$3" "$4" "$5" <<'PY'
import sys, time
path, theme, by, after, since, secs = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5]), float(sys.argv[6])
deadline = time.time() + secs
SESSION_GRACE = 2.0
while True:
    seen = {}
    try:
        lines = open(path, errors='replace').read().split('\n')[after:]
    except FileNotFoundError:
        lines = []
    for line in lines:
        cols = line.split('\t')
        if len(cols) < 8 or cols[6] != 'THEME_CHANGED':
            continue
        f = dict(c.split('=', 1) for c in cols[8:] if '=' in c)
        if f.get('theme') == theme and f.get('by') == by:
            seen.setdefault(cols[3], int(cols[1]))
    if all(c in seen for c in ('shell', 'core', 'session')):
        took = {c: seen[c] - since for c in ('shell', 'core', 'session')}
        print(', '.join(f"{c} {ms} ms" for c, ms in took.items()))
        # the session is given the grace: it looks once a second, and
        # only when its loop wakes
        sys.exit(0 if max(took['shell'], took['core']) <= secs * 1000 else 1)
    if time.time() > deadline + SESSION_GRACE:
        print(f"only {sorted(seen)} logged it")
        sys.exit(1)
    time.sleep(0.05)
PY
}

log_lines() { wc -l < "$APPLOG" | tr -d ' '; }
now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }

# Put `appearance.theme = $1` in force the way an editor saving the
# file does: a whole new file under the same name.
set_theme() {
  printf 'appearance.theme = %s\n' "$1" > "$SETTINGS.new" && mv "$SETTINGS.new" "$SETTINGS"
}

# --- 1. dark at start ---------------------------------------------------
wait_answer dark 60 || fail "the pane's program never got the dark background as its answer (last: $(last_answer))"
sessions_at_start="$(dev_session_pids | sort | tr '\n' ' ')"
[[ -n "$sessions_at_start" ]] || fail "no session process is registered"
echo "==> dark at start; the program is told $(bg_of dark); session pid(s): $sessions_at_start"
grep -q $'\tTHEME_CHANGED\t' "$APPLOG" && fail "a theme change was logged before anything changed"

# --- 2. three switches by the settings file -----------------------------
for theme in light contrast dark; do
  after=$(log_lines)
  since=$(now_ms)
  set_theme "$theme"
  took="$(wait_theme "$theme" setting "$after" "$since" 2)" \
    || fail "$theme: not logged in time by the shell, the core and the session ($took)"
  wait_answer "$theme" 5 \
    || fail "$theme: the program is still told $(last_answer), not $(bg_of "$theme")"
  echo "==> $theme by the settings file: $took; the program is told $(bg_of "$theme")"
done

# --- 3. following the system --------------------------------------------
# The stand-in says dark and the theme in force is dark, so switching
# the setting to `system` changes nothing that is drawn.
after=$(log_lines)
set_theme system
sleep 2.5
tail -n +"$((after + 1))" "$APPLOG" | grep -q $'\tTHEME_CHANGED\t' \
  && fail "switching to system changed the theme while the system's was the one already in force"
wait_answer dark 3 || fail "system (dark): the program is told $(last_answer)"
for theme in light contrast; do
  after=$(log_lines)
  since=$(now_ms)
  echo "$theme" > "$APPEARANCE"
  took="$(wait_theme "$theme" system "$after" "$since" 3)" \
    || fail "system -> $theme: not logged in time by the shell, the core and the session ($took)"
  wait_answer "$theme" 5 \
    || fail "system -> $theme: the program is still told $(last_answer), not $(bg_of "$theme")"
  echo "==> $theme by the system's appearance (stand-in file): $took; the program is told $(bg_of "$theme")"
done
[[ "$(cat "$MARSPOT_STATE_DIR/system-theme" 2>/dev/null)" == contrast ]] \
  || fail "the shell did not publish what the system resolves to"

# --- 4. nothing restarted -----------------------------------------------
sessions_at_end="$(dev_session_pids | sort | tr '\n' ' ')"
[[ "$sessions_at_end" == "$sessions_at_start" ]] \
  || fail "the session process changed: $sessions_at_start -> $sessions_at_end"
(( $(ls "$REC_DIR"/pane.*.answers | wc -l) == 1 )) || fail "the pane's program was started more than once"

# --- 5. still up, and quiet ----------------------------------------------
[[ -n "$(dev_shell_pid)" ]] || fail "the shell is gone"
[[ -n "$(dev_core_pids)" ]] || fail "the core is gone"
grep -qi 'panicked' "$APPLOG" "$RUNLOG" && fail "something panicked"
errors="$(grep -c $'\tERROR\t' "$APPLOG")"
(( errors == 0 )) || {
  grep $'\tERROR\t' "$APPLOG" | cut -f7- | sed 's/^/    /'
  fail "$errors error line(s) in the log"
}

echo
echo "PASS — five theme changes reached the shell, the core and the"
echo "       pane's session with nothing restarted: three by the settings"
echo "       file, two by the system's appearance (stand-in file; the"
echo "       machine's real appearance was not changed)."

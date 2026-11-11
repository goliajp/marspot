#!/usr/bin/env bash
#
# A language change in a real window: the settings file is edited while
# the app runs, and the shell and the core both follow it without
# anything restarting.
#
# The suite changes `appearance.language` by writing the settings file
# in the sandbox's state directory, which is what a hand edit does and
# what the settings panel's click ends in.  The click itself is not
# driven here.
#
# What this asserts, from the structured log:
#   1. English at start: no language change is logged.
#   2. en -> ja -> zh-Hans -> en, each by editing the file: the shell
#      and the core each log LANGUAGE_CHANGED with that language and
#      by=setting within 2 s of the write, and the shell's MENU_BAR
#      line — the menu bar read back from the application's main menu,
#      a sandbox-only line — carries that language's menu titles.
#   3. `system`: the setting is switched to follow the system, and the
#      system's language list is changed.  The machine's real setting
#      is not touched: the shell reads a stand-in file instead
#      (MARSPOT_DEV_SYSTEM_LANGUAGE, sandbox only), so what is proved is
#      that a change of the resolved language reaches both processes,
#      not that the system reports one.  Both log LANGUAGE_CHANGED
#      by=system within 3 s.
#   4. The pane's session process is the same pid from start to end.
#   5. The app is still up, with no panic and no error logged.
#
# What it cannot assert: the items AppKit provides are in the language
# the process was launched in.  Each menu's items are printed as read
# back so a reader can see which those are.
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app
# is never touched.  Exits 0 on pass, non-zero on the first failure.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

RUNLOG=/tmp/marspot-test-language-switch.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SETTINGS="$MARSPOT_STATE_DIR/settings.toml"
SYSTEM_LANGUAGE="$MARSPOT_STATE_DIR/dev-system-language"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"

# The menu bar's titles in each language, as MENU_BAR lists them.
menus_of() {
  case "$1" in
    en) echo 'Marspot,File,Edit,View,Window,Help' ;;
    ja) echo 'Marspot,ファイル,編集,表示,ウインドウ,ヘルプ' ;;
    zh-Hans) echo 'Marspot,文件,编辑,显示,窗口,帮助' ;;
  esac
}

fail() {
  echo "FAIL: $*"
  echo "  --- language and settings events in $APPLOG:"
  grep -E $'\t(LANGUAGE_CHANGED|MENU_BAR|SETTINGS_RELOADED)\t' "$APPLOG" 2>/dev/null | cut -f1,4,7- | sed 's/^/    /'
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
rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
mkdir -p "$MARSPOT_STATE_DIR/logs"
rm -f "$APPLOG" "$SETTINGS" "$MARSPOT_STATE_DIR/system-language"
> "$RUNLOG"
echo en-US > "$SYSTEM_LANGUAGE"

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

echo "==> launching"
SHELL=/bin/sh MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  MARSPOT_DEV_SYSTEM_LANGUAGE="$SYSTEM_LANGUAGE" \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
disown

# Wait until the shell and the core have each logged LANGUAGE_CHANGED
# for language $1 chosen by $2, and the shell a MENU_BAR line whose
# titles are $6, after line $3 of the log; print how long each took, in
# milliseconds since $4 (epoch ms), then each menu's items.
wait_language() { # lang, by, after_line, since_ms, seconds, menus
  python3 - "$APPLOG" "$1" "$2" "$3" "$4" "$5" "$6" <<'PY'
import sys, time
path, lang, by, after, since, secs, menus = (
    sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5]), float(sys.argv[6]), sys.argv[7])
deadline = time.time() + secs + 1.0
while True:
    seen, bar, menus_seen = {}, None, []
    try:
        lines = open(path, errors='replace').read().split('\n')[after:]
    except FileNotFoundError:
        lines = []
    for line in lines:
        cols = line.split('\t')
        if len(cols) < 8:
            continue
        f = dict(c.split('=', 1) for c in cols[8:] if '=' in c)
        if cols[6] == 'LANGUAGE_CHANGED' and f.get('lang') == lang and f.get('by') == by:
            seen.setdefault(cols[3], int(cols[1]))
        if cols[6] == 'MENU_BAR' and f.get('lang') == lang:
            bar = f
        if cols[6] == 'MENU_BAR_ITEMS' and f.get('lang') == lang:
            menus_seen.append(f"{f.get('menu')}: {f.get('items')}")
    if all(c in seen for c in ('shell', 'core')) and bar is not None:
        took = {c: seen[c] - since for c in ('shell', 'core')}
        print(', '.join(f"{c} {ms} ms" for c, ms in took.items()))
        time.sleep(0.2)
        for m in menus_seen:
            print(f"    {m}")
        if bar.get('top') != menus:
            print(f"    its titles are {bar.get('top')!r}, not {menus!r}")
            sys.exit(1)
        sys.exit(0 if max(took.values()) <= secs * 1000 else 1)
    if time.time() > deadline:
        print(f"only {sorted(seen)} logged it; menu bar line: {bar is not None}")
        sys.exit(1)
    time.sleep(0.05)
PY
}

log_lines() { wc -l < "$APPLOG" | tr -d ' '; }
now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }

# Put `appearance.language = $1` in force the way an editor saving the
# file does: a whole new file under the same name.
set_language() {
  printf 'appearance.language = %s\n' "$1" > "$SETTINGS.new" && mv "$SETTINGS.new" "$SETTINGS"
}

# --- 1. English at start --------------------------------------------------
for (( i = 0; i < 600; i++ )); do
  [[ -n "$(dev_session_pids)" && -n "$(dev_core_pids)" ]] && break
  sleep 0.1
done
sessions_at_start="$(dev_session_pids | sort | tr '\n' ' ')"
[[ -n "$sessions_at_start" ]] || fail "no session process is registered"
sleep 2.5
grep -q $'\tLANGUAGE_CHANGED\t' "$APPLOG" && fail "a language change was logged before anything changed"
echo "==> English at start; session pid(s): $sessions_at_start"

# --- 2. three switches by the settings file -------------------------------
for lang in ja zh-Hans en; do
  after=$(log_lines)
  since=$(now_ms)
  set_language "$lang"
  took="$(wait_language "$lang" setting "$after" "$since" 2 "$(menus_of "$lang")")" \
    || fail "$lang: not followed in time by the shell and the core ($took)"
  echo "==> $lang by the settings file: $took"
done

# --- 3. following the system ----------------------------------------------
# The stand-in says English and the language in force is English, so
# switching the setting to `system` changes nothing.
after=$(log_lines)
set_language system
sleep 2.5
tail -n +"$((after + 1))" "$APPLOG" | grep -q $'\tLANGUAGE_CHANGED\t' \
  && fail "switching to system changed the language while the system's was the one already in force"
for tag in ja-JP zh-Hans-CN; do
  lang="${tag%-*}"
  after=$(log_lines)
  since=$(now_ms)
  echo "$tag" > "$SYSTEM_LANGUAGE"
  took="$(wait_language "$lang" system "$after" "$since" 3 "$(menus_of "$lang")")" \
    || fail "system -> $tag: not followed in time by the shell and the core ($took)"
  echo "==> $lang by the system's language list (stand-in file, $tag): $took"
done
[[ "$(head -1 "$MARSPOT_STATE_DIR/system-language" 2>/dev/null)" == zh-Hans ]] \
  || fail "the shell did not publish what the system resolves to"
echo "==> the system's own items were launched in: $(sed -n 2p "$MARSPOT_STATE_DIR/system-language")"

# --- 4. nothing restarted --------------------------------------------------
sessions_at_end="$(dev_session_pids | sort | tr '\n' ' ')"
[[ "$sessions_at_end" == "$sessions_at_start" ]] \
  || fail "the session process changed: $sessions_at_start -> $sessions_at_end"

# --- 5. still up, and quiet ------------------------------------------------
[[ -n "$(dev_shell_pid)" ]] || fail "the shell is gone"
[[ -n "$(dev_core_pids)" ]] || fail "the core is gone"
grep -qi 'panicked' "$APPLOG" "$RUNLOG" && fail "something panicked"
errors="$(grep -c $'\tERROR\t' "$APPLOG")"
(( errors == 0 )) || {
  grep $'\tERROR\t' "$APPLOG" | cut -f7- | sed 's/^/    /'
  fail "$errors error line(s) in the log"
}

echo
echo "PASS — five language changes reached the shell and the core with"
echo "       nothing restarted: three by the settings file, two by the"
echo "       system's language list (stand-in file; the machine's real"
echo "       setting was not changed).  The menu bar's own titles"
echo "       followed each one."

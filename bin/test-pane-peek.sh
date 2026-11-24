#!/usr/bin/env bash
#
# A shadow pane viewed in its parent's place, in a sandboxed app with one
# window of two panes.  The requests come from `marspot-peer pane place`;
# the keys, the wheel and the panel's button are played into the window
# by the core itself (`MARSPOT_DEV_PANELS`, sandbox only), each when this
# script says so (`until:<file>`).
#
#   1. Viewing gives the shadow the parent's size and types into the
#      shadow; the parent is not resized and gets no SIGWINCH.
#   2. Cmd-W leaves the view and closes nothing; the parent is at the
#      scroll position it was left at, with its half-typed line.
#   3. A view whose shell ends keeps its last frame: a key does nothing,
#      Enter leaves, and neither reaches the parent.
#   4. `place unpeek` leaves; a viewed pane is not resized by request; a
#      pane below no window is refused.
#   5. The helper panel's "View here" button.
#   6. After a restart of the core the parent shows itself.
#   7. In a window with one pane (no title strip) the header names the
#      helper while it is viewed and stops when the view is left, and
#      neither session is resized by switching.
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app is
# never touched.  Opens windows, so it runs on the runner:
#   bin/test-e2e-remote.sh test-pane-peek.sh
# Exits 0 when every check passed.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
source "$ROOT/bin/_sandbox-panes.sh"

SD="$MARSPOT_STATE_DIR"
BIN="$ROOT/target/release"
CLI="$BIN/marspot-shell"
PEER="$BIN/marspot-peer"
LOG="$SD/logs/marspot.log"

die() {
  echo "FAIL: $*"
  echo "  --- peek and script events in $LOG:"
  grep -E $'\t(PEEK_[A-Z]+|DEV_PANELS[A-Z_]*|PANE_CONTROL_REPLY|core\\.peek[a-z_.]*)\t' "$LOG" 2>/dev/null | cut -f7- | tail -40 | sed 's/^/    /'
  exit 1
}
trap dev_kill_shell_core EXIT

for b in marspot-shell marspot-core marspot-session marspot-peer; do
  [[ -x $BIN/$b ]] || die "$BIN/$b is not built (cargo build --release --bin marspot-shell --bin marspot-core --bin marspot-session && cargo build --release -p marspot-peer)"
done

pc() { in_sd "$SD" "$PEER" pane "$@"; }
field() { # json key on stdin
  python3 -c 'import json,sys; v=json.load(sys.stdin); r=v.get("result",v); print(r.get(sys.argv[1], ""))' "$1"
}
# the last event's value of a field
last_of() { # event tag, field
  grep -a "$(printf '\t%s\t' "$1")" "$LOG" | tail -1 | tr '\t' '\n' | sed -n "s/^$2=//p"
}
go() { : > "$SD/go-$1"; }
appears() { # file, seconds
  local i
  for (( i = 0; i < $2 * 10; i++ )); do
    [[ -e $1 ]] && return 0
    sleep 0.1
  done
  return 1
}
absent() { sleep 1; [[ ! -e $1 ]]; }

dev_kill_shell_core
dev_wipe_state
rm -rf "$SD/sessions" "$SD/retired"
mkdir -p "$SD/logs"
# every shell says where `mark` was run in it, and each size it is told
cat > "$SD/rc.zsh" <<ZSH
PS1="%# "
unsetopt BEEP
mark() { stty size > "$SD/marked-\$MARSPOT_PEEK_WHO"; }
TRAPWINCH() { stty size >> "$SD/winch-\$MARSPOT_PEEK_WHO"; }
ZSH

python3 - "$SD" <<'PY' || die "could not write the layout"
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1])
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
def pane(): return struct.pack('<Q', 0) + s('') + s('')
body = struct.pack('<II', 0xA5505010, 2) + struct.pack('<HH', 0, 1)
body += struct.pack('<HHHH', 2, 1, 0, 2) + pane() + pane()
(d / 'shell-state.bin').write_bytes(body)
frames = struct.pack('<IIH', 0xA5505011, 2, 1) + struct.pack('<Idddd', 0, 80.0, 200.0, 1500.0, 700.0)
(d / 'window-state.bin').write_bytes(frames)
PY

MARK="type:m;type:a;type:r;type:k"
STEPS=(
  until:go-half "$MARK" scroll:5
  until:go-helper "$MARK" key:enter
  until:go-leave key:cmd-w
  until:go-finish key:enter
  until:go-ended "type:x" key:enter wait:500 "$MARK" key:enter
  until:go-panel click:helpers-mark wait:800 click:helpers-view
)
SCRIPT="$(IFS=';'; echo "${STEPS[*]}")"
env SHELL=/bin/zsh MARSPOT_SESSION_BIN="$BIN/marspot-session" MARSPOT_DEV_PANELS="$SCRIPT" \
  nohup "$CLI" > "$(dev_run_log pane-peek)" 2>&1 < /dev/null &
dev_record_pid $!
wait_count "$SD" HELLO_ACK 0 90 || die "the core never said hello"
wait_count "$SD" PANE_CONTROL_CAPS_SEEN 0 30 || die "the shell never heard what the core does with pane control"
CAPS=$(last_of PANE_CONTROL_CAPS_SEEN caps)
(( CAPS & 4 )) || die "the core does not announce viewing in place (caps=$CAPS)"

LAYOUT=""
for (( i = 0; i < 100; i++ )); do
  LAYOUT=$(pc list --json 2>/dev/null | python3 -c '
import json,sys
ps = json.load(sys.stdin)["result"]["panes"]
print(" ".join(str(p["id"]) for p in sorted((p for p in ps if p["cell"]), key=lambda p: p["cell"])))' 2>/dev/null)
  (( $(wc -w <<<"$LAYOUT") >= 2 )) && break
  sleep 0.5
done
read -r HOST OTHER <<<"$LAYOUT"
[[ -n ${OTHER:-} ]] || die "two panes with cells expected, got '$LAYOUT'"

at_shell() { # pane
  local i
  for (( i = 0; i < 30; i++ )); do
    pc run "$1" --timeout 10 true >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}
named() { # pane, name
  at_shell "$1" || die "pane $1's shell never ran a command"
  # no start-up file but this one, whatever the host's own are
  pc send "$1" 'exec /bin/zsh -f' >/dev/null || die "pane $1 was not typed into"
  wait_shows "$SD" "$1" "% " 10
  at_shell "$1" || die "pane $1's plain shell never ran a command"
  pc run "$1" --timeout 10 ". $SD/rc.zsh; MARSPOT_PEEK_WHO=$2" >/dev/null || die "pane $1 did not take its name"
}
open_helper() { # name: prints the new shadow pane's id
  local id
  id=$(pc open --parent "$HOST" --json 2>&1 | field id)
  [[ -n $id ]] || die "no shadow pane opened"
  named "$id" "$1"
  echo "$id"
}
events() { count "$SD" "$1"; }

named "$HOST" host
pc run "$HOST" --timeout 10 'for i in {1..200}; do print line-$i; done' >/dev/null || die "the parent printed nothing"
H1=$(open_helper h1)
[[ -n $H1 ]] || die "no shadow pane"
HOST_SIZE=$(pc run "$HOST" --timeout 10 "stty size > $SD/host-size" >/dev/null; cat "$SD/host-size")
echo "==> parent $HOST ($HOST_SIZE), other $OTHER, shadow $H1"
go half
for (( i = 0; i < 50; i++ )); do shows "$SD" "$HOST" "% mark" && break; sleep 0.2; done
sleep 1.5
WINCH0=$(cat "$SD/winch-host" 2>/dev/null | wc -l)

echo "════ 1. viewing ════"
check "place peek" pc place "$H1" peek
check "the parent's slot shows it" test "$(last_of PEEK_SHOWN session)" = "$H1"
OFFSET=$(last_of PEEK_SHOWN host_offset)
check "the parent was scrolled back when it was hidden (offset ${OFFSET:-none})" test "${OFFSET:-0}" -gt 0
go helper
check "what is typed runs in the shadow" appears "$SD/marked-h1" 20
check "and not in the parent" absent "$SD/marked-host"
check "the shadow has the parent's size ($HOST_SIZE)" test "$(cat "$SD/marked-h1" 2>/dev/null)" = "$HOST_SIZE"
check "and was told once it has it, so a SIGWINCH is seen" test "$(tail -1 "$SD/winch-h1" 2>/dev/null)" = "$HOST_SIZE"
check "a viewed pane is not resized by request" bash -c "$(printf '%q' "$PEER") pane resize $H1 50x20 2>&1 | grep -q not_resizable"

echo "════ 2. Cmd-W leaves ════"
left=$(events PEEK_LEFT)
go leave
check "Cmd-W left the view" wait_count "$SD" PEEK_LEFT "$left" 10
check "the parent is at the scroll position it was left at" test "$(last_of PEEK_LEFT host_offset)" = "$OFFSET"
sleep 1
check "no pane was closed" bash -c "$(printf '%q' "$PEER") pane list --json | python3 -c 'import json,sys; sys.exit(0 if len(json.load(sys.stdin)[\"result\"][\"panes\"]) == 3 else 1)'"
go finish
check "the line half typed before the view runs on Enter" appears "$SD/marked-host" 20
check "in the parent's own size" test "$(cat "$SD/marked-host" 2>/dev/null)" = "$HOST_SIZE"

echo "════ 3. a view that ends ════"
rm -f "$SD/marked-host"
check "place peek again" pc place "$H1" peek
left=$(events PEEK_LEFT)
pc send "$H1" 'exit' >/dev/null 2>&1
check "the view ended" wait_count "$SD" PEEK_ENDED 0 20
check "its last frame stays" test "$(events PEEK_LEFT)" = "$left"
go ended
check "Enter left it" wait_count "$SD" PEEK_LEFT "$left" 10
# a leaked x would have run `xmark`; a leaked Enter would have run nothing and passed unseen, so the line is looked at too
check "the next keys are the parent's, with nothing before them" appears "$SD/marked-host" 20
check "the key typed at the last frame reached nobody" lacks "$SD" "$HOST" "xmark"

echo "════ 4. place unpeek, and what is refused ════"
H2=$(open_helper h2)
check "place peek" pc place "$H2" peek
shown=$(last_of PEEK_SHOWN session)
check "the second shadow pane is shown" test "$shown" = "$H2"
left=$(events PEEK_LEFT)
check "place unpeek" pc place "$H2" unpeek
check "the parent is shown again" wait_count "$SD" PEEK_LEFT "$left" 5
check "a window's pane is not viewed in place" bash -c "$(printf '%q' "$PEER") pane place $OTHER peek 2>&1 | grep -q 'only a shadow pane'"
check "peek by the parent's name leaves any view of it" pc place "$HOST" unpeek

echo "════ 5. the panel's button ════"
shown=$(events PEEK_SHOWN)
go panel
check "View here shows the shadow pane" wait_count "$SD" PEEK_SHOWN "$shown" 20
check "which is the one in the row" test "$(last_of PEEK_SHOWN session)" = "$H2"
check "every step of the script had somewhere to go" bash -c "! grep -qa $'\tDEV_PANELS_\\(STEP_FAILED\\|REJECTED\\)\t' $(printf '%q' "$LOG")"

check "the parent got no SIGWINCH in all of it" test "$(cat "$SD/winch-host" 2>/dev/null | wc -l)" = "$WINCH0"

echo "════ 6. a core restart ════"
rm -f "$SD"/go-*
adopted=$(events SHADOW_ADOPTED)
shown=$(events PEEK_SHOWN)
swap_core "$SD" "$BIN/marspot-core"
check "the shadow pane is back in the shadows" wait_count "$SD" SHADOW_ADOPTED "$adopted" 20
check "and viewed nowhere" test "$(events PEEK_SHOWN)" = "$shown"
check "the parent is the size it was" bash -c "$(printf '%q' "$PEER") pane run $HOST --timeout 20 'stty size > $SD/host-size' >/dev/null && [[ \$(cat $SD/host-size) == '$HOST_SIZE' ]]"
check "it is viewed again when asked" pc place "$H2" peek

echo "════ 7. a window with one pane ════"
check "no header line while the window had two panes" test "$(events PEEK_LINE)" = 0
left=$(events PEEK_LEFT)
check "place unpeek" pc place "$H2" unpeek
check "the parent is shown again" wait_count "$SD" PEEK_LEFT "$left" 5
check "the other pane is closed" pc close "$OTHER"
LONE=""
for (( i = 0; i < 40; i++ )); do
  pc run "$HOST" --timeout 10 "stty size > $SD/host-size" >/dev/null 2>&1
  LONE=$(cat "$SD/host-size" 2>/dev/null)
  [[ -n $LONE && $LONE != "$HOST_SIZE" ]] && break
  sleep 0.5
done
check "the parent took the title strip's row ($HOST_SIZE -> $LONE)" test "$LONE" != "$HOST_SIZE"
sleep 1
# the counts below prove nothing unless the shells' SIGWINCH records exist
check "the parent was told its new size, so its SIGWINCHes are counted" test "$(tail -1 "$SD/winch-host" 2>/dev/null)" = "$LONE"
WINCH1=$(cat "$SD/winch-host" 2>/dev/null | wc -l)
lines=$(events PEEK_LINE)
check "place peek" pc place "$H2" peek
check "the header says what is viewed" wait_count "$SD" PEEK_LINE "$lines" 10
check "in words that fit the header" test "$(last_of PEEK_LINE drawn)" = 1 -a "$(last_of PEEK_LINE chars)" -gt 0
check "the helper has the parent's size" bash -c "$(printf '%q' "$PEER") pane run $H2 --timeout 20 'stty size > $SD/h2-size' >/dev/null && [[ \$(cat $SD/h2-size) == '$LONE' ]]"
sleep 1
check "and was told it, so its SIGWINCHes are counted" test "$(tail -1 "$SD/winch-h2" 2>/dev/null)" = "$LONE"
WINCH2=$(cat "$SD/winch-h2" 2>/dev/null | wc -l)
lines=$(events PEEK_LINE)
left=$(events PEEK_LEFT)
check "place unpeek" pc place "$H2" unpeek
check "the parent is shown again" wait_count "$SD" PEEK_LEFT "$left" 5
check "the header line goes" wait_count "$SD" PEEK_LINE "$lines" 10
check "and says nothing" test "$(last_of PEEK_LINE chars)" = 0
lines=$(events PEEK_LINE)
check "place peek again" pc place "$H2" peek
check "the line is back" wait_count "$SD" PEEK_LINE "$lines" 10
left=$(events PEEK_LEFT)
check "place unpeek again" pc place "$H2" unpeek
check "and left" wait_count "$SD" PEEK_LEFT "$left" 5
sleep 1
check "switching resized neither the parent nor the helper" test "$(cat "$SD/winch-host" 2>/dev/null | wc -l)" = "$WINCH1" -a "$(cat "$SD/winch-h2" 2>/dev/null | wc -l)" = "$WINCH2"
check "the parent is the size it was" bash -c "$(printf '%q' "$PEER") pane run $HOST --timeout 20 'stty size > $SD/host-size' >/dev/null && [[ \$(cat $SD/host-size) == '$LONE' ]]"

check "the app is still up" alive "$(dev_shell_pid)"
check "no panic in the log" bash -c "! grep -qa $'\tPANIC' $(printf '%q' "$LOG")"

echo
echo "$((panes_checks - panes_failures - panes_not_applicable))/$panes_checks passed, $panes_not_applicable not applicable"
(( panes_failures == 0 ))

#!/usr/bin/env bash
#
# The list of helpers at the bottom of a pane, end to end, in a sandboxed
# app: what it holds, and above all how often the program in the pane is
# resized for it.
#
# The parent pane runs a stand-in Claude session that logs its terminal's
# size at each SIGWINCH (_peerbus_fake_claude.py), on the alternate screen
# in all but the last part.  The helpers' agents are stand-ins too
# (_fake-helper-agent.py); each works until this script makes a file.
#
#   1. with the setting off a helper changes nothing; turned on, off and
#      on again it takes rows, gives them back and takes them, one resize
#      each;
#   2. a second helper takes one more row, in one resize, and both are
#      listed with their names, what they do, how long they have run and
#      the tokens of one that says; the figures go on by the second;
#   3. one helper ending resizes nothing;
#   4. a swap of the core and one of the shell resize nothing, and the
#      list is there again;
#   5. the last one stopped: in view for a while longer, and not drawn
#      again; then the rows go back in one resize;
#   6. with one that only waits for an answer nothing wakes the core;
#   7. nothing is resized under the user's hands;
#   8. claude on the main screen: every row at once, then none.
#
# Sandbox-only: MARSPOT_STATE_DIR and HOME are redirected and the
# installed app is never touched.  Opens windows, so it runs on the
# runner:  bin/test-e2e-remote.sh test-helper-strip.sh
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
SB=/tmp/marspot-strip-$$
CLAUDE_HOME="$SB/home/.claude"
REGISTRY="$CLAUDE_HOME/sessions"
SIZES="$SB/sizes.log"
export PEERBUS_REGISTRY="$REGISTRY" FAKE_HELPER_LOG="$SB/helpers.log" MARSPOT_DEV_STRIP_DUMP=1

die() {
  echo "FAIL: $*"
  echo "  --- sizes:"; sed 's/^/    /' "$SIZES" 2>/dev/null
  echo "  --- strip lines in $LOG:"
  grep -aE 'HELPER_STRIP|DEV_STRIP|PANIC' "$LOG" 2>/dev/null | cut -f1,5- | tail -30 | cut -c1-300 | sed 's/^/    /'
  exit 1
}
cleanup() {
  local rc=$?
  dev_kill_shell_core
  dev_sweep_dead_inboxes
  (( rc != 0 )) || rm -rf "$SB"
}
trap cleanup EXIT

for b in marspot-shell marspot-core marspot-session marspot-peer; do
  [[ -x $BIN/$b ]] || die "$BIN/$b is not built"
done

rm -rf "$SB"
mkdir -p "$SB/bin" "$SB/proj" "$REGISTRY"
mkdir -p "$CLAUDE_HOME/projects"
for p in .claude-profile-1 .codex-profile-1; do mkdir -p "$SB/home/$p"; done
ln -s "$CLAUDE_HOME/projects" "$SB/home/.claude-profile-1/projects"
for a in claude codex; do ln -s "$ROOT/bin/_fake-helper-agent.py" "$SB/bin/$a"; done
chmod +x "$ROOT/bin/_fake-helper-agent.py"
export HOME="$SB/home" PATH="$SB/bin:$PATH"
printf 'export PATH="%s/bin:$PATH"\n' "$SB" > "$HOME/.zshrc"

pc() { in_sd "$SD" "$PEER" pane "$@"; }
hc() { in_sd "$SD" "$PEER" helper "$@"; }
field() { python3 -c 'import json,sys; v=json.load(sys.stdin); r=v.get("result",v); print(r.get(sys.argv[1], ""))' "$1"; }
start_helper() { # agent, task → the answer's JSON
  hc start --json --agent "$1" --parent "$P1" --cwd "$SB/proj" --budget 30 "$2"
}
started() { # agent, task → its name, once it runs
  local a n
  a=$(start_helper "$1" "$2")
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
  rm -rf "$SD/sessions" "$SD/retired" "$SIZES"
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

SETTING=false
start_app
start_parent FAKE_ALT=1

echo "════ 1. the setting ════"
n=$(sizes)
H1=$(started codex "Review the parser. FAKE-WORK=$SB/end1 FAKE-EXIT") || die "no first helper"
sleep 8
check "off: a running helper changes nothing" test "$(sizes)" = "$n"
check "off: no strip was drawn" bash -c "! grep -qa $'\tDEV_STRIP\t' '$LOG'"
setting true
check "turned on: its rows are taken, in one resize" one_resize_to "$n" $(( FULL - 3 )) 10
n=$(sizes)
setting false
check "turned off: they are given back, in one resize" one_resize_to "$n" "$FULL" 10
n=$(sizes)
setting true
check "and on again" one_resize_to "$n" $(( FULL - 3 )) 10

echo "════ 2. a second helper, and what is listed ════"
n=$(sizes)
H2=$(started codex "Read the log. FAKE-WORK=$SB/end2 FAKE-EXIT") || die "no second helper"
check "one more row, in one resize" one_resize_to "$n" $(( FULL - 4 )) 30
check "both are listed, under a main" wait_strip 10 '⏺ main' '◯ codex' 'Review the parser' 'Read the log'
check "two of one name are told apart" bash -c "grep -Eq 'codex [0-9]{4} .*codex [0-9]{4} ' <<<\"\$1\"" _ "$(strip_now)"
figure() { strip_now | grep -oE '[0-9]+(m [0-9]+)?s' | head -1; }
a=$(figure); sleep 3; b=$(figure)
check "how long one has run goes on ($a, $b)" test "$a" != "$b"
n=$(sizes)
H3=$(started claude "Write the notes. FAKE-TOKENS FAKE-WORK=$SB/end3 FAKE-EXIT") || die "no third helper"
check "a third: one more row" one_resize_to "$n" $(( FULL - 5 )) 30
check "the tokens of one that says" wait_strip 30 '◯ claude' '↓ 115.3k tokens'

echo "════ 3. one ends ════"
n=$(sizes)
touch "$SB/end1"
sleep 25
check "nothing is resized" test "$(sizes)" = "$n"
check "the others are still listed" strip_has 'Read the log' 'Write the notes'

echo "════ 4. swaps ════"
n=$(sizes) shown=$(count "$SD" DEV_STRIP)
swap_core "$SD" "$BIN/marspot-core"
wait_count "$SD" DEV_STRIP "$shown" 30
sleep 8
check "a core swap resizes nothing" test "$(sizes)" = "$n"
check "the new core found the rows" logged "$SD" "HELPER_STRIP_FOUND"
check "and lists them again" strip_has '⏺ main' 'Read the log' 'Write the notes'
shown=$(count "$SD" DEV_STRIP) updates=$(count "$SD" SHELL_SELF_UPDATE) acks=$(count "$SD" HELLO_ACK)
sleep 2
mkdir -p "$SD/binaries/pending"
install -m 755 "$CLI" "$SD/binaries/pending/marspot-shell"
kill -USR1 "$(dev_shell_pid)"
wait_count "$SD" SHELL_SELF_UPDATE "$updates" 60 || die "the shell never swapped"
wait_count "$SD" HELLO_ACK "$acks" 90 || die "no core after the shell swap"
wait_count "$SD" DEV_STRIP "$shown" 60
sleep 10
check "a shell swap resizes nothing" test "$(sizes)" = "$n"
check "and they are listed again" strip_has '⏺ main' 'Read the log' 'Write the notes'

echo "════ 5. all have ended ════"
touch "$SB/end3"
sleep 6
hc stop "$H3" >/dev/null 2>&1 || echo "    could not stop $H3"
sleep 2
n=$(sizes)
check "still there with one at work" test "$(rows_now)" = $(( FULL - 5 ))
hc stop "$H2" >/dev/null 2>&1 || echo "    could not stop $H2"
wait_strip 20 'stopped · ' || true
check "the stopped one is in view" strip_has 'stopped · '
sleep 4
shown=$(count "$SD" DEV_STRIP)
sleep 8
check "nothing more is drawn" test "$(count "$SD" DEV_STRIP)" = "$shown"
check "the rows are still taken" test "$(sizes)" = "$n"
check "then they are given back, in one resize" one_resize_to "$n" "$FULL" 60

echo "════ 6. one that waits for an answer ════"
n=$(sizes)
H5=$(started codex "Look elsewhere. FAKE-BLOCK") || die "no helper"
check "it is listed as waiting" wait_strip 120 'awaiting approval'
check "with its row taken, in one resize" one_resize_to "$n" $(( FULL - 3 )) 10
# the graphics driver's timers run for a while after a frame, and the
# supervisor's clock after output
sleep 100
core=$(in_sd "$SD" dev_core_pids | head -1)
"$ROOT/bin/_idle_wakeups.py" --label "core, a strip in view, nothing at work" --window 30 --readings 1 --budget 0.6 \
  --pattern "$(ps -o command= -p "$core" | awk '{print $1}')( |\$)"
rc=$?
if (( rc == 3 )); then
  not_applicable "nothing wakes the core for it" "the host was busy"
else
  check "nothing wakes the core for it" test "$rc" = 0
fi
check "the strip is still in view" strip_has 'awaiting approval'
hc stop "$H5" >/dev/null 2>&1

echo "════ 7. under the user's hands ════"
SETTING=true
typing=$(python3 -c 'print(";".join(["wait:22000"] + ["type:x;wait:400"] * 50))')
start_app MARSPOT_DEV_PANELS="$typing"
start_parent FAKE_ALT=1
for (( i = 0; i < 60; i++ )); do shows "$SD" "$P1" xxx && break; sleep 0.5; done
check "the script types" shows "$SD" "$P1" xxx
n=$(sizes)
rm -f "$SB/end6"
H6=$(started codex "Look around. FAKE-WORK=$SB/end6 FAKE-EXIT") || die "no helper"
sleep 6
typed_on=$(screen "$SD" "$P1" | tr -cd x | wc -c)
check "nothing is resized while the user types" test "$(sizes)" = "$n"
sleep 1
check "and typing was still going on" test "$(screen "$SD" "$P1" | tr -cd x | wc -c)" -gt "$typed_on"
check "once it stops, in one resize" one_resize_to "$n" $(( FULL - 3 )) 40

echo "════ 8. claude on the main screen ════"
start_app
start_parent
n=$(sizes)
rm -f "$SB"/end7*
started codex "One. FAKE-WORK=$SB/end7 FAKE-EXIT" >/dev/null || die "no helper"
check "every row at once" one_resize_to "$n" $(( FULL - 8 )) 30
n=$(sizes)
started codex "Two. FAKE-WORK=$SB/end7 FAKE-EXIT" >/dev/null || die "no helper"
check "a second is listed" wait_strip 30 'One.' 'Two.'
sleep 3
check "and resizes nothing" test "$(sizes)" = "$n"

check "the app is still up" alive "$(dev_shell_pid)"
check "no panic in the log" bash -c "! grep -qa $'\tPANIC' $(printf '%q' "$LOG")"

echo
echo "$((panes_checks - panes_failures - panes_not_applicable))/$panes_checks passed, $panes_not_applicable not applicable"
(( panes_failures == 0 ))

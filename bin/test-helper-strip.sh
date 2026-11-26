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
#   5. the last two stopped: in view for a while longer, and not drawn
#      again; then each gives its row back as it leaves the list, the
#      last with the rest;
#   6. with one that only waits for an answer nothing wakes the core;
#   7. nothing is resized under the user's hands;
#   8. claude on the main screen: its rows and a blank one under them,
#      one more row for a second helper, and none given back;
#   9. a click on a helper's row views it and one on main comes back;
#      down in the stand-in's empty input box takes the keys, enter
#      views, x stops a helper, and keys the strip takes never reach
#      the program while others do;
#  10. two claude helpers given a name and a title: each row names the
#      agent and the model its transcript says answered, says the title,
#      then `idle` once its turn ended or `awaiting approval`; the strip
#      is as high as the rows it draws, alone and under Claude's own
#      list, with no blank row under them; idle and awaiting approval
#      come within 5 s of the turn's end and of the prompt.
#
# Sandbox-only: MARSPOT_STATE_DIR and HOME are redirected and the
# installed app is never touched.  Opens windows, so it runs on the
# runner:  bin/test-e2e-remote.sh test-helper-strip.sh
# With STRIP_SHOTS=<dir> part 10 leaves there what the strip and the pane
# held and pictures of the sandbox's window.
# Exits 0 when every check passed.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
source "$ROOT/bin/_sandbox-panes.sh"
source "$ROOT/bin/_helper-strip-lib.sh"

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
  (( rc != 0 )) || rm -rf "${SB:?}"
}
trap cleanup EXIT

for b in marspot-shell marspot-core marspot-session marspot-peer; do
  [[ -x $BIN/$b ]] || die "$BIN/$b is not built"
done

rm -rf "${SB:?}"
mkdir -p "$SB/bin" "$SB/proj" "$REGISTRY"
mkdir -p "$CLAUDE_HOME/projects"
for p in .claude-profile-1 .codex-profile-1; do mkdir -p "$SB/home/$p"; done
ln -s "$CLAUDE_HOME/projects" "$SB/home/.claude-profile-1/projects"
for a in claude codex; do ln -s "$ROOT/bin/_fake-helper-agent.py" "$SB/bin/$a"; done
chmod +x "$ROOT/bin/_fake-helper-agent.py"
# the stand-ins write where claude would: never into a real profile
export HOME="$SB/home" CLAUDE_CONFIG_DIR="$CLAUDE_HOME" PATH="$SB/bin:$PATH"
printf 'export PATH="%s/bin:$PATH"\n' "$SB" > "$HOME/.zshrc"


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
check "the third stops" quiet hc stop "$H3"
sleep 2
n=$(sizes)
check "still there with one at work" test "$(rows_now)" = $(( FULL - 5 ))
check "and the second" quiet hc stop "$H2"
wait_strip 20 'stopped · ' || true
check "the stopped one is in view" strip_has 'stopped · '
sleep 4
shown=$(count "$SD" DEV_STRIP)
sleep 8
check "nothing more is drawn" test "$(count "$SD" DEV_STRIP)" = "$shown"
check "the rows are still taken" test "$(sizes)" = "$n"
# on the alternate screen a stopped row leaving the list gives its row back
check "then each stopped one's row goes as it leaves the list" grows_back_to "$n" "$FULL" 2 60

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
check "it stops" quiet hc stop "$H5"

echo "════ 7. under the user's hands ════"
SETTING=true
typing=$(python3 -c 'print(";".join(["wait:22000"] + ["type:x;wait:400"] * 50))')
start_app MARSPOT_DEV_PANELS="$typing"
start_parent FAKE_ALT=1
for (( i = 0; i < 60; i++ )); do shows "$SD" "$P1" xxx && break; sleep 0.5; done
check "the script types" shows "$SD" "$P1" xxx
n=$(sizes)
rm -f "${SB:?}/end6"
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
rm -f "${SB:?}"/end7*
started codex "One. FAKE-WORK=$SB/end7 FAKE-EXIT" >/dev/null || die "no helper"
check "a blank row, main, its row and a blank row under it" one_resize_to "$n" $(( FULL - 4 )) 30
n=$(sizes)
started codex "Two. FAKE-WORK=$SB/end7 FAKE-EXIT" >/dev/null || die "no helper"
check "a second: one more row" one_resize_to "$n" $(( FULL - 5 )) 30
check "both are listed" wait_strip 30 'One.' 'Two.'
n=$(sizes)
touch "$SB/end7"
sleep 3
check "the rows are kept once they are over" test "$(sizes)" = "$n"

echo "════ 9. viewing from the strip, and its keyboard ════"
SETTING=true
KEYS="$SB/keys.log"
# each step waits for the file this script makes when it is time
steps="until:go-click;click:strip-2;wait:1500;click:strip-1;wait:1500"
steps+=";until:go-keys;key:down;wait:800;key:down;wait:400;key:enter;wait:1500;click:strip-1;wait:1500"
steps+=";until:go-x;key:down;wait:800;key:x;wait:2000;key:esc;wait:600;key:down;wait:800;type:a;wait:800"
start_app MARSPOT_DEV_PANELS="$steps"
start_parent FAKE_ALT=1 FAKE_KEYS_LOG="$KEYS"
rm -f "${SB:?}/end8" "${SB:?}/end9"
H8=$(started codex "Eight. FAKE-WORK=$SB/end8 FAKE-EXIT") || die "no helper"
H9=$(started codex "Nine. FAKE-WORK=$SB/end9 FAKE-EXIT") || die "no helper"
check "both are listed" wait_strip 30 'Eight.' 'Nine.'
sleep 2
viewed=$(count "$SD" PEEK_SHOWN) left=$(count "$SD" PEEK_LEFT)
touch "$SD/go-click"
check "a click on a helper views it in the pane's place" wait_count "$SD" PEEK_SHOWN "$viewed" 15
check "its row has the filled circle" wait_strip 10 '◯ main' '⏺ codex'
check "a click on main shows the pane again" wait_count "$SD" PEEK_LEFT "$left" 15
check "and main has the circle back" wait_strip 10 '⏺ main'
: > "$KEYS"
viewed=$(count "$SD" PEEK_SHOWN) left=$(count "$SD" PEEK_LEFT) taken=$(count "$SD" HELPER_STRIP_KEYS)
touch "$SD/go-keys"
check "down in the empty box the agent did nothing with takes the keys" wait_count "$SD" HELPER_STRIP_KEYS "$taken" 15
check "down then enter views the second helper" wait_count "$SD" PEEK_SHOWN "$viewed" 15
check "the one viewed is the second" wait_strip 10 '◯ main' 'Nine.'
check "and back to main by a click" wait_count "$SD" PEEK_LEFT "$left" 15
sleep 1
stops=$(count "$SD" HELPER_STRIP_STOP)
touch "$SD/go-x"
check "x stops the selected helper" wait_count "$SD" HELPER_STRIP_STOP "$stops" 20
check "which is shown stopped" wait_strip 20 'stopped'
for (( i = 0; i < 40; i++ )); do grep -qx 61 "$KEYS" && break; sleep 0.5; done
echo "    keys the agent read: $(tr '\n' ' ' < "$KEYS")"
check "the agent read each down that took the keys, and nothing the strip took" \
  test "$(grep -c . "$KEYS")" = 4 -a "$(grep -cx 1b5b42 "$KEYS")" = 3
check "a key that is not the strip's goes back to the agent" grep -qx 61 "$KEYS"

echo "════ 10. what a row says, and no blank row ════"
check "the row check takes a strip with its last row drawn" rows_drawn "|⏺ main|◯ a|◯ b" 4 head
check "and fails on one whose last row is blank" not rows_drawn "|⏺ main|◯ a|" 4 head
check "or holds only spaces" not rows_drawn "|⏺ main|◯ a|  " 4 head
SETTING=true
OWN="$SB/own-list"
rm -f "${OWN:?}" "${SB:?}/idle10" "${SB:?}/ask10"
# debug: the window's buffers are logged, for a picture of them
start_app MARSPOT_LOG=debug
start_parent FAKE_ALT=1 FAKE_OWN_LIST="$OWN"
EVENTS=$(grep -c '' "$SD/helpers/events.log" 2>/dev/null || true)
EVENTS=${EVENTS:-0}
HAIKU='◯ claude haiku {2,}sort the inbox {2,}'
SONNET='◯ claude sonnet {2,}review the parser {2,}'
n=$(sizes)
A=$(started claude "You are working in the mailrs repo. FAKE-MODEL=claude-haiku-4-5-20251001 FAKE-IDLE=$SB/idle10" \
  --model opus --name mailrs-inbox --title "sort the inbox") || die "no first helper"
check "the first: a blank row, main and its row, in one resize" one_resize_to "$n" $(( FULL - 3 )) 30
n=$(sizes)
B=$(started claude "You are working in the parser. FAKE-MODEL=claude-sonnet-4-6 FAKE-APPROVE=$SB/ask10" \
  --model haiku --name reviewer --title "review the parser") || die "no second helper"
check "the second: one more row, in one resize" one_resize_to "$n" $(( FULL - 4 )) 30
check "the name column: the agent and the model its transcript says answered" wait_strip 30 'claude haiku' 'claude sonnet'
check "then the title, a column gap from the name" wait_row_is 10 "$HAIKU"
check "for each" row_is "$SONNET"
check "not the name it was given, nor its task" bash -c '! grep -qE "mailrs-inbox|reviewer|You are working" <<<"$1"' _ "$(strip_now)"
check "alone: a blank row, main and the two rows, none under them" drawn 4 head
n=$(sizes)
touch "$SB/idle10"
check "the one whose turn ended says idle" wait_row_is 30 "${HAIKU}idle · "
check "within 5 s of its turn's end" within 5 "$(drawn_at IDLE)"
shown=$(count "$SD" DEV_STRIP)
touch "$SB/ask10"
check "the one waiting says awaiting approval" wait_row_is 60 "${SONNET}awaiting approval"
check "within 5 s of its prompt" within 5 "$(drawn_at APPROVE)"
check "and not before it" bash -c '! grep -a "$1" "$2" | head -n "$3" | grep -q awaiting_approval' _ $'\tDEV_STRIP\t' "$LOG" "$shown"
check "and the other is still idle" row_is "${HAIKU}idle · "
check "neither resized the pane" test "$(sizes)" = "$n"
sleep 1
shot alone
n=$(sizes)
touch "$OWN"
check "under Claude's own list: its two rows, in one resize" one_resize_to "$n" $(( FULL - 2 )) 20
for (( i = 0; i < 20; i++ )); do drawn 2 under >/dev/null && break; sleep 0.5; done
check "with no main, and none under them" drawn 2 under
check "they still say so" row_is "${SONNET}awaiting approval"
# past the screen's own judgement of a still prompt, ten seconds on
asked=$(drawn_at APPROVE)
while awk -v a="${asked:-0}" -v b="$(now)" 'BEGIN { exit !(b < a + 16) }'; do sleep 1; done
told=$(tail -n "+$(( EVENTS + 1 ))" "$SD/helpers/events.log" | grep -ac $'\tblocked_noticed')
check "its parent was told of the prompt once ($told)" test "$told" = 1
sleep 1
shot under-claudes-list
check "the first stops" quiet hc stop "$A"
check "and the second" quiet hc stop "$B"


check "the app is still up" alive "$(dev_shell_pid)"
check "no panic in the log" bash -c "! grep -qa $'\tPANIC' $(printf '%q' "$LOG")"

echo
echo "$((panes_checks - panes_failures - panes_not_applicable))/$panes_checks passed, $panes_not_applicable not applicable"
(( panes_failures == 0 ))

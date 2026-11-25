#!/usr/bin/env bash
#
# The zombie watch tells a claude session about work it left behind,
# end to end, in a sandboxed app with stand-ins for claude
# (bin/_zombie_fakes.c); no agent CLI runs.
#
#   pane A  three background tasks the session started with the same
#           command, silent a day (the older two are B1), and a
#           `tail -f` of a file two days old that launchd adopted (O1)
#   pane B  one background task that writes every second
#
# Both are shadow panes, so the person's cursor is in neither.  The
# sandbox's settings shrink the watch's times sixty-fold and turn
# `zombie.notify` to `live`.  Checked:
#   1. while A's turn runs its suspects are found and nothing is sent
#   2. once A's turn has ended, A's inbox gets exactly one message,
#      naming the two B1 suspects and the O1 one, and no more two
#      checks later; B gets nothing
#   3. a `marspot-peer procs stop` from A's session ends one of A's
#      suspects and is refused B's task (`not_yours`), and
#      control-audit.log has both
#
# Sandbox-only: MARSPOT_STATE_DIR and HOME are redirected; the
# installed app and the user's ~/.claude* are never touched.  Opens a
# window, so it runs on the runner:
#   bin/test-e2e-remote.sh test-zombie-watch.sh

set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
source "$ROOT/bin/_sandbox-panes.sh"

SD="$MARSPOT_STATE_DIR"
BIN="$ROOT/target/release"
CLI="$BIN/marspot-shell"
PEER="$BIN/marspot-peer"
LOG="$SD/logs/marspot.log"
SB=/tmp/marspot-zw.$$
DRY="$SD/zombie/dryrun.log"
UA=aaaaaaaa-1111-4222-8333-000000000001
UB=bbbbbbbb-1111-4222-8333-000000000002

stop_fakes() {
  local f pid
  # by the pids the stand-ins wrote down, never by name: `claude` is
  # also the name of the user's real sessions
  for f in "$SB"/A/claude.log "$SB"/B/claude.log; do
    [[ -f $f ]] || continue
    for pid in $(sed -n 's/.*pid=\([0-9]*\).*/\1/p' "$f"); do kill -9 "$pid" 2>/dev/null; done
  done
  [[ -f $SB/A/tail.pid ]] && kill -9 "$(cat "$SB/A/tail.pid")" 2>/dev/null
  return 0
}
# the stand-ins' own records, into the run's output: the directory
# holding them goes in every case
dump_fakes() {
  local f
  for f in "$SB"/A/claude.log "$SB"/A/inbox.log "$SB"/A/cmd.out "$SB"/A/cmd.status "$SB"/B/claude.log "$SB"/B/inbox.log; do
    [[ -f $f ]] || continue
    echo "  --- ${f#"$SB"/}:"; tail -40 "$f" | sed 's/^/    /'
  done
}
cleanup() {
  local rc=$?
  dev_kill_shell_core
  stop_fakes
  (( rc == 0 )) || dump_fakes
  rm -rf "${SB:?}"
}
trap cleanup EXIT
die() {
  echo "FAIL: $*"
  echo "  --- zombie events:"; grep -aE $'\tzombie\\.' "$LOG" 2>/dev/null | cut -f7- | tail -30 | sed 's/^/    /'
  echo "  --- $DRY:"; grep -a '^== ' "$DRY" 2>/dev/null | tail -20 | sed 's/^/    /'
  exit 1
}
pc() { in_sd "$SD" "$PEER" pane "$@"; }
field() { python3 -c 'import json,sys; v=json.load(sys.stdin); r=v.get("result",v); print(r.get(sys.argv[1], ""))' "$1"; }
iso() { python3 -c 'import datetime; print(datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.000Z"))'; }
# pid@start_us, the start time as the kernel keeps it (proc_bsdinfo)
proc_ident() {
  python3 - "$1" <<'PY'
import ctypes, struct, sys
pid = int(sys.argv[1])
buf = ctypes.create_string_buffer(136)
if ctypes.CDLL("/usr/lib/libSystem.B.dylib").proc_pidinfo(pid, 3, ctypes.c_uint64(0), buf, 136) != 136:
    sys.exit(1)
sec, usec = struct.unpack_from("<QQ", buf.raw, 120)
print(f"{pid}@{sec * 1000000 + usec}")
PY
}
wait_until() { # seconds, command…
  local secs=$1 i
  shift
  for (( i = 0; i < secs * 2; i++ )); do "$@" && return 0; sleep 0.5; done
  return 1
}
suspects() { # sid, rule
  sed -n "s/^== suspect [0-9]* sid=$1 rule=$2 id=\([0-9@]*\) .*/\1/p" "$DRY" 2>/dev/null | sort -u
}
lines() { [[ -f $1 ]] && wc -l < "$1" | tr -d ' ' || echo 0; }

for b in marspot-shell marspot-core marspot-session marspot-peer; do
  [[ -x $BIN/$b ]] || die "$BIN/$b is not built"
done

echo "==> the stand-ins"
rm -rf "${SB:?}"; mkdir -p "$SB/bin" "$SB/A/tasks" "$SB/B/tasks"
cc -O0 -o "$SB/bin/claude" "$ROOT/bin/_zombie_fakes.c" || die "cc failed"
for n in tail loop chatty; do cp "$SB/bin/claude" "$SB/bin/$n"; done
OLD="$SB/old.log"
echo old > "$OLD"
touch -t "$(date -v-2d +%Y%m%d%H%M)" "$OLD"
LOOP="loop --every 30"
{
  for i in 1 2 3; do printf 'bzwa0000%d\texec %s/bin/loop\n' "$i" "$SB"; done
  printf -- '-\t%s/bin/tail -f %s & echo $! > %s/A/tail.pid\n' "$SB" "$OLD" "$SB"
} > "$SB/A/tasks.tsv"
printf 'bzwb00001\texec %s/bin/chatty\n' "$SB" > "$SB/B/tasks.tsv"

export HOME="$SB/home"
CFG="$HOME/.claude-profile-1"
mkdir -p "$CFG/sessions" "$CFG/projects"

echo "==> launching the sandbox app"
dev_kill_shell_core
dev_wipe_state
rm -rf "${SD:?}/sessions" "${SD:?}/retired" "${SD:?}/zombie"
mkdir -p "$SD/logs" "$SD/zdot"
printf 'PS1="%%# "\nunsetopt BEEP\n' > "$SD/zdot/.zshrc"
printf 'zombie.notify = "live"\nzombie.scale_for_tests = 60\n' > "$SD/settings.toml"
env SHELL=/bin/zsh ZDOTDIR="$SD/zdot" MARSPOT_SESSION_BIN="$BIN/marspot-session" MARSPOT_CORE_BIN="$BIN/marspot-core" \
  MARSPOT_CC_IDLE_HIBERNATE_S=0 nohup "$CLI" > "$(dev_run_log zombie-watch)" 2>&1 < /dev/null &
dev_record_pid $!
wait_count "$SD" HELLO_ACK 0 90 || die "the core never said hello"
wait_count "$SD" PANE_CONTROL_CAPS_SEEN 0 30 || die "the shell never heard what the core does with pane control"
CAPS=$(grep -a $'\tPANE_CONTROL_CAPS_SEEN\t' "$LOG" | tail -1 | grep -o $'\tcaps=[0-9]*' | cut -d= -f2)
(( (${CAPS:-0} & 2) != 0 )) || die "the core opens no shadow panes (caps=${CAPS:-none})"
P1=""
wait_until 50 eval 'P1=$(pc list --json 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)[\"result\"][\"panes\"][0][\"id\"])" 2>/dev/null); [[ -n $P1 ]]' \
  || die "no pane in the window"
A=$(pc open --parent "$P1" --json | field id)
B=$(pc open --parent "$P1" --json | field id)
[[ -n $A && -n $B ]] || die "could not open two shadow panes ($A, $B)"
for s in "$A" "$B"; do wait_until 30 pc run "$s" --timeout 10 true >/dev/null 2>&1 || die "pane $s's shell never answered"; done
echo "==> pane A = $A, pane B = $B (shadow panes of $P1)"

start_fake() { # pane, dir, uuid
  pc send "$1" "FAKE_DIR=$2 FAKE_UUID=$3 FAKE_TASKS=$2/tasks.tsv CLAUDE_CONFIG_DIR=$CFG $SB/bin/claude" >/dev/null \
    || die "could not type into pane $1"
  wait_until 20 grep -q '^start ' "$2/claude.log" || die "the stand-in in pane $1 did not start"
}
start_fake "$A" "$SB/A" "$UA"
start_fake "$B" "$SB/B" "$UB"
wait_until 10 test -s "$SB/A/tail.pid" || die "the adopted tail was not started"

# the session's records: each task's call and the result naming it
transcript_of() { # dir, uuid
  local cwd
  cwd=$(sed -n 's/^start pid=[0-9]* cwd=//p' "$1/claude.log" | head -1)
  mkdir -p "$CFG/projects/$(tr / - <<<"$cwd")"
  echo "$CFG/projects/$(tr / - <<<"$cwd")/$2.jsonl"
}
record() { # file, uuid, json without type/session/time, type
  printf '{"type":"%s","sessionId":"%s","timestamp":"%s",%s}\n' "$4" "$2" "$(iso)" "$3" >> "$1"
}
started() { # file, uuid, call, task, command
  record "$1" "$2" "\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$3\",\"name\":\"Bash\",\"input\":{\"command\":\"$5\",\"run_in_background\":true}}]}" assistant
  record "$1" "$2" "\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$3\",\"content\":\"Command running in background with ID: $4. Output is being written to a file\"}]}" user
}
turn_ends() { # file, uuid
  record "$1" "$2" '"message":{"role":"assistant","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}' assistant
  record "$1" "$2" '"subtype":"turn_duration","durationMs":1000' system
}
TA=$(transcript_of "$SB/A" "$UA")
TB=$(transcript_of "$SB/B" "$UB")
record "$TA" "$UA" '"message":{"role":"user","content":"start"}' user
for i in 1 2 3; do started "$TA" "$UA" "toolu_a$i" "bzwa0000$i" "$LOOP"; done
record "$TB" "$UB" '"message":{"role":"user","content":"start"}' user
started "$TB" "$UB" toolu_b1 bzwb00001 "chatty"
turn_ends "$TB" "$UB"

echo "════ 1. a turn running: suspects found, nothing sent ════"
found_a() { (( $(suspects "$A" B1 | wc -l) == 2 && $(suspects "$A" O1 | wc -l) == 1 )); }
check "two B1 and one O1 suspects in pane A" wait_until 240 found_a
check "the message waits for the turn" wait_until 30 grep -aqE $'\tzombie\\.tell\\.waiting\t.*sid='"$A"'.*why=turn_running' "$LOG"
check "nothing sent while the turn runs" test "$(lines "$SB/A/inbox.log")" = 0

echo "════ 2. the turn ended: one message ════"
turn_ends "$TA" "$UA"
check "pane A's inbox got a message" wait_until 120 test -s "$SB/A/inbox.log"
sleep 15
check "exactly one, two checks later" test "$(lines "$SB/A/inbox.log")" = 1
for id in $(suspects "$A" B1) $(suspects "$A" O1); do
  check "the message names $id" grep -qF "$id" "$SB/A/inbox.log"
done
newest=$(sed -n 's/^task bzwa00003 pid=//p' "$SB/A/claude.log")
check "and not the newest copy (pid $newest)" bash -c "! grep -qF 'pid $newest@' '$SB/A/inbox.log'"
check "pane B got nothing" test "$(lines "$SB/B/inbox.log")" = 0
check "no suspect in pane B" test -z "$(grep -a "^== suspect [0-9]* sid=$B " "$DRY")"
check "zombie.told was logged" grep -aqE $'\tzombie\\.told\t.*sid='"$A" "$LOG"

echo "════ 3. procs stop from the session ════"
MINE=$(suspects "$A" B1 | head -1)
THEIRS=$(proc_ident "$(sed -n 's/^task bzwb00001 pid=//p' "$SB/B/claude.log")")
[[ -n $MINE && -n $THEIRS ]] || die "nothing to stop ($MINE, $THEIRS)"
echo "env MARSPOT_STATE_DIR=$SD $PEER procs stop $MINE $THEIRS" > "$SB/A/cmd.tmp" && mv "$SB/A/cmd.tmp" "$SB/A/cmd"
check "the session ran procs stop" wait_until 30 test -s "$SB/A/cmd.status"
sed 's/^/    /' "$SB/A/cmd.out" 2>/dev/null
audited() { grep -a '"op":"proc.stop"' "$SD/control-audit.log" 2>/dev/null | grep -F "\"id\":\"$1\"" | grep -qF "\"result\":\"$2\""; }
check "control-audit.log has the stop" wait_until 10 audited "$MINE" stopped
check "control-audit.log has the refusal" wait_until 10 audited "$THEIRS" not_yours
check "the suspect has ended" wait_until 10 bash -c "! kill -0 ${MINE%@*} 2>/dev/null"
check "pane B's task still runs" kill -0 "${THEIRS%@*}"

check "the app is still up" alive "$(dev_shell_pid)"
check "no panic in the log" bash -c "! grep -qa $'\tPANIC' '$LOG'"
echo
echo "$((panes_checks - panes_failures))/$panes_checks passed"
(( panes_failures == 0 )) || die "$panes_failures check(s) failed"
echo "PASS: the zombie watch told pane A once, and its session stopped its own suspect"

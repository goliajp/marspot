#!/usr/bin/env bash
#
# Helpers end to end: a session starts claude, codex, gemini and kimi
# helpers in shadow panes under its pane, and gets their results back
# in its inbox, in a sandboxed app.
#
# The parent pane runs a stand-in Claude session (_peerbus_fake_claude.py:
# a registry record and an inbox that logs what arrives).  The helpers'
# agents are stand-ins too (_fake-helper-agent.py, linked as claude,
# codex, gemini and kimi first on PATH), so no account and no model are
# used.  Gemini and Kimi have no usable account on this machine at all:
# their real command lines are covered only by unit tests and by these
# stand-ins, never by a real run here.
#
#   1. four helpers start, one per agent, and are listed;
#   2. each one's stop reaches the parent's inbox as exactly one message
#      carrying its result: codex waits at an empty input box (judged
#      from its quiet screen), the other three exit (gemini judged from
#      the screen, claude and kimi from the per-pane state); kimi's task
#      is over 32 KiB and is pasted in two sends;
#   3. `helper stop` closes the one left and removes its directory;
#   4. a helper past its budget is told and closed;
#   5. after the shell restarts, a helper recorded as started whose task
#      never went in is closed and its parent told, and a running one is
#      still listed.
#
# Sandbox-only: MARSPOT_STATE_DIR and HOME are redirected and the
# installed app is never touched.  Opens windows, so it runs on the
# runner:  bin/test-e2e-remote.sh test-agent-control.sh
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
SB=/tmp/marspot-agentctl.$$
REGISTRY="$SB/home/.claude"/sessions
INBOX="$SB/inbox.log"
export PEERBUS_REGISTRY="$REGISTRY"
export FAKE_HELPER_LOG="$SB/helpers.log"

die() {
  echo "FAIL: $*"
  echo "  --- helper lines in $LOG:"
  grep -aE 'helpers|HELPER' "$LOG" 2>/dev/null | cut -f5- | tail -30 | cut -c1-260 | sed 's/^/    /'
  echo "  --- events.log:"; tail -20 "$SD/helpers/events.log" 2>/dev/null | cut -c1-200 | sed 's/^/    /'
  echo "  --- inbox:"; cut -c1-300 "$INBOX" 2>/dev/null | tail -10 | sed 's/^/    /'
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
for p in .claude-profile-1 .codex-profile-1 .gemini-profile-1 .kimi-code-profile-1; do mkdir -p "$SB/home/$p"; done
for a in claude codex gemini kimi; do ln -s "$ROOT/bin/_fake-helper-agent.py" "$SB/bin/$a"; done
chmod +x "$ROOT/bin/_fake-helper-agent.py"
export HOME="$SB/home"
export PATH="$SB/bin:$PATH"
# the login shell's path_helper puts the system's directories first, where
# a real gemini or kimi may be: the stand-ins go first again after it, in
# the .zshrc of this test's HOME
printf 'export PATH="%s/bin:$PATH"\n' "$SB" > "$HOME/.zshrc"

pc() { in_sd "$SD" "$PEER" pane "$@"; }
hc() { in_sd "$SD" "$PEER" helper "$@"; }
field() { python3 -c 'import json,sys; v=json.load(sys.stdin); r=v.get("result",v); print(r.get(sys.argv[1], ""))' "$1"; }
notices_of() { # helper name → messages for it in the inbox
  local n
  n=$(grep -F "marspot helper notice" "$INBOX" 2>/dev/null | grep -cF "$1")
  echo "${n:-0}"
}
wait_notices() { # name, how many, seconds
  local i
  for (( i = 0; i < $3; i++ )); do
    (( $(notices_of "$1") >= $2 )) && return 0
    sleep 1
  done
  return 1
}
listed() { # name → whether helper list --all names it
  hc list --all --json | grep -qF "\"name\":\"$1\""
}
all_listed() { # names…
  local n
  for n in "$@"; do listed "$n" || return 1; done
}
under_parent() { # how many, seconds → whether that many shadow panes are under P1
  local i
  for (( i = 0; i < $2; i++ )); do
    (( $(pc list --json | grep -o "\"parent\":$P1[,}]" | wc -l) >= $1 )) && return 0
    sleep 1
  done
  return 1
}
none_listed() { hc list --all --json | grep -qF '"helpers":[]'; }
pane_gone() { # sid, seconds
  local i
  for (( i = 0; i < $2; i++ )); do
    pc list --json | grep -qF "\"id\":$1," || return 0
    sleep 1
  done
  return 1
}
start_helper() { # agent, budget, task… → the answer's JSON
  local agent=$1 budget=$2
  shift 2
  hc start --json --agent "$agent" --parent "$P1" --cwd "$SB/proj" --budget "$budget" "$@"
}

launch_app() {
  env MARSPOT_SESSION_BIN="$BIN/marspot-session" MARSPOT_CORE_BIN="$BIN/marspot-core" \
    nohup "$CLI" >> "$(dev_run_log agent-control)" 2>&1 < /dev/null &
  dev_record_pid $!
}

dev_kill_shell_core
dev_wipe_state
rm -rf "$SD/sessions" "$SD/retired"
mkdir -p "$SD/logs"
launch_app
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

echo "════ the parent: a Claude session with an inbox ════"
pc send "$P1" "cd $SB/proj && python3 $ROOT/bin/_peerbus_fake_claude.py $REGISTRY 1500 $INBOX" >/dev/null || die "could not start the stand-in Claude session"
for (( i = 0; i < 60; i++ )); do
  pc list --json | python3 -c "import json,sys; ps=json.load(sys.stdin)['result']['panes']; sys.exit(0 if any(p['id']==$P1 and p['agent']=='claude' for p in ps) else 1)" && break
  sleep 0.5
done
check "the parent pane runs claude, as the pane table says" bash -c "$(printf '%q' "$PEER") pane list --json | grep -q '\"agent\":\"claude\"'"

echo "════ 1. four helpers start ════"
KIMI_TASK=$(python3 -c 'print("\n".join("kimi task line %05d %s" % (i, "x" * 40) for i in range(700)) + "\nFAKE-EXIT END-OF-TASK")')
A_CODEX=$(start_helper codex 30 "Review the parser. FAKE-STAY")
A_GEMINI=$(start_helper gemini 30 "Summarise the log. FAKE-EXIT")
A_CLAUDE=$(start_helper claude 30 "Write the notes. FAKE-EXIT")
A_KIMI=$(printf '%s' "$KIMI_TASK" | start_helper kimi 30 -)
for a in CODEX GEMINI CLAUDE KIMI; do
  v="A_$a"
  check "$a: started on account P1 in mode auto" bash -c "grep -q '\"ok\":true' <<<'${!v}' && grep -q '\"account\":\"P1\"' <<<'${!v}' && grep -q '\"mode\":\"auto\"' <<<'${!v}'"
done
N_CODEX=$(field name <<<"$A_CODEX"); N_GEMINI=$(field name <<<"$A_GEMINI")
N_CLAUDE=$(field name <<<"$A_CLAUDE"); N_KIMI=$(field name <<<"$A_KIMI")
S_CODEX=$(field id <<<"$A_CODEX")
[[ -n $N_CODEX && -n $N_GEMINI && -n $N_CLAUDE && -n $N_KIMI ]] || die "a helper did not start: $A_CODEX $A_GEMINI $A_CLAUDE $A_KIMI"
check "helper list names all four" all_listed "$N_CODEX" "$N_GEMINI" "$N_CLAUDE" "$N_KIMI"
check "the four are shadow panes under the parent" under_parent 4 15

echo "════ 2. one message per stop, with the result ════"
for n in "$N_CODEX" "$N_GEMINI" "$N_CLAUDE" "$N_KIMI"; do
  wait_notices "$n" 1 180 || echo "    no message for $n after 180 s"
done
for pair in "codex:$N_CODEX" "gemini:$N_GEMINI" "claude:$N_CLAUDE" "kimi:$N_KIMI"; do
  a=${pair%%:*} n=${pair#*:}
  check "$a: the parent got a message carrying its result" bash -c "grep -F 'marspot helper notice' '$INBOX' | grep -F '$n' | grep -q 'RESULT-$a-OK'"
done
check "codex: told it waits for input" bash -c "grep -F '$N_CODEX' '$INBOX' | grep -q 'waits for its next input\|prompt that needs an answer'"
check "kimi: the whole task went in (two sends)" bash -c "awk -F'\t' '\$1==\"kimi\" && \$2 > 32768' '$FAKE_HELPER_LOG' | grep -q ."
sleep 20
for pair in "codex:$N_CODEX" "gemini:$N_GEMINI" "claude:$N_CLAUDE" "kimi:$N_KIMI"; do
  a=${pair%%:*} n=${pair#*:}
  check "$a: exactly one message" test "$(notices_of "$n")" = 1
done
for n in "$N_GEMINI" "$N_CLAUDE" "$N_KIMI"; do
  check "$n exited: no longer listed" bash -c "! $(printf '%q' "$PEER") helper list --all --json | grep -qF '$n'"
done

echo "════ 3. helper stop ════"
check "helper stop names what it stopped" bash -c "$(printf '%q' "$PEER") helper stop --json '$N_CODEX' | grep -qF '$N_CODEX'"
check "its pane is gone" pane_gone "$S_CODEX" 15
check "its directory is gone" test ! -e "$SD/helpers/$S_CODEX"
check "nothing is listed" none_listed

echo "════ 4. a budget runs out ════"
A_BUDGET=$(start_helper codex 1 "Wait for nothing. FAKE-SILENT")
N_BUDGET=$(field name <<<"$A_BUDGET"); S_BUDGET=$(field id <<<"$A_BUDGET")
[[ -n $N_BUDGET ]] || die "the budget helper did not start: $A_BUDGET"
for (( i = 0; i < 120; i++ )); do grep -F "$N_BUDGET" "$INBOX" 2>/dev/null | grep -q 'minute budget' && break; sleep 1; done
check "the parent was told the budget ran out" bash -c "grep -F '$N_BUDGET' '$INBOX' | grep -q '1-minute budget'"
check "and the helper's pane was closed" pane_gone "$S_BUDGET" 30

echo "════ 5. a restart of the shell ════"
A_KEEP=$(start_helper codex 30 "Keep going. FAKE-STAY")
N_KEEP=$(field name <<<"$A_KEEP")
wait_notices "$N_KEEP" 1 60 || echo "    no message for $N_KEEP"
ORPHAN=$(pc open --parent "$P1" --json | field id)
[[ -n $ORPHAN ]] || die "could not open the shadow pane that plays a helper whose task never went in"
now=$(python3 -c 'import time; print(int(time.time()*1000))')
printf '%s\t%s\t%s\tstarted\tparent=%s\tcaller=claude\tagent=codex\taccount=P1\tmodel=default\tmode=auto\tisolation=host\tbudget=30\ttask=never delivered\tsha256=00\tlabel=\n' \
  "$now" "$now" "$ORPHAN" "$P1" >> "$SD/helpers/events.log"
hellos=$(count "$SD" HELLO_ACK)
kill -TERM "$(dev_shell_pid)" 2>/dev/null
for (( i = 0; i < 60; i++ )); do [[ -z $(dev_shell_pid) ]] && break; sleep 0.5; done
launch_app
wait_count "$SD" HELLO_ACK "$hellos" 90 || die "the core never said hello after the restart"
check "the helper whose task never went in was closed" pane_gone "$ORPHAN" 90
# told once the restarted shell has named its panes, a moment after the close
for (( i = 0; i < 30; i++ )); do grep -qF 'task had not gone in' "$INBOX" 2>/dev/null && break; sleep 0.5; done
check "and its parent was told" bash -c "grep -F 'task had not gone in' '$INBOX' | grep -q ."
check "the running helper is still listed" listed "$N_KEEP"
hc stop --all >/dev/null 2>&1
hc stop "$N_KEEP" >/dev/null 2>&1

check "the app is still up" alive "$(dev_shell_pid)"
check "no panic in the log" bash -c "! grep -qa $'\tPANIC' $(printf '%q' "$LOG")"

echo
echo "$((panes_checks - panes_failures - panes_not_applicable))/$panes_checks passed, $panes_not_applicable not applicable"
(( panes_failures == 0 ))

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
#   5. claude's first-run dialogs: the trust dialog for the helper's own
#      directory, then the bypass one, are answered (moved off "No, exit",
#      never Enter on it) and written to the audit log, however late the
#      agent draws them; one that stays up gets one Enter and the parent is
#      told; a trust dialog for another directory is left alone and the
#      parent told once;
#   6. after the shell restarts, a helper recorded as started whose task
#      never went in is closed and its parent told, and a running one is
#      still listed;
#   7. a helper in a container, docker being a stand-in (_fake-docker.sh):
#      refused for want of a login that outlives the budget, preparing
#      while its image is built, its result told, its container removed
#      when its budget ends; an image built across a restart of the shell,
#      which also removes a container left by a helper that is gone; a
#      login that came to last too short a time; a container started
#      again with a renewed login; no codex, no gemini without a login.
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
export FAKE_DOCKER_DIR="$SB/docker"

die() {
  echo "FAIL: $*"
  echo "  --- helper lines in $LOG:"
  grep -aE 'helpers|HELPER' "$LOG" 2>/dev/null | cut -f5- | tail -30 | cut -c1-260 | sed 's/^/    /'
  echo "  --- events.log:"; tail -20 "$SD/helpers/events.log" 2>/dev/null | cut -c1-200 | sed 's/^/    /'
  echo "  --- inbox:"; cut -c1-300 "$INBOX" 2>/dev/null | tail -10 | sed 's/^/    /'
  exit 1
}
cleanup() {
  local rc=$? f
  # the stand-in containers, by the processes recorded for them
  for f in "$FAKE_DOCKER_DIR"/ps/*; do
    [[ -f $f ]] && kill "$(sed -n 1p "$f")" 2>/dev/null
  done
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
ln -s "$ROOT/bin/_fake-docker.sh" "$SB/bin/docker"
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
start_helper() { # agent, budget, task… → the answer's JSON; in $HELPER_CWD when set
  local agent=$1 budget=$2
  shift 2
  hc start --json --agent "$agent" --parent "$P1" --cwd "${HELPER_CWD:-$SB/proj}" --budget "$budget" "$@"
}

launch_app() {
  env MARSPOT_SESSION_BIN="$BIN/marspot-session" MARSPOT_CORE_BIN="$BIN/marspot-core" \
    nohup "$CLI" >> "$(dev_run_log agent-control)" 2>&1 < /dev/null &
  dev_record_pid $!
}

restart_app() {
  local hellos i
  hellos=$(count "$SD" HELLO_ACK)
  kill -TERM "$(dev_shell_pid)" 2>/dev/null
  for (( i = 0; i < 60; i++ )); do [[ -z $(dev_shell_pid) ]] && break; sleep 0.5; done
  launch_app
  wait_count "$SD" HELLO_ACK "$hellos" 90 || die "the core never said hello after the restart"
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

echo "════ 5. first-run dialogs ════"
DEEP="$SB/$(printf 'deep-%.0s' {1..30})/proj"
mkdir -p "$DEEP"
A_TRUST=$(HELPER_CWD="$DEEP" start_helper claude 30 "Look around. FAKE-TRUST FAKE-BYPASS FAKE-LATE FAKE-EXIT")
A_OTHER=$(start_helper claude 30 "Look elsewhere. FAKE-TRUST-OTHER FAKE-EXIT")
N_TRUST=$(field name <<<"$A_TRUST"); S_TRUST=$(field id <<<"$A_TRUST")
N_OTHER=$(field name <<<"$A_OTHER"); S_OTHER=$(field id <<<"$A_OTHER")
[[ -n $N_TRUST && -n $N_OTHER ]] || die "a dialog helper did not start: $A_TRUST $A_OTHER"
wait_notices "$N_OTHER" 1 60 || echo "    no message for $N_OTHER after 60 s"
wait_notices "$N_TRUST" 1 120 || echo "    no message for $N_TRUST after 120 s"
check "past both dialogs: the parent got its result" bash -c "grep -F '$N_TRUST' '$INBOX' | grep -q RESULT-claude-OK"
check "each dialog was moved to yes and entered there" bash -c "[[ \$(grep -a '^key' '$FAKE_HELPER_LOG' | cut -f2 | paste -sd, -) == 'arrow to Yes, I trust this folder,enter on Yes, I trust this folder,arrow to Yes, I accept,enter on Yes, I accept' ]]"
check "a screen with nothing drawn was not told as a stop" bash -c "! grep -F '$N_TRUST' '$INBOX' | grep -q 'prompt that needs an answer'"
A_STUCK=$(start_helper claude 30 "Look around. FAKE-TRUST FAKE-TRUST-STUCK FAKE-EXIT")
N_STUCK=$(field name <<<"$A_STUCK"); S_STUCK=$(field id <<<"$A_STUCK")
[[ -n $N_STUCK ]] || die "the stuck-dialog helper did not start: $A_STUCK"
wait_notices "$N_STUCK" 1 90 || echo "    no message for $N_STUCK after 90 s"
# past another round of looking at it
sleep 15
audited() { # sid, result → how many helper.trust lines of it
  grep -a '"op":"helper.trust"' "$SD/control-audit.log" | grep -F "\"sid\":$1," | grep -cF "\"result\":\"$2"
}
sleep 2
check "the trust is in the audit log with its directory" test "$(audited "$S_TRUST" "answered: /private$DEEP")" = 1
check "so is the bypass" test "$(audited "$S_TRUST" 'answered"')" = 1
check "a dialog that stays up got one Enter" test "$(grep -ac 'enter kept on Yes, I trust' "$FAKE_HELPER_LOG")" = 1
check "and one move" test "$(grep -ac 'arrow to Yes, I trust' "$FAKE_HELPER_LOG")" = 2
check "it is in the audit log as not answered" test "$(audited "$S_STUCK" 'not answered')" = 1
check "its parent was told once, of a prompt" bash -c "[[ \$(grep -F '$N_STUCK' '$INBOX' | grep -c 'prompt that needs an answer') == 1 ]]"
hc stop "$N_STUCK" >/dev/null 2>&1
check "another directory: left alone, and audited once" test "$(audited "$S_OTHER" 'left, not the helper')" = 1
check "another directory: the parent was told, with the screen" bash -c "grep -F '$N_OTHER' '$INBOX' | grep 'prompt that needs an answer' | grep -q 'Accessing workspace'"
check "told once" test "$(notices_of "$N_OTHER")" = 1
check "it is listed as stopped" bash -c "$(printf '%q' "$PEER") helper list --all --json | grep -F '$N_OTHER' | grep -q '\"state\":\"stopped\"'"
hc stop "$N_OTHER" >/dev/null 2>&1

echo "════ 6. a restart of the shell ════"
A_KEEP=$(start_helper codex 30 "Keep going. FAKE-STAY")
N_KEEP=$(field name <<<"$A_KEEP")
wait_notices "$N_KEEP" 1 60 || echo "    no message for $N_KEEP"
ORPHAN=$(pc open --parent "$P1" --json | field id)
[[ -n $ORPHAN ]] || die "could not open the shadow pane that plays a helper whose task never went in"
now=$(python3 -c 'import time; print(int(time.time()*1000))')
printf '%s\t%s\t%s\tstarted\tparent=%s\tcaller=claude\tagent=codex\taccount=P1\tmodel=default\tmode=auto\tisolation=host\tbudget=30\ttask=never delivered\tsha256=00\tlabel=\n' \
  "$now" "$now" "$ORPHAN" "$P1" >> "$SD/helpers/events.log"
restart_app
check "the helper whose task never went in was closed" pane_gone "$ORPHAN" 90
# told once the restarted shell has named its panes, a moment after the close
for (( i = 0; i < 30; i++ )); do grep -qF 'task had not gone in' "$INBOX" 2>/dev/null && break; sleep 0.5; done
check "and its parent was told" bash -c "grep -F 'task had not gone in' '$INBOX' | grep -q ."
check "the running helper is still listed" listed "$N_KEEP"
hc stop --all >/dev/null 2>&1
hc stop "$N_KEEP" >/dev/null 2>&1

echo "════ 7. a helper in a container ════"
TOKEN="fake-access-token-$$"
login_lasting() { # minutes
  python3 - "$HOME/.claude-profile-1/.credentials.json" "$TOKEN" "$1" <<'PY'
import json, os, sys, time
path, token, minutes = sys.argv[1:]
with open(path, "w") as f:
    json.dump({"claudeAiOauth": {"accessToken": token, "expiresAt": int((time.time() + 60 * int(minutes)) * 1000)}}, f)
os.chmod(path, 0o600)
PY
}
DOCKER_LOG="$FAKE_DOCKER_DIR/log"
login_lasting 25
A_SHORT=$(start_helper claude 30 --container "Look around. FAKE-STAY")
check "no login outlives the budget: no_account, with what is left" bash -c "grep -q no_account <<<\"\$1\" && grep -q 'P1 has 2[45] min left' <<<\"\$1\"" _ "$A_SHORT"
login_lasting 480
A_BOX=$(start_helper claude 1 --container "Look around. FAKE-STAY")
N_BOX=$(field name <<<"$A_BOX"); S_BOX=$(field id <<<"$A_BOX")
[[ -n $N_BOX ]] || die "the container helper did not start: $A_BOX"
check "the answer says preparing, in a container" bash -c "grep -q '\"state\":\"preparing\"' <<<\"\$1\" && grep -q '\"isolation\":\"container\"' <<<\"\$1\"" _ "$A_BOX"
check "the env file is private" test "$(stat -f %Lp "$SD/helpers/$S_BOX/env")" = 600
wait_notices "$N_BOX" 1 90 || echo "    no message for $N_BOX after 90 s"
event_at() { awk -F'\t' -v s="$S_BOX" -v e="$1" '$3 == s && $4 == e { print NR; exit }' "$SD/helpers/events.log"; }
check "preparing, then the task went in" bash -c "[[ -n '$(event_at preparing)' && '$(event_at preparing)' -lt '$(event_at task_delivered)' ]]"
check "the image was built from a Dockerfile" grep -q '^FROM ' "$FAKE_DOCKER_DIR/Dockerfile"
check "the parent got its result" bash -c "grep -F '$N_BOX' '$INBOX' | grep -q RESULT-claude-OK"
check "the run got its mounts and the env file" bash -c "grep -a '^run ' '$DOCKER_LOG' | grep -F -- ',target=/work' | grep -F -- '/share,target=/marspot' | grep -qF -- '--env-file $SD/helpers/$S_BOX/env'"
check "the agent had the token in its environment" grep -q "token of ${#TOKEN} bytes" "$DOCKER_LOG"
check "the token is on no command line and in no log" bash -c "! grep -rqF '$TOKEN' '$DOCKER_LOG' '$FAKE_HELPER_LOG' '$SD/logs' '$SD/helpers/events.log' '$SD/control-audit.log' '$INBOX'"
for (( i = 0; i < 100; i++ )); do grep -F "$N_BOX" "$INBOX" | grep -q 'minute budget' && break; sleep 1; done
check "the parent was told its budget ran out" bash -c "grep -F '$N_BOX' '$INBOX' | grep -q '1-minute budget'"
check "its pane was closed" pane_gone "$S_BOX" 30
for (( i = 0; i < 20; i++ )); do grep -q "^rm -f marspot-helper-.*-$S_BOX\$" "$DOCKER_LOG" && break; sleep 0.5; done
check "docker was told to remove its container" grep -q "^rm -f marspot-helper-.*-$S_BOX\$" "$DOCKER_LOG"
check "no container is left" bash -c "[[ -z \$(ls '$FAKE_DOCKER_DIR/ps') ]]"
for (( i = 0; i < 20; i++ )); do [[ ! -e "$SD/helpers/$S_BOX" ]] && break; sleep 0.5; done
check "its directory is gone" test ! -e "$SD/helpers/$S_BOX"
check "codex is not supported in a container" bash -c "grep -q not_supported <<<\"\$1\"" _ "$(start_helper codex 30 --container "Look around.")"
check "gemini with no login there: no_account" bash -c "grep -q no_account <<<\"\$1\"" _ "$(start_helper gemini 30 --container "Look around.")"
check "and nothing was made in its account" test ! -e "$HOME/.gemini-profile-1/.gemini/oauth_creds.json"
check "the home directory is refused" bash -c "grep -q bad_request <<<\"\$1\"" _ "$(HELPER_CWD="$HOME" start_helper claude 30 --container "Look around.")"
mkdir -p "$HOME/.gemini-profile-1/.gemini"
echo '{}' > "$HOME/.gemini-profile-1/.gemini/oauth_creds.json"
A_STOP=$(start_helper gemini 30 --container "Look around. FAKE-STAY")
N_STOP=$(field name <<<"$A_STOP"); S_STOP=$(field id <<<"$A_STOP")
check "with the image there, the next runs at once" grep -q '"state":"running"' <<<"$A_STOP"
wait_notices "$N_STOP" 1 100 || echo "    no message for $N_STOP after 100 s"
check "gemini: the parent got its result" bash -c "grep -F '$N_STOP' '$INBOX' | grep -q RESULT-gemini-OK"
check "gemini: its login is mounted" bash -c "grep -a '^run ' '$DOCKER_LOG' | grep -qF '.gemini-profile-1/.gemini/oauth_creds.json,target='"
hc stop "$N_STOP" >/dev/null 2>&1
for (( i = 0; i < 20; i++ )); do grep -q "^rm -f marspot-helper-.*-$S_STOP\$" "$DOCKER_LOG" && break; sleep 0.5; done
check "stopped: its container is removed" bash -c "grep -q '^rm -f marspot-helper-.*-$S_STOP\$' '$DOCKER_LOG' && [[ -z \$(ls '$FAKE_DOCKER_DIR/ps') ]]"

echo "════ 8. a container's login, and a restart of the shell ════"
A_OVER=$(start_helper claude 30 --container "Look around. FAKE-STAY FAKE-ASK")
N_OVER=$(field name <<<"$A_OVER"); S_OVER=$(field id <<<"$A_OVER")
wait_notices "$N_OVER" 1 60 || echo "    no message for $N_OVER after 60 s"
echo 1000 > "$SD/helpers/$S_OVER/expires"
runs=$(grep -ac '^run ' "$DOCKER_LOG")
pc send "$N_OVER" "FAKE-REFUSE" >/dev/null || echo "    could not type to $N_OVER"
wait_notices "$N_OVER" 2 60 || echo "    no second message for $N_OVER after 60 s"
check "a refused login: the parent is told it starts again" bash -c "grep -F '$N_OVER' '$INBOX' | grep -q 'the container is started again'"
for (( i = 0; i < 60; i++ )); do (( $(grep -ac '^run ' "$DOCKER_LOG") > runs )) && break; sleep 0.5; done
check "it ran again, after its container was removed" bash -c "grep -aA9 '^rm -f marspot-helper-.*-$S_OVER\$' '$DOCKER_LOG' | grep -q '^run .*-$S_OVER '"
for (( i = 0; i < 30; i++ )); do grep -a '^claude' "$FAKE_HELPER_LOG" | grep -q -- --continue && break; sleep 0.5; done
check "to go on with its conversation" bash -c "grep -a '^claude' '$FAKE_HELPER_LOG' | grep -q -- --continue"
for (( i = 0; i < 20; i++ )); do [[ $(cat "$SD/helpers/$S_OVER/expires" 2>/dev/null) != 1000 ]] && break; sleep 0.5; done
check "with the newer login" bash -c "(( \$(cat '$SD/helpers/$S_OVER/expires') > 1000 ))"

build_takes() { echo "$1" > "$FAKE_DOCKER_DIR/build-s"; rm -f "$FAKE_DOCKER_DIR"/images/*; }
build_takes 8
A_SHORT2=$(start_helper claude 30 --container "Look around. FAKE-STAY")
N_SHORT2=$(field name <<<"$A_SHORT2"); S_SHORT2=$(field id <<<"$A_SHORT2")
login_lasting 35
python3 -c 'import sys,time; open(sys.argv[1],"w").write(str(int((time.time()+35*60)*1000)))' "$SD/helpers/$S_SHORT2/expires"
for (( i = 0; i < 60; i++ )); do grep -F "$N_SHORT2" "$INBOX" | grep -q 'came to last less' && break; sleep 0.5; done
check "a login too short by the time the image is there: closed, with the reason" bash -c "grep -F '$N_SHORT2' '$INBOX' | grep -q 'came to last less'"
check "its pane is gone" pane_gone "$S_SHORT2" 30
check "its task never went in" test -z "$(S_BOX=$S_SHORT2 event_at task_delivered)"
login_lasting 480

build_takes 25
A_RE=$(start_helper claude 30 --container "Look around. FAKE-STAY")
N_RE=$(field name <<<"$A_RE"); S_RE=$(field id <<<"$A_RE")
check "preparing again" grep -q '"state":"preparing"' <<<"$A_RE"
sleep 600 &
LEFT=$!
printf '%s\n%s\n' "$LEFT" 999999 > "$FAKE_DOCKER_DIR/ps/marspot-helper-00000000-999999"
restart_app
wait_notices "$N_RE" 1 120 || echo "    no message for $N_RE after 120 s"
check "prepared across the restart: the parent got its result" bash -c "grep -F '$N_RE' '$INBOX' | grep -q RESULT-claude-OK"
check "its task went in once" test "$(awk -F'\t' -v s="$S_RE" '$3 == s && $4 == "task_delivered"' "$SD/helpers/events.log" | wc -l | tr -d ' ')" = 1
check "the container of a helper that is gone was removed" bash -c "[[ ! -e '$FAKE_DOCKER_DIR/ps/marspot-helper-00000000-999999' ]] && ! kill -0 $LEFT 2>/dev/null"
check "the running container was not" bash -c "ls '$FAKE_DOCKER_DIR/ps' | grep -q -- '-$S_OVER\$'"
hc stop "$N_RE" >/dev/null 2>&1
hc stop "$N_OVER" >/dev/null 2>&1
for (( i = 0; i < 40; i++ )); do [[ -z $(ls "$FAKE_DOCKER_DIR/ps") ]] && break; sleep 0.5; done
check "no container is left" bash -c "[[ -z \$(ls '$FAKE_DOCKER_DIR/ps') ]]"

check "the app is still up" alive "$(dev_shell_pid)"
check "no panic in the log" bash -c "! grep -qa $'\tPANIC' $(printf '%q' "$LOG")"

echo
echo "$((panes_checks - panes_failures - panes_not_applicable))/$panes_checks passed, $panes_not_applicable not applicable"
(( panes_failures == 0 ))

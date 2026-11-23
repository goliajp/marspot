# Sourced by test-peerbus.sh, with a Codex in pane $P1 and pane $P2 at its
# shell.  The parent is started in pane $P2 without permission prompts, a
# class the helper is not in: the two take each other's messages all the
# same, and nobody else of that class is heard by the helper.
#
# A helper as marspot starts it (`-a never -s workspace-write`, no
# override that would take it off its home's app-server), whose model
# calls SendMessage at once: sooner than marspot can have bound it.

# read by the running app-server when the helper's thread starts
python3 - "$HOME/.codex/config.toml" <<'PY'
import sys
t = open(sys.argv[1]).read()
at = 'args = ["mcp"]\n'
open(sys.argv[1], "w").write(t.replace(at, at + 'default_tools_approval_mode = "approve"\n'))
PY
ln -s .codex "$HOME/.codex-profile-1"

send_once "$P2" "cd $SB/proj && codex"
RP="$(wait_record "$P2" "" 60)" || fail "the helper's parent in pane $P2 never got an inbox"
HPARENT="$(cut -f1 <<<"$(wait_name "$P2" '^proj#[1-9][0-9]{3}$' 25)")"
[[ "$HPARENT" =~ ^proj#[1-9][0-9]{3}$ ]] || fail "pane $P2 does not show a full name: $HPARENT"
helper() { MARSPOT_STATE_DIR="$MARSPOT_STATE_DIR" "$PEER_BIN" helper "$@"; }
helper_field() { python3 -c 'import json,sys; v=json.load(sys.stdin); print(v.get("result",v).get(sys.argv[1], ""))' "$1" 2>/dev/null; }
HJSON="$(helper start --json --agent codex --parent "$P2" --cwd "$SB/proj" --budget 10 --mode auto "Send $HPARENT the text PEERBUS-HELPER-ONE")"
HSID="$(helper_field id <<<"$HJSON")"; HNAME="$(helper_field name <<<"$HJSON")"
[[ -n "$HSID" && "$HNAME" =~ ^proj#[1-9][0-9]{3}$ ]] || fail "the helper did not start: $HJSON"
for (( i = 0; i < 60; i++ )); do
  HREC="$(records | awk -F'\t' -v n="$HNAME" '$1 == n')"
  (( $(seen "$P2" PEERBUS-HELPER-ONE) > 0 )) && [[ -n "$HREC" ]] && break
  sleep 0.5
done
if (( $(seen "$P2" PEERBUS-HELPER-ONE) == 0 )); then
  echo "  --- the helper's screen:"; "$SHELL_BIN" --read "$HSID" 2>&1 | tail -30 | sed 's/^/    /'
  fail "the helper's message never reached pane $P2"
fi
[[ -n "$HREC" ]] || fail "no record is called $HNAME: $(records)"
grep -qF "from-name=\\\"$HNAME\\\"" "$SB/model-requests.log" || fail "the message does not carry the helper's name $HNAME"
ok "a Codex helper's SendMessage, made as it started, reached its parent from $HNAME"

mcp_send "$(cut -f3 <<<"$RP")" "$HNAME" PEERBUS-TO-HELPER >/dev/null || fail "the parent could not send to $HNAME"
for (( i = 0; i < 60; i++ )); do (( $(seen "$HSID" PEERBUS-TO-HELPER) > 0 )) && break; sleep 0.5; done
(( $(seen "$HSID" PEERBUS-TO-HELPER) > 0 )) || fail "the parent's reply never showed in the helper's pane: $("$SHELL_BIN" --read "$HSID" 2>&1 | tail -20)"
ok "the parent's reply reached the helper's inbox"

# pane $P1's Codex, in the parent's class, has nothing to do with it
refusals() { grep -c 'peerbus.refused' "$APPLOG"; }
BEFORE="$(refusals)"
mcp_send "$(cut -f3 <<<"$R1B")" "$HNAME" PEERBUS-STRANGER >/dev/null || fail "pane $P1 could not hand its message to the helper's inbox"
for (( i = 0; i < 40; i++ )); do (( $(refusals) > BEFORE )) && break; sleep 0.5; done
(( $(refusals) > BEFORE )) || fail "a message of another class from a session that is not the parent was not refused"
(( $(seen "$HSID" PEERBUS-STRANGER) == 0 )) || fail "a refused message showed in the helper's pane"
ok "another session of the parent's class is still refused by the helper"

helper stop "$HNAME" >/dev/null || fail "the helper was not stopped"
HSOCK="$(cut -f4 <<<"$HREC")"
for (( i = 0; i < 40; i++ )); do records | grep -qF "$HSOCK" || break; sleep 0.5; done
records | grep -qF "$HSOCK" && fail "the helper's record stayed after it was stopped"
ok "stopping the helper removed its record"

send_once "$P2" "/quit"
for (( i = 0; i < 40; i++ )); do record_of_pane "$P2" >/dev/null || break; sleep 0.5; done
record_of_pane "$P2" >/dev/null && fail "the parent's record stayed after it quit"
rm -f "$SB/ready-$P2"
shell_ready "$P2"

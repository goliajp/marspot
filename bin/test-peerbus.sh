#!/usr/bin/env bash
#
# Messages between a Claude Code session and Codex panes, end to end:
# a sandbox marspot with two panes, a real Codex TUI in each, the real
# Codex app-server, the real registry, and replies that Codex itself
# sends with the marspot MCP tools (`marspot-peer mcp`).
#
# What it proves:
#   - the binding: an inbox reaches the pane it was made for and the
#     conversation that pane shows, in the two cases the directory
#     alone cannot settle -- Codex quit and reopened in the same pane
#     while the thread it left is still loaded, and a second Codex in
#     the same directory.  Each message appears on its own pane only;
#   - names: every session is addressed by its pane's name.  A pane
#     alone in its directory is `proj`; once two panes share it each
#     shows its full name, `proj#<four-digit code>`.  On both sending
#     paths -- the MCP tools a Codex calls and the Claude Code hook --
#     a shared plain name is refused with the candidates, `kind:name`
#     picks the pane by its agent unless two panes run the same one, a
#     full name reaches its pane after the pane changed agent or
#     directory, and the name of a pane whose session is gone is
#     unknown;
#   - idle notices: a subscription placed with a message comes back as
#     one `peer_idle_notice` once the Codex session is idle again.
#
# No account and no model: Codex talks to `_peerbus_mock_model.py`,
# which calls the marspot SendMessage tool when a delivered message
# asks for it.  The Claude side is played by `_peerbus_claude_side.py`
# (and `_peerbus_fake_claude.py` for a registered Claude session),
# speaking the Claude Code socket protocol.
#
# Sandbox-only: MARSPOT_STATE_DIR *and* HOME are redirected, so the
# Codex home, the registry and the profiles are this test's own.  Opens
# windows: run it through bin/test-e2e-remote.sh, never on the machine
# someone is working at.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

CODEX_VERSION=0.163.0
# The packaged layout, not the bare binary: a bare `codex` refuses to
# start its app-server ("this CLI has no complete local package").
CODEX_SHA256=b0344fc2e41f15e0071147e5869008047b8ff4bf2c4a8897975d3bae3f103b34
CODEX_URL="https://github.com/openai/codex/releases/download/rust-v$CODEX_VERSION/codex-package-aarch64-apple-darwin.tar.gz"
CODEX_CACHE="$HOME/.cache/marspot-e2e/codex-$CODEX_VERSION"

# Short on purpose: Codex puts unix sockets under $HOME/.codex, and a
# socket path has to fit in 104 bytes.
SB=/tmp/marspot-peerbus
RUNLOG=$SB/run.log
# the Claude Code registry, under this test's HOME
REGISTRY="$SB/home/.claude"/sessions
# the Claude side checks socket peers against the records, as Claude does
export PEERBUS_REGISTRY="$REGISTRY"
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"
PEER_BIN="$ROOT/target/release/marspot-peer"

fail() {
  echo "FAIL: $*"
  echo "  --- records:"; cat "$REGISTRY"/*.json 2>/dev/null | sed 's/^/    /'
  echo "  --- last 30 shell lines mentioning peerbus:"
  grep -i peerbus "$APPLOG" 2>/dev/null | tail -30 | cut -c1-240 | sed 's/^/    /'
  for sid in ${SIDS:-}; do
    echo "  --- pane $sid:"; "$SHELL_BIN" --read "$sid" 2>&1 | tail -15 | sed 's/^/    /'
  done
  exit 1
}
ok() { echo "  ✓ $1"; }

# pid of a process this test started, from a Codex pid file (JSON with
# a "pid" field), only when its command line is under this test's home
codex_pid_from() {
  local p
  p="$(sed -n 's/^{"pid":\([0-9]*\).*/\1/p' "$1" 2>/dev/null)"
  [[ -n "$p" ]] && ps -ww -o command= -p "$p" 2>/dev/null | grep -qF "$SB/" && echo "$p"
}

cleanup() {
  dev_kill_shell_core
  # By recorded pids, never by name: `codex` and `python3` are running
  # for other people on this machine.
  local f p
  for f in "$SB/home/.codex/app-server-daemon/daemon.pid" "$SB/home/.codex/app-server-daemon/daemon-updater.pid"; do
    p="$(codex_pid_from "$f")"
    [[ -n "$p" ]] && kill "$p" 2>/dev/null
  done
  [[ -f "$SB/mock.pid" ]] && kill "$(cat "$SB/mock.pid")" 2>/dev/null
  [[ -n "${FAKE_PID:-}" ]] && kill "$FAKE_PID" 2>/dev/null
  # the inboxes this run's records name, and the Claude side's own
  for p in $(records 2>/dev/null | cut -f4) /tmp/cc-socks/e2e-*.sock; do rm -f "$p"; done
}
trap cleanup EXIT

wait_for() { # regex, seconds
  local re="$1" secs="$2" i
  for (( i = 0; i < secs * 5; i++ )); do
    grep -qE "$re" "$APPLOG" 2>/dev/null && return 0
    sleep 0.2
  done
  return 1
}

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" || ! -x "$SESSION_BIN" || ! -x "$PEER_BIN" ]]; then
  echo "==> building release binaries"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 \
    && cargo build --release -p marspot-peer 2>&1 | tail -3 )
fi

# ── Codex, pinned ─────────────────────────────────────────────────
codex_entry() { # the package's entrypoint, from its manifest
  local m
  m="$(find "$CODEX_CACHE" -maxdepth 3 -name codex-package.json | head -1)"
  [[ -n "$m" ]] && echo "$(dirname "$m")/$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["entrypoint"])' "$m")"
}
if [[ ! -x "$(codex_entry)" ]]; then
  echo "==> fetching Codex $CODEX_VERSION"
  rm -rf "$CODEX_CACHE"; mkdir -p "$CODEX_CACHE"
  curl -fsSL -o "$CODEX_CACHE/codex.tar.gz" "$CODEX_URL" || { echo "download failed"; exit 1; }
  got="$(shasum -a 256 "$CODEX_CACHE/codex.tar.gz" | cut -d' ' -f1)"
  [[ "$got" == "$CODEX_SHA256" ]] || { echo "Codex archive digest $got, expected $CODEX_SHA256"; exit 1; }
  tar -xzf "$CODEX_CACHE/codex.tar.gz" -C "$CODEX_CACHE"
  rm "$CODEX_CACHE/codex.tar.gz"
  [[ -x "$(codex_entry)" ]] || { echo "no Codex entrypoint in the package"; find "$CODEX_CACHE" -maxdepth 3 | head; exit 1; }
fi

# ── this test's own HOME ──────────────────────────────────────────
rm -rf "$SB"; mkdir -p "$SB/bin" "$SB/proj" "$REGISTRY" "$SB/home/.codex"
cp "$PEER_BIN" "$SB/bin/marspot-peer"

python3 "$ROOT/bin/_peerbus_mock_model.py" "$SB/port" "$SB/model-requests.log" > "$SB/mock.log" 2>&1 &
echo $! > "$SB/mock.pid"
for (( i = 0; i < 50; i++ )); do [[ -s "$SB/port" ]] && break; sleep 0.1; done
[[ -s "$SB/port" ]] || { echo "the model stand-in did not start"; cat "$SB/mock.log"; exit 1; }

export HOME="$SB/home"
cat > "$HOME/.codex/config.toml" <<EOF
model = "mock-model"
model_provider = "mock"
approval_policy = "never"
sandbox_mode = "danger-full-access"
check_for_update_on_startup = false

[model_providers.mock]
name = "mock"
base_url = "http://127.0.0.1:$(cat "$SB/port")/v1"
wire_api = "responses"

[projects."$SB/proj"]
trust_level = "trusted"

[mcp_servers.marspot]
command = "$SB/bin/marspot-peer"
args = ["mcp"]

[tui]
screen_reader_detection_done = true
terminal_title = ["activity", "thread-name", "project-name", "thread-id"]
EOF
# the package's own bin/ on PATH, not a link to it: Codex finds the
# rest of its package next to the path it was started from
export PATH="$(dirname "$(codex_entry)"):$SB/bin:$PATH"

echo "==> launching with two panes"
dev_kill_shell_core
rm -rf "$MARSPOT_STATE_DIR"; mkdir -p "$MARSPOT_STATE_DIR/logs"
launch() {
  MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" MARSPOT_DEV_EXTRA_WINDOWS=1 \
    nohup "$SHELL_BIN" >>"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
  disown
}
launch

wait_for 'L3_SPAWNED' 90 || fail "no pane session was ever spawned"
SIDS=""
for (( i = 0; i < 150; i++ )); do
  SIDS="$("$SHELL_BIN" --panes 2>/dev/null | awk 'NR>1 {print $1}' | sort -n | tr '\n' ' ')"
  [[ $(wc -w <<<"$SIDS") -ge 2 ]] && break
  sleep 0.4
done
read -r P1 P2 _ <<<"$SIDS"
[[ -n "${P2:-}" ]] || fail "two panes never came up (have: $SIDS)"
echo "==> panes $P1 $P2"

# typed only once each pane's shell is reading; see
# test-codex-profile-switch.sh for why a retried command is poison
shell_ready() {
  local sid=$1 mark="$SB/ready-$1" attempt i
  for (( attempt = 1; attempt <= 10; attempt++ )); do
    "$SHELL_BIN" --send "$sid" "echo reading > $mark" >/dev/null 2>&1 || continue
    for (( i = 0; i < 15; i++ )); do
      [[ -f "$mark" ]] && return 0
      sleep 0.2
    done
  done
  fail "pane $sid's shell never ran anything typed into it"
}
send_once() { # pane, text: retried only while refused, which queues nothing
  local i
  for (( i = 0; i < 40; i++ )); do
    "$SHELL_BIN" --send "$1" "$2" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  fail "--send to pane $1 was refused 40 times"
}

records() { # name<TAB>codex pid<TAB>thread<TAB>socket, one per bound Codex pane
  python3 - "$REGISTRY" <<'PY'
import glob, json, os, sys
for p in sorted(glob.glob(os.path.join(sys.argv[1], "*.json"))):
    r = json.load(open(p))
    if r.get("marspotOwner"):
        print("\t".join(str(r.get(k, "")) for k in ("name", "marspotAgentPid", "sessionId", "messagingSocketPath")))
PY
}
name_of_pid() { # pid → name<TAB>status, from any record
  python3 - "$REGISTRY/$1.json" <<'PY' 2>/dev/null
import json, sys
r = json.load(open(sys.argv[1]))
print("%s\t%s" % (r.get("name"), r.get("status")))
PY
}
wait_name() { # pane, regex, seconds → the pane's record once its name matches
  local i line
  for (( i = 0; i < $3 * 2; i++ )); do
    line="$(record_of_pane "$1")" && [[ "$(cut -f1 <<<"$line")" =~ $2 ]] && { echo "$line"; return 0; }
    sleep 0.5
  done
  return 1
}
record_of_pane() { # pane → its record line, through the Codex's parent shell
  local sid=$1 shell line pid
  shell="$(sed -n 's/^shell_child_pid = //p' "$(find "$MARSPOT_STATE_DIR" -path "*/sessions/$sid/entry.toml" | head -1)")"
  while IFS= read -r line; do
    pid="$(cut -f2 <<<"$line")"
    [[ "$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')" == "$shell" ]] && { echo "$line"; return 0; }
  done < <(records)
  return 1
}
wait_record() { # pane, thread it must not be (or ""), seconds
  local i line
  for (( i = 0; i < $3 * 2; i++ )); do
    line="$(record_of_pane "$1")" && [[ "$(cut -f3 <<<"$line")" != "$2" ]] && { echo "$line"; return 0; }
    sleep 0.5
  done
  return 1
}
seen() { # pane, text → how many times it is on the pane's screen
  "$SHELL_BIN" --read "$1" -n 200 2>/dev/null | grep -cF "$2"
}
round_trip() { # pane, record line, tag, [notify]
  local sock tag=$3 out
  sock="$(cut -f4 <<<"$2")"
  out="$(python3 "$ROOT/bin/_peerbus_claude_side.py" "$sock" 90 PEERBUS-E2E-REPLY "$tag" ${4:-})" \
    || fail "no reply${4:+ or idle notice} through $(cut -f1 <<<"$2"): $out"
  grep -qF "\"from\": \"uds:$sock\"" <<<"$out" || fail "the reply did not come from the inbox written to: $out"
  local i
  for (( i = 0; i < 20; i++ )); do (( $(seen "$1" "$tag") > 0 )) && break; sleep 0.5; done
  (( $(seen "$1" "$tag") > 0 )) || fail "$tag never showed on pane $1"
}

# ── 1. reopened in the same pane ──────────────────────────────────
shell_ready "$P1"; shell_ready "$P2"
send_once "$P1" "cd $SB/proj && codex"
R1="$(wait_record "$P1" "" 60)" || fail "pane $P1's Codex never got an inbox"
T1="$(cut -f3 <<<"$R1")"
ok "pane $P1 bound: $(cut -f1 <<<"$R1") thread $T1"
[[ "$(cut -f1 <<<"$R1")" == proj ]] || fail "pane $P1's session is not named after its pane: $(cut -f1 <<<"$R1")"
ok "the session is named after its pane: proj"
round_trip "$P1" "$R1" PEERBUS-TAG-ONE notify
ok "a message to it reached pane $P1, Codex answered with SendMessage from that inbox, and the idle subscription came back"

send_once "$P1" "/quit"
# the record outlives the process by up to a tick of the plugin
SOCK1="$(cut -f4 <<<"$R1")"
for (( i = 0; i < 40; i++ )); do
  records | grep -qF "$SOCK1" || [[ -e "$SOCK1" ]] || break
  sleep 0.5
done
records | grep -qF "$SOCK1" && fail "the record stayed after Codex quit"
[[ -e "$SOCK1" ]] && fail "the inbox socket stayed after Codex quit"
ok "quitting Codex removed its record and socket"

send_once "$P1" "codex"
R1B="$(wait_record "$P1" "$T1" 60)" \
  || fail "the reopened Codex never got an inbox (the thread it left is still loaded)"
ok "the reopened Codex bound to its new thread $(cut -f3 <<<"$R1B"), not the one it left"
round_trip "$P1" "$R1B" PEERBUS-TAG-TWO

# ── 2. a second Codex in the same directory ───────────────────────
send_once "$P2" "cd $SB/proj && codex"
R2="$(wait_record "$P2" "" 60)" || fail "pane $P2's Codex never got an inbox"
[[ "$(cut -f3 <<<"$R2")" != "$(cut -f3 <<<"$R1B")" ]] || fail "both panes were bound to one thread"
ok "pane $P2 bound: $(cut -f1 <<<"$R2") thread $(cut -f3 <<<"$R2")"
round_trip "$P2" "$R2" PEERBUS-TAG-THREE
(( $(seen "$P1" PEERBUS-TAG-THREE) == 0 )) || fail "pane $P2's message also showed on pane $P1"
(( $(seen "$P2" PEERBUS-TAG-TWO) == 0 )) || fail "pane $P1's message showed on pane $P2"
ok "each message showed only on its own pane"

# ── names: two Codex panes in one directory ───────────────────────
mcp_send() { # sending thread, recipient, text → the tool's answer; fails when the tool reports an error
  python3 - "$SB/bin/marspot-peer" "$1" "$2" "$3" <<'PY'
import json, subprocess, sys
req = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
    "name": "SendMessage", "arguments": {"to": sys.argv[3], "message": sys.argv[4]}, "_meta": {"threadId": sys.argv[2]}}}
out = subprocess.run([sys.argv[1], "mcp"], input=json.dumps(req) + "\n", capture_output=True, text=True).stdout
r = json.loads(out.splitlines()[0])["result"]
print(r["content"][0]["text"])
sys.exit(1 if r["isError"] else 0)
PY
}
hook_send() { # recipient → what the Claude Code SendMessage hook answers
  printf '{"hook_event_name":"PreToolUse","tool_name":"SendMessage","session_id":"x","tool_input":{"to":"%s","message":"hi"}}' "$1" \
    | "$SB/bin/marspot-peer" claude-hook
}
hook_field() { # hook answer, field of hookSpecificOutput ("updatedInput.to" for the rewritten recipient)
  python3 - "$1" "$2" <<'PY' 2>/dev/null
import json, sys
v = json.loads(sys.argv[1])["hookSpecificOutput"]
for k in sys.argv[2].split("."):
    v = v[k]
print(v)
PY
}
hook_to() { hook_field "$(hook_send "$1")" updatedInput.to; }
hook_refusal() { hook_field "$(hook_send "$1")" permissionDecisionReason; }
shows() { # pane, text, what
  local i
  for (( i = 0; i < 60; i++ )); do (( $(seen "$1" "$2") > 0 )) && return 0; sleep 0.5; done
  fail "$3: $2 never showed on pane $1"
}
pane_entry() { # socket → name<TAB>code<TAB>base<TAB>agent, from marspot's pane table
  python3 - "$REGISTRY" "$1" <<'PY'
import glob, json, os, sys
for p in glob.glob(os.path.join(sys.argv[1], ".marspot-panes-*.json")):
    for e in json.load(open(p)).get("panes", []):
        if e["socket"] == sys.argv[2]:
            print("\t".join(str(e.get(k, "")) for k in ("name", "code", "base", "agent")))
PY
}
wait_entry() { # socket, regex for the name, seconds → the pane-table entry once its name matches
  local i line
  for (( i = 0; i < $3 * 2; i++ )); do
    line="$(pane_entry "$1")"; [[ "$(cut -f1 <<<"$line")" =~ $2 ]] && { echo "$line"; return 0; }
    sleep 0.5
  done
  return 1
}

FULL='^proj#[1-9][0-9]{3}$'
R1C="$(wait_name "$P1" "$FULL" 25)" || fail "pane $P1's session does not show a full name: $(record_of_pane "$P1")"
R2C="$(wait_name "$P2" "$FULL" 25)" || fail "pane $P2's session does not show a full name: $(record_of_pane "$P2")"
N1="$(cut -f1 <<<"$R1C")"; N2="$(cut -f1 <<<"$R2C")"
S1="$(cut -f4 <<<"$R1C")"; S2="$(cut -f4 <<<"$R2C")"
TH1="$(cut -f3 <<<"$R1C")"; TH2="$(cut -f3 <<<"$R2C")"
[[ "$N1" != "$N2" ]] || fail "both panes carry one code: $N1"
ok "two panes in one directory show their full names: $N1 and $N2"

python3 "$ROOT/bin/_peerbus_claude_side.py" "$S2" 30 "" "Send $N1 the text PEERBUS-C2C-ONE" >/dev/null \
  || fail "the forwarding request did not reach pane $P2"
shows "$P1" PEERBUS-C2C-ONE "Codex in pane $P2 sent to $N1"
ok "Codex in pane $P2 messaged the Codex in pane $P1 by its full name"

# the shared plain name, and an agent both run, are refused with the candidates
for to in proj codex:proj; do
  out="$(mcp_send "$TH2" "$to" PEERBUS-REFUSED)" && fail "the MCP tool sent to $to although two panes fit: $out"
  grep -qF "2 panes are named \"$to\"" <<<"$out" && grep -qF "$N1 (codex)" <<<"$out" && grep -qF "$N2 (codex)" <<<"$out" \
    || fail "the MCP refusal of $to does not list both panes: $out"
  out="$(hook_refusal "$to")"
  grep -qF "2 panes are named \"$to\"" <<<"$out" && grep -qF "$N1 (codex)" <<<"$out" && grep -qF "$N2 (codex)" <<<"$out" \
    || fail "the hook's refusal of $to does not list both panes: $(hook_send "$to")"
done
(( $(seen "$P1" PEERBUS-REFUSED) == 0 )) || fail "a refused message was delivered"
ok "proj and codex:proj are refused on both paths, each listing $N1 and $N2 as codex"
[[ "$(hook_to "$N1")" == "uds:$S1" && "$(hook_to "$N2")" == "uds:$S2" ]] || fail "the hook does not send each full name to its pane: $(hook_send "$N1") $(hook_send "$N2")"
mcp_send "$TH2" "$N1" PEERBUS-MCP-FULL >/dev/null || fail "the MCP tool refused $N1"
shows "$P1" PEERBUS-MCP-FULL "an MCP send to $N1"
ok "each full name reaches its own pane on both paths"

# ── a Claude session beside a Codex one, in one directory ─────────
start_fake() { # → FAKE_PID and FAKE_SOCK of a stand-in Claude session started in pane $P2
  local before="${FAKE_PID:-}" i
  send_once "$P2" "python3 $ROOT/bin/_peerbus_fake_claude.py $REGISTRY 240 $SB/fake-inbox.log"
  for (( i = 0; i < 40; i++ )); do
    FAKE_PID="$(grep -lE '"entrypoint": *"cli"' "$REGISTRY"/*.json 2>/dev/null | head -1 | xargs -n1 basename 2>/dev/null | sed 's/\.json$//')"
    [[ -n "$FAKE_PID" && "$FAKE_PID" != "$before" ]] && break
    sleep 0.5
  done
  [[ -n "$FAKE_PID" && "$FAKE_PID" != "$before" ]] || fail "the stand-in Claude session never registered"
  FAKE_SOCK="/tmp/cc-socks/e2e-fake-$FAKE_PID.sock"
}
stop_fake() { # by its recorded pid; gone once its record is
  local i
  kill "$FAKE_PID" 2>/dev/null
  for (( i = 0; i < 40; i++ )); do [[ -e "$REGISTRY/$FAKE_PID.json" ]] || return 0; sleep 0.25; done
  fail "the stand-in Claude session did not leave"
}
got() { # text → whether the stand-in Claude's inbox received it
  local i
  for (( i = 0; i < 40; i++ )); do grep -qF "$1" "$SB/fake-inbox.log" 2>/dev/null && return 0; sleep 0.25; done
  return 1
}
send_once "$P2" "/quit"
shell_ready "$P2"
start_fake
E2="$(wait_entry "$FAKE_SOCK" "$FULL" 25)" || fail "the Claude session in pane $P2 is not in the pane table under a full name: $(pane_entry "$FAKE_SOCK")"
[[ "$(cut -f1 <<<"$E2")" == "$N2" ]] || fail "pane $P2 changed its code when its agent changed: was $N2, is $(cut -f1 <<<"$E2")"
[[ "$(cut -f4 <<<"$E2")" == claude ]] || fail "the pane table does not say pane $P2 runs claude: $E2"
[[ "$(cut -f1 <<<"$(name_of_pid "$FAKE_PID")")" == fake-claude-6a ]] || fail "the Claude session's own record was renamed: $(name_of_pid "$FAKE_PID")"
ok "pane $P2 keeps $N2 with a Claude session in it, and that session keeps its own name"

out="$(mcp_send "$TH1" proj PEERBUS-REFUSED-TWO)" && fail "the MCP tool sent to proj although two panes fit: $out"
grep -qF "$N1 (codex)" <<<"$out" && grep -qF "$N2 (claude)" <<<"$out" && grep -qF "or to" <<<"$out" && grep -qF "claude:proj" <<<"$out" && grep -qF "codex:proj" <<<"$out" \
  || fail "the MCP refusal of proj does not name both panes, their agents and the two ways out: $out"
out="$(hook_refusal proj)"
grep -qF "$N1 (codex)" <<<"$out" && grep -qF "$N2 (claude)" <<<"$out" && grep -qF "claude:proj" <<<"$out" \
  || fail "the hook's refusal of proj does not name both panes and their agents: $(hook_send proj)"
[[ "$(hook_to codex:proj)" == "uds:$S1" && "$(hook_to claude:proj)" == "uds:$FAKE_SOCK" ]] \
  || fail "the hook does not pick the pane by its agent: $(hook_send codex:proj) $(hook_send claude:proj)"
[[ "$(hook_to "$N1")" == "uds:$S1" && "$(hook_to "$N2")" == "uds:$FAKE_SOCK" ]] || fail "the hook does not send each full name to its pane"
mcp_send "$TH1" claude:proj PEERBUS-MCP-KIND >/dev/null || fail "the MCP tool refused claude:proj"
got PEERBUS-MCP-KIND || fail "an MCP send to claude:proj did not reach the Claude session in pane $P2"
mcp_send "$TH1" "$N2" PEERBUS-MCP-FULL-TWO >/dev/null || fail "the MCP tool refused $N2"
got PEERBUS-MCP-FULL-TWO || fail "an MCP send to $N2 did not reach the Claude session in pane $P2"
grep -qF "from-name=\\\"$N1\\\"" "$SB/fake-inbox.log" || fail "the message does not carry the sender's display name $N1: $(tail -c 600 "$SB/fake-inbox.log")"
ok "a claude and a codex in one directory: proj is refused with both, claude:proj and codex:proj and the full names each reach the right one"

LIST="$(python3 - "$SB/bin/marspot-peer" "$TH1" <<'PY'
import json, subprocess, sys
req = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "ListAgents", "arguments": {}, "_meta": {"threadId": sys.argv[2]}}}
out = subprocess.run([sys.argv[1], "mcp"], input=json.dumps(req) + "\n", capture_output=True, text=True).stdout
print(json.loads(out.splitlines()[0])["result"]["content"][0]["text"])
PY
)"
grep -qF "This session is $N1" <<<"$LIST" && grep -qE "^  $N2  ·  claude  ·  idle" <<<"$LIST" || fail "the MCP listing does not show $N2 as claude: $LIST"
SID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["sessionId"])' "$REGISTRY/$FAKE_PID.json")"
CTX="$(printf '{"hook_event_name":"PostToolUse","tool_name":"ListAgents","session_id":"%s","tool_input":{}}' "$SID" \
  | "$SB/bin/marspot-peer" claude-hook)"
grep -qF "$N1  codex  " <<<"$CTX" && grep -qF "$N2  claude  idle → this session" <<<"$CTX" && grep -qF 'kind:name' <<<"$CTX" \
  || fail "the ListAgents hook did not list the panes with their agents and the recipient forms: $CTX"
ok "both listings give each pane's name and agent"

# ── the pane changes directory: one of each name again ────────────
stop_fake
mkdir -p "$SB/other"
send_once "$P2" "cd $SB/other"
start_fake
E2="$(wait_entry "$FAKE_SOCK" '^other$' 25)" || fail "pane $P2 is not shown as other after changing directory: $(pane_entry "$FAKE_SOCK")"
[[ "other#$(cut -f2 <<<"$E2")" == "other#${N2#proj#}" ]] || fail "pane $P2 changed its code when it changed directory: was $N2, is $E2"
NEW2="other#${N2#proj#}"
wait_name "$P1" '^proj$' 25 >/dev/null || fail "pane $P1 does not show the plain name once it is the only proj: $(record_of_pane "$P1")"
ok "pane $P2 is other with its code unchanged, and pane $P1 shows proj again"
for to in proj "$N1" codex:proj; do
  [[ "$(hook_to "$to")" == "uds:$S1" ]] || fail "the hook does not send $to to the only proj: $(hook_send "$to")"
done
for to in other "$NEW2" claude:other; do
  mcp_send "$TH1" "$to" "PEERBUS-LONE-$to" >/dev/null || fail "the MCP tool refused $to"
  got "PEERBUS-LONE-$to" || fail "an MCP send to $to did not reach pane $P2"
done
ok "a pane alone under its name answers to the plain name, the full name and kind:name on both paths"
out="$(mcp_send "$TH1" "$N2" PEERBUS-MOVED)" || fail "the MCP tool refused the full name $N2 the pane had before it moved: $out"
got PEERBUS-MOVED || fail "an MCP send to $N2 did not follow the pane to its new directory"
grep -qF "$N2 is now called $NEW2" <<<"$out" || fail "the MCP answer does not say what $N2 is called now: $out"
[[ "$(hook_to "$N2")" == "uds:$FAKE_SOCK" ]] || fail "the hook does not send $N2 to the pane that moved: $(hook_send "$N2")"
grep -qF "$N2 is now called $NEW2" <<<"$(hook_send "$N2")" || fail "the hook does not say what $N2 is called now: $(hook_send "$N2")"
ok "the full name $N2 still reaches the pane as $NEW2, and the answer says so"

# ── the session is gone: its names are unknown ────────────────────
stop_fake
for (( i = 0; i < 50; i++ )); do [[ -z "$(pane_entry "$FAKE_SOCK")" ]] && break; sleep 0.5; done
for to in "$N2" "$NEW2"; do
  out="$(mcp_send "$TH1" "$to" PEERBUS-GONE)" && fail "the MCP tool sent to $to, whose session is gone: $out"
  grep -qF "no pane is named \"$to\"" <<<"$out" || fail "the MCP refusal of $to is not that no pane has the name: $out"
  grep -qF "no pane is named \"$to\"" <<<"$(hook_refusal "$to")" || fail "the hook did not refuse $to as unknown: $(hook_send "$to")"
done
[[ "$(hook_to "$N1")" == "uds:$S1" ]] || fail "the remaining pane's full name $N1 stopped working"
ok "the names of the pane whose session ended are unknown on both paths, and $N1 still reaches pane $P1"
FAKE_PID=""

# ── 3. marspot killed, then started again ────────────────────────
#
# Killed, not quit: a quit stops the plugin, which removes its records
# (the live test covers that), but a killed or crashed shell cannot.
# Its records are left naming dead inboxes until the next start clears
# them -- which is what is checked here.
LEFT="$(records | cut -f4)"
dev_kill_shell_core
[[ -n "$(records)" ]] || fail "the records were gone before the restart; this step tests nothing"
launch
for (( i = 0; i < 60; i++ )); do [[ -z "$(records)" ]] && break; sleep 0.5; done
[[ -z "$(records)" ]] || fail "records left from the killed run: $(records)"
for s in $LEFT; do [[ -e "$s" ]] && fail "inbox socket left from the killed run: $s"; done
ok "the next start cleared the records and sockets a killed run left"
dev_kill_shell_core

echo "PASS — Claude-side messages reach the right Codex pane and come back from it"

#!/usr/bin/env bash
#
# Kimi Code in a pane, end to end: a sandbox marspot, the real kimi
# binary, the real registry, and a model stand-in on 127.0.0.1.
#
# What it proves:
#   - configuration: what marspot writes into a kimi home (the MCP
#     server in mcp.json, the hooks in config.toml) is accepted by
#     `kimi doctor`;
#   - recognition: a pane running kimi is seen, bound to the session
#     its lock file names, and badged with its model;
#   - state: a submitted prompt reads as busy, and as idle once the
#     reply has ended;
#   - receiving: a message sent to an idle kimi wakes it and reaches
#     the model, and one sent to a busy kimi reaches the model when the
#     running turn ends, without the pane being typed into;
#   - sending: kimi answers through the marspot MCP tools, and the
#     answer arrives at the sender's own socket;
#   - a refused account: when the service answers 429 with
#     `exceeded_current_quota_error`, kimi records the failed turn the
#     way marspot reads it, and the pane is moved to the second kimi
#     account, where the conversation is handed over and goes on.
#
# No account and no model: kimi talks to `_kimi_mock_model.py` through
# the KIMI_MODEL_* variables.  The other session is played by
# `_peerbus_claude_side.py`, speaking the Claude Code socket protocol.
#
# The kimi binary is fetched once with the vendor's installer into
# ~/.cache/marspot-e2e and copied from there; later runs need no network.
#
# Sandbox-only: MARSPOT_STATE_DIR *and* HOME are redirected, so the
# kimi home, the registry and the profiles are this test's own.  Opens
# windows: run it through bin/test-e2e-remote.sh, never on the machine
# someone is working at.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

KIMI_INSTALLER=https://code.kimi.com/kimi-code/install.sh
KIMI_CACHE="$HOME/.cache/marspot-e2e/kimi"

SB=/tmp/marspot-kimi
RUNLOG=$SB/run.log
REGISTRY="$SB/home/.claude"/sessions
export PEERBUS_REGISTRY="$REGISTRY"
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"
PEER_BIN="$ROOT/target/release/marspot-peer"
REQUESTS=$SB/model-requests.log

fail() {
  echo "FAIL: $*"
  echo "  --- records:"; cat "$REGISTRY"/*.json 2>/dev/null | sed 's/^/    /'
  echo "  --- last 40 shell lines mentioning kimi:"
  grep -i kimi "$APPLOG" 2>/dev/null | tail -40 | cut -c1-260 | sed 's/^/    /'
  echo "  --- pane ${P1:-?}:"; "$SHELL_BIN" --read "${P1:-0}" 2>&1 | tail -25 | sed 's/^/    /'
  echo "  --- kimi's own log:"; tail -15 "$SB/home/.kimi-code/logs/kimi-code.log" 2>/dev/null | cut -c1-240 | sed 's/^/    /'
  exit 1
}
ok() { echo "  ✓ $1"; }

cleanup() {
  dev_kill_shell_core
  # by recorded pid, never by name
  [[ -f "$SB/mock.pid" ]] && kill "$(cat "$SB/mock.pid")" 2>/dev/null
  rm -f /tmp/cc-socks/e2e-*.sock
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
count() { grep -cE "$1" "$APPLOG" 2>/dev/null || true; }

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" || ! -x "$SESSION_BIN" || ! -x "$PEER_BIN" ]]; then
  echo "==> building release binaries"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 \
    && cargo build --release -p marspot-peer 2>&1 | tail -3 )
fi

# ── kimi, as its installer leaves it ──────────────────────────────
if [[ ! -x "$KIMI_CACHE/.kimi-code/bin/kimi" ]]; then
  echo "==> installing kimi with the vendor's script"
  rm -rf "$KIMI_CACHE"; mkdir -p "$KIMI_CACHE"
  curl -fsSL --max-time 120 -o "$KIMI_CACHE/install.sh" "$KIMI_INSTALLER" || { echo "download failed"; exit 1; }
  env -i HOME="$KIMI_CACHE" PATH=/usr/bin:/bin KIMI_NO_MODIFY_PATH=1 /bin/bash "$KIMI_CACHE/install.sh" > "$KIMI_CACHE/install.log" 2>&1 \
    || { echo "the installer failed"; tail -20 "$KIMI_CACHE/install.log"; exit 1; }
  [[ -x "$KIMI_CACHE/.kimi-code/bin/kimi" ]] || { echo "the installer left no kimi"; exit 1; }
fi

# ── this test's own HOME: a default kimi home and a second account ─
rm -rf "$SB"; mkdir -p "$SB/proj" "$REGISTRY" "$SB/home/.kimi-code/bin" "$SB/home/.kimi-code-profile-2"
cp "$KIMI_CACHE/.kimi-code/bin/kimi" "$SB/home/.kimi-code/bin/kimi"

python3 "$ROOT/bin/_kimi_mock_model.py" "$SB/port" "$REQUESTS" > "$SB/mock.log" 2>&1 &
echo $! > "$SB/mock.pid"
for (( i = 0; i < 50; i++ )); do [[ -s "$SB/port" ]] && break; sleep 0.1; done
[[ -s "$SB/port" ]] || { echo "the model stand-in did not start"; cat "$SB/mock.log"; exit 1; }

export HOME="$SB/home"
export KIMI_MODEL_NAME=mock-model KIMI_MODEL_API_KEY=e2e
export KIMI_MODEL_BASE_URL="http://127.0.0.1:$(cat "$SB/port")/v1"
export KIMI_CODE_NO_AUTO_UPDATE=1 KIMI_DISABLE_TELEMETRY=1 KIMI_CODE_TRUST_WORKSPACE=1
KIMI="$HOME/.kimi-code/bin/kimi"
echo "==> kimi $("$KIMI" --version 2>&1 | head -1)"

echo "==> launching"
dev_kill_shell_core
rm -rf "$MARSPOT_STATE_DIR"; mkdir -p "$MARSPOT_STATE_DIR/logs"
MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  nohup "$SHELL_BIN" >>"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
disown

wait_for 'L3_SPAWNED' 90 || fail "no pane session was ever spawned"
P1=""
for (( i = 0; i < 150; i++ )); do
  P1="$("$SHELL_BIN" --panes 2>/dev/null | awk 'NR>1 {print $1}' | sort -n | head -1)"
  [[ -n "$P1" ]] && break
  sleep 0.4
done
[[ -n "$P1" ]] || fail "no pane came up"

send_once() { # text: retried only while refused, which queues nothing
  local i
  for (( i = 0; i < 40; i++ )); do
    "$SHELL_BIN" --send "$P1" "$1" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  fail "--send was refused 40 times: $1"
}
on_screen() { "$SHELL_BIN" --read "$P1" -n 200 2>/dev/null | grep -cF "$1"; }
wait_screen() { # text, seconds
  local i
  for (( i = 0; i < $2 * 2; i++ )); do (( $(on_screen "$1") > 0 )) && return 0; sleep 0.5; done
  return 1
}

# ── 1. configuration ──────────────────────────────────────────────
for (( i = 0; i < 120; i++ )); do
  grep -q 'kimi-hook' "$HOME/.kimi-code/config.toml" 2>/dev/null && grep -q '"marspot"' "$HOME/.kimi-code/mcp.json" 2>/dev/null && break
  sleep 0.5
done
grep -q 'kimi-hook' "$HOME/.kimi-code/config.toml" 2>/dev/null || fail "marspot wrote no hooks into the kimi home"
grep -q 'kimi-hook' "$HOME/.kimi-code-profile-2/config.toml" 2>/dev/null || fail "the second kimi home got no hooks"
[[ $(grep -c '^\[\[hooks\]\]' "$HOME/.kimi-code/config.toml") == 2 ]] || fail "expected two hook tables: $(cat "$HOME/.kimi-code/config.toml")"
"$KIMI" doctor > "$SB/doctor.log" 2>&1 || fail "kimi doctor rejects the configuration marspot wrote: $(tail -5 "$SB/doctor.log")"
ok "kimi doctor accepts the mcp.json and config.toml marspot wrote"

# ── 2. recognition ────────────────────────────────────────────────
MARK="$SB/ready"
for (( i = 0; i < 10; i++ )); do
  "$SHELL_BIN" --send "$P1" "echo reading > $MARK" >/dev/null 2>&1
  for (( j = 0; j < 15; j++ )); do [[ -f "$MARK" ]] && break 2; sleep 0.2; done
done
[[ -f "$MARK" ]] || fail "the pane's shell never ran anything typed into it"
send_once "cd $SB/proj && kimi --yolo"
wait_for "kimi.*sid=$P1 pid=[0-9]+ home=$HOME/.kimi-code session=none" 60 || fail "the kimi pane was not recognised"
wait_for "sid=$P1 badge=\"kimi\"" 10 || fail "no bare badge for a kimi whose arguments are gone"
ok "the pane is recognised as kimi before it has a session"
wait_screen "│ >" 60 || fail "kimi's input box never appeared"

# ── 3. state ──────────────────────────────────────────────────────
send_once "SLOW say hi"
wait_for "sid=$P1 pid=[0-9]+ .*session=session_[0-9a-f-]+ .*status=busy" 60 || fail "the submitted prompt never read as busy"
ok "the pane is bound to its session and reads as busy"
wait_for "sid=$P1 pid=[0-9]+ .*session=session_.*status=idle" 60 || fail "the ended turn never read as idle"
wait_for "sid=$P1 badge=\"mock-model·on\"" 20 || fail "the badge does not show the model and effort the request was sent with"
ok "the turn's end reads as idle, and the badge shows mock-model·on"

record() { # field → its value in the kimi pane's record
  python3 - "$REGISTRY" "$1" <<'PY'
import glob, json, os, sys
for p in sorted(glob.glob(os.path.join(sys.argv[1], "*.json"))):
    r = json.load(open(p))
    if r.get("marspotAgent") == "kimi":
        print(r.get(sys.argv[2], ""))
PY
}
for (( i = 0; i < 60; i++ )); do [[ "$(record status)" == idle ]] && break; sleep 0.5; done
[[ "$(record status)" == idle ]] || fail "the kimi record never said idle (status: $(record status))"
[[ "$(record marspotPermissionClass)" == bypass ]] || fail "a kimi started with --yolo should be the bypass class"
SOCK="$(record messagingSocketPath)"
[[ -S "$SOCK" ]] || fail "the kimi record names no inbox socket"
ok "the session has a record with a status and an inbox"
hook_to() { # recipient → the address the Claude Code SendMessage hook turns it into
  printf '{"hook_event_name":"PreToolUse","tool_name":"SendMessage","session_id":"x","tool_input":{"to":"%s","message":"hi"}}' "$1" \
    | "$PEER_BIN" claude-hook \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["hookSpecificOutput"]["updatedInput"]["to"])' 2>/dev/null
}
for (( i = 0; i < 50; i++ )); do [[ "$(hook_to kimi:proj)" == "uds:$SOCK" ]] && break; sleep 0.5; done
[[ "$(hook_to kimi:proj)" == "uds:$SOCK" ]] || fail "kimi:proj is not the kimi pane's inbox: $(hook_to kimi:proj)"
[[ -z "$(hook_to codex:proj)" ]] || fail "codex:proj reached the kimi pane"
ok "the pane is addressed as kimi:proj, and not as another agent's"

# ── 4. a message to an idle kimi wakes it; kimi answers by MCP ────
woken=$(count 'kimi.*wake.*mail is waiting')
out="$(python3 "$ROOT/bin/_peerbus_claude_side.py" "$SOCK" 120 KIMI-E2E-REPLY TAG-IDLE notify)" \
  || fail "no reply or idle notice from the idle kimi: $out"
grep -qF "\"from\": \"uds:$SOCK\"" <<<"$out" || fail "the reply did not come from the inbox written to: $out"
(( $(count 'kimi.*wake.*mail is waiting') == woken + 1 )) || fail "the idle pane was not woken exactly once"
grep -qF 'TAG-IDLE' "$REQUESTS" || fail "the message never reached the model"
grep -qF 'Read the messages that just arrived from other agent sessions.' "$REQUESTS" || fail "the wake line was not what was submitted"
ok "an idle kimi is woken once, the model sees the message, and its reply and idle notice come back"

# ── 5. a message to a busy kimi waits for the turn's end ──────────
for (( i = 0; i < 60; i++ )); do [[ "$(record status)" == idle ]] && break; sleep 0.5; done
woken=$(count 'kimi.*wake.*mail is waiting')
busy=$(count "sid=$P1 pid=[0-9]+ .*status=busy")
send_once "SLOW count to ten"
for (( i = 0; i < 150; i++ )); do (( $(count "sid=$P1 pid=[0-9]+ .*status=busy") > busy )) && break; sleep 0.2; done
(( $(count "sid=$P1 pid=[0-9]+ .*status=busy") > busy )) || fail "the second prompt never read as busy"
out="$(python3 "$ROOT/bin/_peerbus_claude_side.py" "$SOCK" 120 KIMI-E2E-REPLY TAG-BUSY)" \
  || fail "no reply from the busy kimi: $out"
grep -qF 'TAG-BUSY' "$REQUESTS" || fail "the message sent mid-turn never reached the model"
(( $(count 'kimi.*wake.*mail is waiting') == woken )) || fail "a busy pane was typed into"
ok "a busy kimi gets the message when its turn ends, without being typed into"

# ── 6. the service refuses the account ────────────────────────────
for (( i = 0; i < 60; i++ )); do [[ "$(record status)" == idle ]] && break; sleep 0.5; done
send_once "QUOTA-EXHAUSTED please"
WIRE="$(find "$HOME/.kimi-code/sessions" -name wire.jsonl -path '*/agents/main/*' | head -1)"
for (( i = 0; i < 120; i++ )); do grep -q '"reason":"failed"' "$WIRE" 2>/dev/null && break; sleep 0.5; done
grep '"type":"turn.ended"' "$WIRE" | grep '"reason":"failed"' > "$SB/failed-turn.jsonl" || fail "kimi recorded no failed turn"
echo "    $(cut -c1-400 "$SB/failed-turn.jsonl")"
grep -q '"name":"APIProviderQuotaExhaustedError"' "$SB/failed-turn.jsonl" || fail "the failed turn does not carry the error name marspot reads"
wait_for "sid=$P1 pid=[0-9]+ .*failure=APIProviderQuotaExhaustedError@turn" 30 || fail "marspot did not read the refusal out of the record"
ok "a 429 exceeded_current_quota_error is recorded as APIProviderQuotaExhaustedError, and marspot reads it"

# ── 7. the pane leaves the refused account ────────────────────────
wait_for "kimi.*quota.move.*sid=$P1: .*moving to profile 2" 20 || fail "the pane was not moved off the refused account"
wait_for "sid=$P1 pid=[0-9]+ home=$HOME/.kimi-code-profile-2 session=none" 90 || fail "no kimi came up under the second account"
wait_for "sid=$P1 pid=[0-9]+ home=$HOME/.kimi-code-profile-2 session=session_" 90 || fail "the handed-over conversation started no session under the second account"
for (( i = 0; i < 60; i++ )); do grep -qF '[marspot handoff]' "$REQUESTS" && break; sleep 0.5; done
grep -qF '[marspot handoff]' "$REQUESTS" || fail "the handoff never reached the model under the second account"
[[ $(count "kimi.*quota.move.*sid=$P1") == 1 ]] || fail "the pane was moved more than once for one refusal"
ok "the pane moves to the second kimi account once, and the conversation is handed over there"

echo "PASS"

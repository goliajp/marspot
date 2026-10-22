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
#   - names: every session is named after its pane (`proj`, then
#     `proj#1` and `proj#2` once two panes share the directory), Codex
#     sessions and a Claude-registered process alike, and Codex sends
#     to another Codex by that name;
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

records() { # name<TAB>codex pid<TAB>thread<TAB>socket<TAB>former names, one per bound Codex pane
  python3 - "$REGISTRY" <<'PY'
import glob, json, os, sys
for p in sorted(glob.glob(os.path.join(sys.argv[1], "*.json"))):
    r = json.load(open(p))
    if r.get("marspotOwner"):
        former = ",".join(f.get("name", "") for f in r.get("formerNames") or [])
        print("\t".join(str(r.get(k, "")) for k in ("name", "marspotCodexPid", "sessionId", "messagingSocketPath")) + "\t" + former)
PY
}
name_of_pid() { # pid → name<TAB>former names<TAB>status, from any record
  python3 - "$REGISTRY/$1.json" <<'PY' 2>/dev/null
import json, sys
r = json.load(open(sys.argv[1]))
print("%s\t%s\t%s" % (r.get("name"), ",".join(f.get("name", "") for f in r.get("formerNames") or []), r.get("status")))
PY
}
wait_name() { # pane, name, seconds → the pane's record once it carries that name
  local i line
  for (( i = 0; i < $3 * 2; i++ )); do
    line="$(record_of_pane "$1")" && [[ "$(cut -f1 <<<"$line")" == "$2" ]] && { echo "$line"; return 0; }
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

# ── names follow the panes, and Codex sends by them ───────────────
R1C="$(wait_name "$P1" "proj#1" 25)" || fail "pane $P1's session was not renamed proj#1: $(record_of_pane "$P1")"
wait_name "$P2" "proj#2" 25 >/dev/null || fail "pane $P2's session was not named proj#2: $(record_of_pane "$P2")"
grep -qF proj <<<"$(cut -f5 <<<"$R1C")" || fail "the old name is not among pane $P1's former names: $R1C"
ok "two panes in one directory: proj#1 and proj#2, and proj#1 still answers to proj"
python3 "$ROOT/bin/_peerbus_claude_side.py" "$(cut -f4 <<<"$R2")" 30 "" "Send proj#1 the text PEERBUS-C2C-ONE" >/dev/null \
  || fail "the forwarding request did not reach pane $P2"
for (( i = 0; i < 60; i++ )); do (( $(seen "$P1" PEERBUS-C2C-ONE) > 0 )) && break; sleep 0.5; done
(( $(seen "$P1" PEERBUS-C2C-ONE) > 0 )) || fail "Codex in pane $P2 sent to proj#1, and nothing showed on pane $P1"
ok "Codex in pane $P2 messaged the Codex in pane $P1 by its pane name"

# ── a Claude session is renamed after its pane ────────────────────
send_once "$P2" "/quit"
shell_ready "$P2"
send_once "$P2" "python3 $ROOT/bin/_peerbus_fake_claude.py $REGISTRY 120"
for (( i = 0; i < 20; i++ )); do
  FAKE_PID="$(grep -lE '"entrypoint": *"cli"' "$REGISTRY"/*.json 2>/dev/null | head -1 | xargs -n1 basename 2>/dev/null | sed 's/\.json$//')"
  [[ -n "$FAKE_PID" ]] && break
  sleep 0.5
done
[[ -n "$FAKE_PID" ]] || fail "the stand-in Claude session never registered"
for (( i = 0; i < 50; i++ )); do
  [[ "$(name_of_pid "$FAKE_PID" | cut -f1)" == "proj#2" ]] && break
  sleep 0.5
done
FAKE="$(name_of_pid "$FAKE_PID")"
[[ "$(cut -f1 <<<"$FAKE")" == "proj#2" ]] || fail "the Claude session in pane $P2 was not renamed proj#2: $FAKE"
[[ "$(cut -f2 <<<"$FAKE")" == fake-claude-6a ]] || fail "its own name is not kept among its former names: $FAKE"
[[ "$(cut -f3 <<<"$FAKE")" == idle ]] || fail "renaming changed another field of its record: $FAKE"
ok "a Claude session in pane $P2 is renamed proj#2, keeping its own name as a former one"

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

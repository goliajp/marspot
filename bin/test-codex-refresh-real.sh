#!/usr/bin/env bash
#
# The refresh with a real codex: two released CLIs side by side in the
# packaged layout, the real app-server, a turn run against a model
# stand-in, and peerbus binding the pane to its thread from what the
# daemon reports.  The pane started on the older release ends up on the
# installed one, on the same thread, with the conversation still on it.
#
# test-codex-refresh.sh pins marspot's half with a stand-in; this one is
# for the half that belongs to codex: that a thread survives its TUI
# being stopped, that `codex resume <id>` from a newer CLI reattaches
# it, and that the daemon's idle status is what peerbus publishes.
#
# No account and no model: codex talks to `_peerbus_mock_model.py`.
# Sandbox-only: MARSPOT_STATE_DIR and HOME are redirected.  Opens
# windows: run it through bin/test-e2e-remote.sh.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

OLD=0.163.0
OLD_SHA256=b0344fc2e41f15e0071147e5869008047b8ff4bf2c4a8897975d3bae3f103b34
NEW=0.164.0
NEW_SHA256=dd99e9092270691a67bdde7be04ac283dea9da8e86cbde356c2cc851a79205fa
TARGET=aarch64-apple-darwin

# short: codex puts unix sockets under $HOME/.codex, and a socket path
# has to fit in 104 bytes
SB=/tmp/marspot-cxr
RUNLOG=$SB/run.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"
PEER_BIN="$ROOT/target/release/marspot-peer"

fail() {
  echo "FAIL: $*"
  echo "  --- shell lines about codex, peerbus and the refresh:"
  grep -E 'refresh|peerbus\.(bind|unbind|unbound)|codex\.update' "$APPLOG" 2>/dev/null | tail -30 | cut -c1-240 | sed 's/^/    /'
  for sid in ${SIDS:-}; do
    echo "  --- pane $sid:"; "$SHELL_BIN" --read "$sid" 2>&1 | tail -15 | sed 's/^/    /'
  done
  exit 1
}
ok() { echo "  ✓ $1"; }

# pid of a process this test started, from a codex pid file, only when
# its command line is under this test's home
codex_pid_from() {
  local p
  p="$(sed -n 's/^{"pid":\([0-9]*\).*/\1/p' "$1" 2>/dev/null)"
  [[ -n "$p" ]] && ps -ww -o command= -p "$p" 2>/dev/null | grep -qF "$SB/" && echo "$p"
}

cleanup() {
  dev_kill_shell_core
  # by recorded pids, never by name: `codex` and `python3` are running
  # for other people on this machine
  local f p
  for f in "$SB/home/.codex/app-server-daemon/daemon.pid" "$SB/home/.codex/app-server-daemon/daemon-updater.pid"; do
    p="$(codex_pid_from "$f")"
    [[ -n "$p" ]] && kill "$p" 2>/dev/null
  done
  [[ -f "$SB/mock.pid" ]] && kill "$(cat "$SB/mock.pid")" 2>/dev/null
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

# ── two codex releases, pinned ────────────────────────────────────
fetch() { # version, sha256 → the unpacked package directory
  local v=$1 sha=$2 dir="$HOME/.cache/marspot-e2e/codex-$1" got
  if [[ ! -x "$dir/bin/codex" ]]; then
    rm -rf "$dir"; mkdir -p "$dir"
    curl -fsSL -o "$dir/codex.tar.gz" \
      "https://github.com/openai/codex/releases/download/rust-v$v/codex-package-$TARGET.tar.gz" \
      || { echo "download of codex $v failed" >&2; return 1; }
    got="$(shasum -a 256 "$dir/codex.tar.gz" | cut -d' ' -f1)"
    [[ "$got" == "$sha" ]] || { echo "codex $v archive digest $got, expected $sha" >&2; return 1; }
    tar -xzf "$dir/codex.tar.gz" -C "$dir" && rm "$dir/codex.tar.gz"
  fi
  echo "$dir"
}
echo "==> codex $OLD and $NEW"
OLD_PKG="$(fetch "$OLD" "$OLD_SHA256")" || exit 1
NEW_PKG="$(fetch "$NEW" "$NEW_SHA256")" || exit 1

# ── this test's own HOME, with both installed, the newer selected ─
rm -rf "$SB"; mkdir -p "$SB/bin" "$SB/proj1" "$SB/proj2"
cp "$PEER_BIN" "$SB/bin/marspot-peer"
python3 "$ROOT/bin/_peerbus_mock_model.py" "$SB/port" "$SB/model-requests.log" > "$SB/mock.log" 2>&1 &
echo $! > "$SB/mock.pid"
for (( i = 0; i < 50; i++ )); do [[ -s "$SB/port" ]] && break; sleep 0.1; done
[[ -s "$SB/port" ]] || { echo "the model stand-in did not start"; cat "$SB/mock.log"; exit 1; }

export HOME="$SB/home"
PKG="$HOME/.codex/packages/standalone"
mkdir -p "$PKG/releases"
cp -Rc "$OLD_PKG" "$PKG/releases/$OLD-$TARGET"
cp -Rc "$NEW_PKG" "$PKG/releases/$NEW-$TARGET"
ln -sfn "$PKG/releases/$NEW-$TARGET" "$PKG/current"
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

[projects."$SB/proj1"]
trust_level = "trusted"

[projects."$SB/proj2"]
trust_level = "trusted"

[tui]
screen_reader_detection_done = true
terminal_title = ["activity", "thread-name", "project-name", "thread-id"]
EOF
# what the installer leaves on PATH: the selected release's own bin/,
# through the `current` link
export PATH="$PKG/current/bin:$SB/bin:$PATH"

echo "==> launching with a second window"
dev_kill_shell_core
rm -rf "$MARSPOT_STATE_DIR"; mkdir -p "$MARSPOT_STATE_DIR/logs"
MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" MARSPOT_DEV_EXTRA_WINDOWS=1 \
  nohup "$SHELL_BIN" >>"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
disown

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
seen() { "$SHELL_BIN" --read "$1" -n 200 2>/dev/null | grep -cF "$2"; }
# the release of the codex running in a pane: its executable, among the
# shell's descendants.  Not a direct child (the refresh starts it from a
# subshell), and not by process name (codex renames its process).
shell_of() { sed -n 's/^shell_child_pid = //p' "$(find "$MARSPOT_STATE_DIR" -path "*/sessions/$1/entry.toml" | head -1)"; }
descendants() { local c; for c in $(pgrep -P "$1"); do echo "$c"; descendants "$c"; done; }
release_of() {
  local p exe
  for p in $(descendants "$(shell_of "$1")"); do
    exe="$(lsof -a -p "$p" -d txt -Fn 2>/dev/null | sed -n 's/^n//p' | head -1)"
    [[ "$exe" == */bin/codex ]] && { grep -oE "releases/[0-9.]+-" <<<"$exe" | head -1; return; }
  done
}

for sid in "$P1" "$P2"; do shell_ready "$sid"; done
n=0
for sid in "$P1" "$P2"; do
  n=$((n + 1))
  send_once "$sid" "cd $SB/proj$n && $PKG/releases/$OLD-$TARGET/bin/codex"
done
for sid in "$P1" "$P2"; do
  wait_for "peerbus.bind.*sid=$sid[[:space:]]" 60 || fail "pane $sid's codex never bound to a thread"
done
ok "both panes run codex $OLD, each bound to its thread"

# a turn, so the thread has a status the daemon reports
for sid in "$P1" "$P2"; do
  send_once "$sid" "REFRESH-MARK-$sid say something"
done
for sid in "$P1" "$P2"; do
  for (( i = 0; i < 60; i++ )); do (( $(seen "$sid" "REFRESH-MARK-$sid") > 0 )) && break; sleep 0.5; done
  (( $(seen "$sid" "REFRESH-MARK-$sid") > 0 )) || fail "pane $sid's prompt never showed"
done
ok "each pane ran a turn"

echo "==> waiting for the refresh (needs 60 s of quiet)"
wait_for 'refresh\.start' 200 || fail "no pane was refreshed within 200 s"
LINE="$(grep -E 'refresh\.start' "$APPLOG" | head -1)"
REFRESHED="$(grep -oE 'pane [0-9]+' <<<"$LINE" | head -1 | grep -oE '[0-9]+')"
THREAD="$(grep -oE 'thread [0-9a-f-]{36}' <<<"$LINE" | cut -d' ' -f2)"
grep -q "codex $OLD → $NEW" <<<"$LINE" || fail "the refresh was not $OLD → $NEW: $LINE"
BOUND="$(grep -E "peerbus.bind.*sid=$REFRESHED[[:space:]]" "$APPLOG" | head -1 | grep -oE 'thread=[0-9a-f-]{36}' | cut -d= -f2)"
[[ -n "$THREAD" && "$THREAD" == "$BOUND" ]] || fail "refreshed thread $THREAD is not the one pane $REFRESHED was bound to ($BOUND)"
ok "pane $REFRESHED refreshed $OLD → $NEW on its thread $THREAD"

for (( i = 0; i < 60; i++ )); do [[ "$(release_of "$REFRESHED")" == "releases/$NEW-" ]] && break; sleep 0.5; done
[[ "$(release_of "$REFRESHED")" == "releases/$NEW-" ]] \
  || fail "pane $REFRESHED runs $(release_of "$REFRESHED") after the refresh"
ok "pane $REFRESHED's codex is now the $NEW executable"

binds() { grep -cE "peerbus.bind.*sid=$REFRESHED[[:space:]].*thread=$THREAD" "$APPLOG"; }
for (( i = 0; i < 60; i++ )); do (( $(binds) >= 2 )) && break; sleep 0.5; done
(( $(binds) >= 2 )) \
  || fail "the new codex in pane $REFRESHED did not come back on thread $THREAD"
ok "and peerbus bound it to the same thread again"
for (( i = 0; i < 30; i++ )); do (( $(seen "$REFRESHED" "REFRESH-MARK-$REFRESHED") > 0 )) && break; sleep 0.5; done
(( $(seen "$REFRESHED" "REFRESH-MARK-$REFRESHED") > 0 )) \
  || fail "the conversation is not on pane $REFRESHED after the refresh"
ok "the conversation is still on the pane"

OTHER=$P1; [[ "$REFRESHED" == "$P1" ]] && OTHER=$P2
[[ "$(release_of "$OTHER")" == "releases/$OLD-" ]] || fail "pane $OTHER, which has the cursor, was touched"
grep -qE "refresh.held.*pane $OTHER:.*held: user_here" "$APPLOG" \
  || fail "pane $OTHER was not held back as the pane with the cursor"
ok "pane $OTHER, which has the cursor, still runs $OLD"

echo "PASS — a real codex pane on $OLD is moved to the installed $NEW on the same thread"

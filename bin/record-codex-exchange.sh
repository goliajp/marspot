#!/usr/bin/env bash
#
# Records the exchange bin/codex-standin draws from: one real Codex TUI,
# pinned, started in a throwaway HOME with no account, one prompt
# answered by the model stand-in (_peerbus_mock_model.py), then /quit.
# Writes bin/codex-standin/exchange.json with the scratch paths, the
# thread id and the time replaced by placeholders.
#
# Starts a real agent CLI, so it refuses on a runner
# (bin/_real-agent-guard.sh).  Everything it starts is under a hard
# timeout and ended by the pid it recorded.
#
#   bin/record-codex-exchange.sh [scratch-dir]
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_real-agent-guard.sh"
refuse_real_agent_on_runner "the real Codex CLI"

CODEX_VERSION=0.171.0
CODEX_SHA256=58c88032ec446e72922d911d8f03773102da20c16a6059cf21f6744bce57f210
CODEX_URL="https://github.com/openai/codex/releases/download/rust-v$CODEX_VERSION/codex-package-aarch64-apple-darwin.tar.gz"
OUT="$ROOT/bin/codex-standin/exchange.json"

# short: codex puts unix sockets under its home, and a socket path has
# to fit in 104 bytes
SB="${1:-$(mktemp -d /tmp/cxrec.XXXXXX)}"
SB="$(mkdir -p "$SB" && cd "$SB" && pwd -P)"
mkdir -p "$SB/pkg" "$SB/home/.codex" "$SB/proj"

cleanup() {
  local f p
  for f in "$SB/home/.codex/app-server-daemon/daemon.pid" "$SB/home/.codex/app-server-daemon/daemon-updater.pid"; do
    p="$(sed -n 's/^{"pid":\([0-9]*\).*/\1/p' "$f" 2>/dev/null)"
    [[ -n "$p" ]] && ps -ww -o command= -p "$p" 2>/dev/null | grep -qF "$SB/" && kill "$p"
  done
  [[ -f "$SB/mock.pid" ]] && kill "$(cat "$SB/mock.pid")" 2>/dev/null
}
trap cleanup EXIT

if [[ ! -f "$SB/pkg/codex-package.json" ]]; then
  /usr/bin/timeout 300 curl -fsSL -o "$SB/codex.tar.gz" "$CODEX_URL" || { echo "download failed"; exit 1; }
  got="$(shasum -a 256 "$SB/codex.tar.gz" | cut -d' ' -f1)"
  [[ "$got" == "$CODEX_SHA256" ]] || { echo "codex archive digest $got, expected $CODEX_SHA256"; exit 1; }
  tar -xzf "$SB/codex.tar.gz" -C "$SB/pkg" && rm "$SB/codex.tar.gz"
fi
MANIFEST="$(find "$SB/pkg" -maxdepth 3 -name codex-package.json | head -1)"
CODEX="$(dirname "$MANIFEST")/$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["entrypoint"])' "$MANIFEST")"
[[ -x "$CODEX" ]] || { echo "no codex entrypoint in the package"; exit 1; }

python3 "$ROOT/bin/_peerbus_mock_model.py" "$SB/port" "$SB/model-requests.log" >"$SB/mock.log" 2>&1 &
echo $! >"$SB/mock.pid"
for (( i = 0; i < 50; i++ )); do [[ -s "$SB/port" ]] && break; sleep 0.1; done
[[ -s "$SB/port" ]] || { echo "the model stand-in did not start"; exit 1; }

cat >"$SB/home/.codex/config.toml" <<EOF
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

[tui]
screen_reader_detection_done = true
terminal_title = ["activity", "thread-name", "project-name", "thread-id"]
EOF

export HOME="$SB/home"
export PATH="$(dirname "$CODEX"):$PATH"
/usr/bin/timeout 180 python3 -I "$ROOT/bin/codex-standin/record.py" "$CODEX" "$HOME/.codex" "$SB/proj" "$SB/raw.json" \
  || { echo "the recording did not finish"; exit 1; }
python3 -I "$ROOT/bin/codex-standin/sanitize.py" "$SB/raw.json" "$SB" "$CODEX_VERSION" "$OUT" || exit 1
echo "wrote $OUT (raw recording: $SB/raw.json)"

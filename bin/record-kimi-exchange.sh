#!/usr/bin/env bash
#
# Records the exchange bin/kimi-standin draws from: one real Kimi Code
# TUI, a copy of the kimi binary already installed on this machine,
# started in a throwaway HOME and KIMI_CODE_HOME with no account, three
# prompts answered by the model stand-in (_kimi_mock_model.py), then
# /exit.  The user's own kimi home is only read from (its binary).
# Writes bin/kimi-standin/exchange.json with the scratch paths, the
# session id, the port, the host and the user name replaced by
# placeholders.
#
# Starts a real agent CLI, so it refuses on a runner
# (bin/_real-agent-guard.sh).  Everything it starts is under a hard
# timeout and ended by the pid it recorded.
#
#   bin/record-kimi-exchange.sh [kimi-binary] [scratch-dir]
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_real-agent-guard.sh"
refuse_real_agent_on_runner "the real Kimi CLI"

SRC="${1:-$HOME/.kimi-code/bin/kimi}"
[[ -x "$SRC" ]] || { echo "no kimi at $SRC"; exit 1; }
OUT="$ROOT/bin/kimi-standin/exchange.json"
HERE="$ROOT/bin/kimi-standin"

SB="${2:-$(mktemp -d /tmp/kmrec.XXXXXX)}"
SB="$(mkdir -p "$SB" && cd "$SB" && pwd -P)"
rm -rf "${SB:?}/home" "${SB:?}/proj" "${SB:?}/probes" "${SB:?}/port" "${SB:?}/model-requests.log"
mkdir -p "$SB/home/.kimi-code/bin" "$SB/proj" "$SB/probes"
cp "$SRC" "$SB/home/.kimi-code/bin/kimi"
KIMI="$SB/home/.kimi-code/bin/kimi"

cleanup() {
  [[ -f "$SB/mock.pid" ]] && kill "$(cat "$SB/mock.pid")" 2>/dev/null
}
trap cleanup EXIT

python3 "$ROOT/bin/_kimi_mock_model.py" "$SB/port" "$SB/model-requests.log" >"$SB/mock.log" 2>&1 &
echo $! >"$SB/mock.pid"
for (( i = 0; i < 50; i++ )); do [[ -s "$SB/port" ]] && break; sleep 0.1; done
[[ -s "$SB/port" ]] || { echo "the model stand-in did not start"; exit 1; }

PY="$(command -v python3)"
cat >"$SB/home/.kimi-code/config.toml" <<EOF
[[hooks]]
event = "UserPromptSubmit"
command = "'$PY' '$HERE/probe.py' hook '$SB/probes/hook.log'"
timeout = 10

[[hooks]]
event = "Stop"
command = "'$PY' '$HERE/probe.py' hook '$SB/probes/hook.log'"
timeout = 10
EOF
cat >"$SB/home/.kimi-code/mcp.json" <<EOF
{"mcpServers": {"marspot": {"command": "$PY", "args": ["$HERE/probe.py", "mcp", "$SB/probes/mcp.log"]}}}
EOF

# a clean environment: nothing of this shell (tokens, keys, the user's
# paths) reaches kimi, its hooks or its MCP server, so none of it can
# end up in the recording
/usr/bin/timeout 120 /usr/bin/env -i PATH="$PATH" HOME="$SB/home" TERM=xterm-256color \
  KIMI_CODE_HOME="$SB/home/.kimi-code" KIMI_MODEL_NAME=mock-model KIMI_MODEL_API_KEY=e2e \
  KIMI_MODEL_BASE_URL="http://127.0.0.1:$(cat "$SB/port")/v1" \
  KIMI_CODE_NO_AUTO_UPDATE=1 KIMI_DISABLE_TELEMETRY=1 KIMI_CODE_TRUST_WORKSPACE=1 \
  "$PY" -I "$HERE/record.py" "$KIMI" "$SB/home/.kimi-code" "$SB/proj" "$SB/probes" "$SB/raw.json" \
  || { echo "the recording did not finish"; exit 1; }
python3 -I "$HERE/sanitize.py" "$SB/raw.json" "$SB" "$OUT" || exit 1
echo "wrote $OUT (raw recording: $SB/raw.json)"

#!/usr/bin/env bash
# bin/drivers/terminal.sh — automated Terminal.app launcher.
#
# Usage:
#   bin/drivers/terminal.sh run-windows <n> <cmd>
#     Open <n> Terminal.app windows, run <cmd> in each.  Windows
#     (not tabs) because Terminal.app's tab AppleScript is fragile
#     across macOS versions; window-per-worker measures the same
#     PTY-contention property and is reliable.
#
#   bin/drivers/terminal.sh run-single <cmd>
#     Same as run-windows 1 — convenience for cat-* / startup scenarios.
#
#   bin/drivers/terminal.sh quit

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

cmd_word=${1:-}
shift || true

case "$cmd_word" in
  run-single)
    user_cmd=${1:?cmd}; shift || true
    esc_cmd=${user_cmd//\"/\\\"}
    osascript <<APPLESCRIPT >/dev/null
tell application "Terminal"
  activate
  do script "$esc_cmd"
end tell
APPLESCRIPT
    ;;
  run-windows)
    n=${1:?n}; shift
    user_cmd=${1:?cmd}; shift || true
    esc_cmd=${user_cmd//\"/\\\"}
    osascript <<APPLESCRIPT >/dev/null
tell application "Terminal"
  activate
  set i to 1
  repeat $n times
    do script "$esc_cmd"
    set i to i + 1
  end repeat
end tell
APPLESCRIPT
    ;;
  quit)
    echo "terminal.sh: refusing to quit Terminal.app — that would kill the user's work." >&2
    exit 2
    ;;
  *)
    echo "terminal.sh: unknown sub-command: ${cmd_word:-(none)}" >&2
    exit 2
    ;;
esac

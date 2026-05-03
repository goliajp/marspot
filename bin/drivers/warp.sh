#!/usr/bin/env bash
# bin/drivers/warp.sh — Warp launcher.
#
# Warp has no real AppleScript API.  We can launch the app via `open`
# and either (a) use System-Events keystroke to type a command (very
# fragile — focus races, Warp's command-bar interception, autocompletion
# rewriting), or (b) ask the user to paste a known one-liner once.
#
# This driver does (a) for run-single (one window, one command) and
# falls back to **paste mode** for run-windows (N parallel commands) —
# scripting N keystroked commands into separate windows is far below
# the reliability bar we want for a recurring bench.
#
# Usage:
#   bin/drivers/warp.sh run-single <cmd>
#     Open a fresh Warp window, type <cmd>, hit return.
#
#   bin/drivers/warp.sh paste-block <n> <cmd>
#     Print a paste-ready block: open Warp, hit cmd-N <n-1> times,
#     paste the block in each window.  Caller polls the marker.
#
#   bin/drivers/warp.sh quit

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
    open -a Warp
    sleep 0.8  # focus settle
    # The keystroke approach is fragile but tolerable for run-single.
    # If this misfires, fall back to paste-block.
    osascript <<APPLESCRIPT >/dev/null
tell application "System Events"
  tell process "Warp"
    set frontmost to true
  end tell
  delay 0.2
  keystroke "$user_cmd"
  keystroke return
end tell
APPLESCRIPT
    ;;
  paste-block)
    n=${1:?n}; shift
    user_cmd=${1:?cmd}; shift || true
    cat <<EOF
----- Warp manual paste (n=$n) -----
1. Open Warp (cmd+space → Warp)
2. Hit cmd+N $((n-1)) times to spawn $n windows (or skip for n=1)
3. Click on each window, paste:
$user_cmd
4. Hit return
-------------------------------------
EOF
    ;;
  quit)
    echo "warp.sh: refusing to quit Warp — that would kill the user's work." >&2
    exit 2
    ;;
  *)
    echo "warp.sh: unknown sub-command: ${cmd_word:-(none)}" >&2
    exit 2
    ;;
esac

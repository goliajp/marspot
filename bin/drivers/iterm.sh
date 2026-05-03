#!/usr/bin/env bash
# bin/drivers/iterm.sh — automated iTerm2 launcher.
#
# Usage:
#   bin/drivers/iterm.sh run-tabs <n> <cmd>
#     Open a fresh iTerm2 window, then create <n>-1 additional tabs in
#     it.  Each tab runs <cmd>.  Returns when all `write text` calls
#     have been issued (i.e. ~immediately) — caller polls a marker
#     file to detect completion.
#
#   bin/drivers/iterm.sh run-windows <n> <cmd>
#     Open <n> separate iTerm2 windows in the same iTerm2 process.
#     For scenarios where tab-vs-window doesn't matter and tab API
#     instability is biting us, this is the simpler option.
#
#   bin/drivers/iterm.sh quit
#
# All iTerm2 dispatch goes through `osascript` — proper AppleScript
# API, no System-Events keystroke fragility.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

cmd_word=${1:-}
shift || true

case "$cmd_word" in
  run-tabs)
    n=${1:?n}; shift
    user_cmd=${1:?cmd}; shift || true
    # AppleScript escape: replace " with \"
    esc_cmd=${user_cmd//\"/\\\"}
    osascript <<APPLESCRIPT >/dev/null
tell application "iTerm"
  activate
  set theWindow to (create window with default profile)
  tell current session of theWindow to write text "$esc_cmd"
  set i to 1
  repeat ($n - 1) times
    tell theWindow to create tab with default profile
    tell current session of theWindow to write text "$esc_cmd"
    set i to i + 1
  end repeat
end tell
APPLESCRIPT
    ;;
  run-windows)
    n=${1:?n}; shift
    user_cmd=${1:?cmd}; shift || true
    esc_cmd=${user_cmd//\"/\\\"}
    osascript <<APPLESCRIPT >/dev/null
tell application "iTerm"
  activate
  set i to 1
  repeat $n times
    set theWindow to (create window with default profile)
    tell current session of theWindow to write text "$esc_cmd"
    set i to i + 1
  end repeat
end tell
APPLESCRIPT
    ;;
  quit)
    echo "iterm.sh: refusing to quit — that would kill the user's open work." >&2
    echo "          Bench drivers must only open new windows; close them via" >&2
    echo "          AppleScript window-id tracking (TODO) or manually." >&2
    exit 2
    ;;
  *)
    echo "iterm.sh: unknown sub-command: ${cmd_word:-(none)}" >&2
    exit 2
    ;;
esac

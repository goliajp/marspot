#!/usr/bin/env bash
# bin/drivers/iterm.sh — automated iTerm2 launcher.
#
# Usage:
#   bin/drivers/iterm.sh run-tabs <n> <cmd>
#     Open one fresh iTerm2 window with <n> tabs.  Each tab runs <cmd>.
#     Prints the window ID (one line) to stdout — pass it to close-windows
#     when the bench is done.
#
#   bin/drivers/iterm.sh run-windows <n> <cmd>
#     Open <n> separate iTerm2 windows, each running <cmd>.  Prints
#     <n> window IDs to stdout, one per line.
#
#   bin/drivers/iterm.sh close-windows <id> [<id>...]
#     Close exactly those windows by id.  Does NOT touch any other
#     windows — that's the whole point of the id-tracking contract.
#
# All iTerm2 dispatch goes through `osascript`; the run-tabs / run-windows
# subcommands print iTerm2's window IDs which are stable identifiers
# (not the visible "name").  History/contents matching as a fallback
# is intentionally avoided — it can collateral-close the user's real
# windows that happen to mention "mars-bench" somewhere.

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
    esc_cmd=${user_cmd//\"/\\\"}
    # First tab is the new window's initial session; create n-1 more tabs.
    osascript <<APPLESCRIPT
tell application "iTerm"
  activate
  set newWin to (create window with default profile)
  tell current session of newWin to write text "$esc_cmd"
  repeat ($n - 1) times
    tell newWin to create tab with default profile
    tell current session of newWin to write text "$esc_cmd"
  end repeat
  return (id of newWin) as text
end tell
APPLESCRIPT
    ;;
  run-windows)
    n=${1:?n}; shift
    user_cmd=${1:?cmd}; shift || true
    esc_cmd=${user_cmd//\"/\\\"}
    osascript <<APPLESCRIPT
tell application "iTerm"
  activate
  set ids to {}
  repeat $n times
    set newWin to (create window with default profile)
    tell current session of newWin to write text "$esc_cmd"
    set end of ids to (id of newWin) as text
  end repeat
  set AppleScript's text item delimiters to linefeed
  return ids as text
end tell
APPLESCRIPT
    ;;
  close-windows)
    # Close only the IDs we're given.  Each id may not exist anymore
    # (window already closed by user) — we ignore those silently.
    if (( $# == 0 )); then
      echo "iterm.sh close-windows: no ids given (nothing to close)" >&2
      exit 0
    fi
    # Build the AppleScript id list.
    ids_list=""
    for id in "$@"; do
      if [[ -n "$ids_list" ]]; then ids_list+=","; fi
      ids_list+="$id"
    done
    osascript <<APPLESCRIPT >/dev/null
tell application "iTerm"
  set targetIds to {$ids_list}
  set closedN to 0
  repeat with wid in targetIds
    try
      close (first window whose id is wid)
      set closedN to closedN + 1
    end try
  end repeat
  return closedN
end tell
APPLESCRIPT
    ;;
  quit)
    echo "iterm.sh: refusing to quit — that would kill the user's open work." >&2
    exit 2
    ;;
  *)
    echo "iterm.sh: unknown sub-command: ${cmd_word:-(none)}" >&2
    echo "  usage: iterm.sh run-tabs <n> <cmd>" >&2
    echo "         iterm.sh run-windows <n> <cmd>" >&2
    echo "         iterm.sh close-windows <id> [<id>...]" >&2
    exit 2
    ;;
esac

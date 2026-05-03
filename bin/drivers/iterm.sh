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
  run-tabs|run-windows)
    n=${1:?n}; shift
    user_cmd=${1:?cmd}; shift || true
    esc_cmd=${user_cmd//\"/\\\"}
    # ID-diff approach: snapshot window IDs before, create windows /
    # tabs, snapshot after, return the diff.  This guarantees every
    # window we created is reported, even if a per-window `write text`
    # fails after `create window` succeeded — otherwise the tracking
    # leaks windows that show up as "unknown" empty sessions.
    #
    # Focus isolation: we used to `activate` iTerm here so the new
    # windows took focus.  That blocks the user's other work — they
    # can't keep typing in their editor while the bench runs.  We now
    # let iTerm get whatever focus AppleScript happens to give it
    # (usually it does come to front momentarily); the scenario
    # restores user focus immediately after.  Bench windows keep
    # rendering / draining PTY in the background — focus doesn't
    # affect throughput, only which app receives keystrokes.
    if [[ "$cmd_word" == "run-tabs" ]]; then
      osascript <<APPLESCRIPT
tell application "iTerm"
  set beforeIds to {}
  repeat with w in windows
    try
      set end of beforeIds to (id of w) as text
    end try
  end repeat
  set newWin to (create window with default profile)
  try
    tell current session of newWin to write text "$esc_cmd"
  end try
  repeat ($n - 1) times
    try
      tell newWin to create tab with default profile
      tell current session of newWin to write text "$esc_cmd"
    end try
  end repeat
  set newIds to {}
  repeat with w in windows
    try
      set wid to (id of w) as text
      if wid is not in beforeIds then set end of newIds to wid
    end try
  end repeat
  set AppleScript's text item delimiters to linefeed
  return newIds as text
end tell
APPLESCRIPT
    else
      osascript <<APPLESCRIPT
tell application "iTerm"
  set beforeIds to {}
  repeat with w in windows
    try
      set end of beforeIds to (id of w) as text
    end try
  end repeat
  repeat $n times
    set newWin to (create window with default profile)
    try
      tell current session of newWin to write text "$esc_cmd"
    end try
  end repeat
  set newIds to {}
  repeat with w in windows
    try
      set wid to (id of w) as text
      if wid is not in beforeIds then set end of newIds to wid
    end try
  end repeat
  set AppleScript's text item delimiters to linefeed
  return newIds as text
end tell
APPLESCRIPT
    fi
    ;;
  close-windows)
    if (( $# == 0 )); then
      exit 0
    fi
    ids_list=""
    for id in "$@"; do
      [[ -n "$ids_list" ]] && ids_list+=","
      ids_list+="$id"
    done
    # Path A: AppleScript `close`.  Works for most windows.
    osascript <<APPLESCRIPT >/dev/null 2>&1 || true
tell application "iTerm"
  set targetIds to {$ids_list}
  repeat with wid in targetIds
    try
      close (first window whose id is wid)
    end try
  end repeat
end tell
APPLESCRIPT
    sleep 0.4
    # Path B: any survivor (e.g. a window with no current session
    # because `write text` failed after `create window`) gets a
    # focused Cmd-W via System Events.  Targets only the specific
    # window — never a global Cmd-W loop.
    for id in "$@"; do
      survives=$(osascript -e "tell application \"iTerm\"
  try
    return name of (first window whose id is $id)
  end try
end tell" 2>/dev/null)
      [[ -z "$survives" ]] && continue
      osascript -e "tell application \"iTerm\" to select (first window whose id is $id)" >/dev/null 2>&1 || true
      osascript -e 'tell application "iTerm" to activate' >/dev/null 2>&1 || true
      sleep 0.15
      osascript -e 'tell application "System Events" to tell process "iTerm2" to keystroke "w" using command down' >/dev/null 2>&1 || true
      sleep 0.2
    done
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

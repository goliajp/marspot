#!/usr/bin/env bash
# bin/drivers/terminal.sh — automated Terminal.app launcher.
#
# Usage:
#   bin/drivers/terminal.sh run-windows <n> <cmd>
#     Open <n> Terminal.app windows (not tabs — Terminal.app's tab
#     AppleScript surface is fragile across macOS versions), each
#     running <cmd>.  Prints <n> window IDs to stdout, one per line.
#
#   bin/drivers/terminal.sh run-single <cmd>
#     Convenience for run-windows 1.  Prints the new window ID.
#
#   bin/drivers/terminal.sh close-windows <id> [<id>...]
#     Close exactly those windows.  Terminal.app's `close window`
#     command silently no-ops on windows whose shell is still alive
#     (you have to confirm "kill the running process?"), so we kill
#     the shell on the window's tty first, then close.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

cmd_word=${1:-}
shift || true

case "$cmd_word" in
  run-single|run-windows)
    if [[ "$cmd_word" == "run-single" ]]; then
      n=1
    else
      n=${1:?n}; shift
    fi
    user_cmd=${1:?cmd}; shift || true
    esc_cmd=${user_cmd//\"/\\\"}
    # ID-diff approach: Terminal.app's `do script` returns a tab object
    # but extracting its window's id is awkward.  Snapshot the existing
    # window-id set, run do-script n times, the new ids are the diff.
    osascript <<APPLESCRIPT
tell application "Terminal"
  activate
  set beforeIds to {}
  repeat with w in windows
    try
      set end of beforeIds to id of w
    end try
  end repeat
  repeat $n times
    do script "$esc_cmd"
    delay 0.15
  end repeat
  delay 0.2
  set newIds to {}
  repeat with w in windows
    try
      set wid to id of w
      if wid is not in beforeIds then
        set end of newIds to wid as text
      end if
    end try
  end repeat
  set AppleScript's text item delimiters to linefeed
  return newIds as text
end tell
APPLESCRIPT
    ;;
  close-windows)
    if (( $# == 0 )); then
      exit 0
    fi
    # Step 1: collect ttys for each id, kill the shells.
    ttys=$(osascript <<APPLESCRIPT
tell application "Terminal"
  set out to {}
  set targetIds to {$(IFS=,; echo "$*")}
  repeat with wid in targetIds
    try
      set w to (first window whose id is wid)
      set end of out to (tty of tab 1 of w)
    end try
  end repeat
  set AppleScript's text item delimiters to linefeed
  return out as text
end tell
APPLESCRIPT
)
    for tty_path in $ttys; do
      tty_name=${tty_path#/dev/}
      for pid in $(ps -t "$tty_name" -o pid= 2>/dev/null | tr -d ' '); do
        kill "$pid" 2>/dev/null || true
      done
    done
    sleep 0.3

    # Step 2: focus Terminal.app and close each window via Cmd-W from
    # System Events.  Terminal.app's AppleScript `close` returns
    # success but the window doesn't actually close in some configs;
    # Cmd-W from the foreground process reliably does.  We focus the
    # window first to ensure Cmd-W targets it.
    osascript <<APPLESCRIPT >/dev/null 2>&1
tell application "Terminal" to activate
delay 0.2
APPLESCRIPT
    for id in "$@"; do
      osascript <<APPLESCRIPT >/dev/null 2>&1 || true
tell application "Terminal"
  try
    set w to (first window whose id is $id)
    set frontmost of w to true
  end try
end tell
delay 0.1
tell application "System Events"
  tell process "Terminal"
    try
      keystroke "w" using command down
    end try
  end tell
end tell
APPLESCRIPT
      sleep 0.1
    done
    ;;
  quit)
    echo "terminal.sh: refusing to quit Terminal.app — that would kill the user's work." >&2
    exit 2
    ;;
  *)
    echo "terminal.sh: unknown sub-command: ${cmd_word:-(none)}" >&2
    echo "  usage: terminal.sh run-single <cmd>" >&2
    echo "         terminal.sh run-windows <n> <cmd>" >&2
    echo "         terminal.sh close-windows <id> [<id>...]" >&2
    exit 2
    ;;
esac

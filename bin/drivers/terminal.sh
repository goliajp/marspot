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
    # Focus isolation note: we used to `activate` Terminal here.
    # That stole focus from whatever the user was working in.  We now
    # rely on AppleScript's implicit foregrounding (Terminal gets a
    # brief flash to front) and the scenario restores user focus
    # immediately after dispatch.
    osascript <<APPLESCRIPT
tell application "Terminal"
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
    # Build the AppleScript id list once.
    ids_alist=""
    for id in "$@"; do
      [[ -n "$ids_alist" ]] && ids_alist+=", "
      ids_alist+="$id"
    done

    # Step 1: kill the shells running in each target window's tty.
    # With shells dead, the windows show "process completed" and the
    # AppleScript `close` command stops hitting the kill-prompt.
    for id in "$@"; do
      tty_path=$(osascript -e "tell application \"Terminal\" to return tty of tab 1 of (first window whose id is $id)" 2>/dev/null || echo "")
      [[ -z "$tty_path" ]] && continue
      tty_name=${tty_path#/dev/}
      for pid in $(ps -t "$tty_name" -o pid= 2>/dev/null | tr -d ' '); do
        kill "$pid" 2>/dev/null || true
      done
    done
    sleep 0.6

    # Step 2: close each by id.  After the shell is gone, plain
    # `close ... saving no` works.  If a particular window survives
    # (rare — usually a profile that re-prompts), fall back to focused
    # Cmd-W on just that id, never a global Cmd-W loop.
    for id in "$@"; do
      osascript -e "tell application \"Terminal\" to close (first window whose id is $id) saving no" >/dev/null 2>&1 || true
    done
    sleep 0.5

    # Final pass: anything still alive gets a focused Cmd-W.
    for id in "$@"; do
      still=$(osascript -e "tell application \"Terminal\" to return id of (first window whose id is $id)" 2>/dev/null || echo "")
      [[ -z "$still" ]] && continue
      osascript -e "tell application \"Terminal\" to set selected of tab 1 of (first window whose id is $id) to true" >/dev/null 2>&1 || true
      osascript -e 'tell application "Terminal" to activate' >/dev/null 2>&1 || true
      sleep 0.15
      osascript -e 'tell application "System Events" to tell process "Terminal" to keystroke "w" using command down' >/dev/null 2>&1 || true
      sleep 0.2
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

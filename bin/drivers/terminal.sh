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
#   bin/drivers/terminal.sh <run-sub-command> --pid-file <file> ...
#     Any run sub-command: also append the app's pid to <file>.
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
# shellcheck source=bin/_app-pids.sh
source "$ROOT/bin/_app-pids.sh"

cmd_word=${1:-}
shift || true
# --pid-file <file>: once the app is up, append its pid there.  The
# caller passes it only for an app that was not running before, so it
# can later stop exactly what it started.
pid_file=""
if [[ ${1:-} == --pid-file ]]; then
  pid_file=${2:?pid-file}
  shift 2
fi

case "$cmd_word" in
  run-single|run-windows)
    if [[ "$cmd_word" == "run-single" ]]; then
      n=1
    else
      n=${1:?n}; shift
    fi
    user_cmd=${1:?cmd}; shift || true
    # Same paste-threshold pattern as iterm.sh — Terminal.app's
    # `do script` accepts long strings but smart-paste / autocompletion
    # can mangle escape-heavy bench cmds. Stash in a wrapper, dispatch
    # a short `bash <wrapper>` instead.
    wrapper=$(mktemp /tmp/terminal-wrapper-XXXXXX)
    printf '#!/bin/bash\n%s\n' "$user_cmd" > "$wrapper"
    chmod 0755 "$wrapper"
    esc_cmd="bash $wrapper"
    # ID-diff approach: Terminal.app's `do script` returns a tab object
    # but extracting its window's id is awkward.  Snapshot the existing
    # window-id set, run do-script n times, the new ids are the diff.
    # Focus isolation note: we used to `activate` Terminal here.
    # That stole focus from whatever the user was working in.  We now
    # rely on AppleScript's implicit foregrounding (Terminal gets a
    # brief flash to front) and the scenario restores user focus
    # immediately after dispatch.
    osascript <<APPLESCRIPT || rc=$?
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
    # recorded even when the script failed part way: the app is up
    [[ -z $pid_file ]] || record_app_pid com.apple.Terminal "$pid_file"
    exit "${rc:-0}"
    ;;
  close-windows)
    if (( $# == 0 )); then
      exit 0
    fi

    # Step 1: kill the shells running in each target window's tty.
    # This is the *primary* cleanup — once the shell is dead, the
    # window stops doing any work even if its frame stays on screen.
    # Any leftover "[Process completed]" window is harmless: no PTY,
    # no CPU, no PID — just a static frame the user can close manually.
    for id in "$@"; do
      tty_path=$(osascript -e "tell application \"Terminal\" to return tty of tab 1 of (first window whose id is $id)" 2>/dev/null || echo "")
      [[ -z "$tty_path" ]] && continue
      tty_name=${tty_path#/dev/}
      for pid in $(ps -t "$tty_name" -o pid= 2>/dev/null | tr -d ' '); do
        kill "$pid" 2>/dev/null || true
      done
    done
    sleep 0.5

    # Step 2: ask Terminal.app to close the window.  Best-effort —
    # depends on the user's profile setting "When the shell exits".
    # If "Close the window" is set, the window is already gone after
    # step 1.  If "Don't close the window" is set, this AppleScript
    # close request will dismiss the leftover frame on most configs.
    # We do NOT fall back to Cmd-W keystrokes — those are global,
    # and if focus drifts they hit the user's foreground app.
    for id in "$@"; do
      osascript -e "tell application \"Terminal\" to close (first window whose id is $id) saving no" >/dev/null 2>&1 || true
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

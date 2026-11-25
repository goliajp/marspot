#!/usr/bin/env bash
# bin/_active_iterm.sh — the iTerm2 half of bin/measure-active.sh: one
# window laid out as a grid of split panes, each running a given command,
# and its teardown.  Runs in the console session (AppleScript reaches
# iTerm2 only from there).
#
#   _active_iterm.sh open PIDFILE RUN CMD ROWS LAST X1 Y1 X2 Y2
#     launches iTerm2 and writes the pids of its processes to PIDFILE
#     at once, before anything else can fail, and again after the
#     window is made (iTermServer comes with the first pane).  The
#     caller refuses to run while iTerm2 is up, so every one of them was
#     started here.  Any window iTerm2 opens by itself at launch is
#     closed first.  Then a window with bounds {X1, Y1, X2, Y2}
#     (top-left origin), ROWS rows of 4 panes each except the last,
#     which has LAST; ROWS and LAST are 1, 2 or 4, because iTerm2 splits
#     a pane in halves and only those counts come out even.  Prints the
#     window id, then each pane's columns x rows as iTerm2 reports them.
#   _active_iterm.sh close PIDFILE [WINDOW_ID]
#     while a recorded iTerm2 is alive: types `exit` into every pane of
#     that window, or of every window without an id (a pane whose shell
#     exits cleanly closes without asking; closing a window of several
#     live panes raises a confirmation nobody can click here), quits
#     iTerm2, and sends SIGTERM to the recorded pids still running an
#     iTerm2 binary after ten seconds.  Removes PIDFILE.  The run's
#     timeout path calls this too, with the file the run left behind.
set -uo pipefail

ITERM_PATTERN='(iTerm\.app/Contents/|Application Support/iTerm2/[i]TermServer)'

# matched on the executable path (comm), not the command line: a process
# that only names iTerm.app in its arguments is never recorded
iterm_pids() { ps -Ao pid=,comm= | grep -E "$ITERM_PATTERN" | awk '{print $1}'; }

record() { # pidfile
  { cat "$1" 2>/dev/null; iterm_pids; } | sort -un > "$1.tmp" && mv "$1.tmp" "$1"
}

# the recorded pids that still run something of iTerm2's: a pid read
# back from a file may belong to anything by now
recorded_alive() { # pidfile [pattern]
  local p
  for p in $(cat "$1" 2>/dev/null); do
    ps -o comm= -p "$p" 2>/dev/null | grep -qE "${2:-$ITERM_PATTERN}" && echo "$p"
  done
}

# writes `exit` into every session of window $1, or of every window
exit_all() {
  /usr/bin/timeout 30 /usr/bin/osascript - "${1:-0}" <<'AS' >/dev/null 2>&1
on run argv
  set wid to (item 1 of argv) as integer
  tell application "iTerm"
    if wid is 0 then
      set ws to every window
    else
      set ws to {first window whose id is wid}
    end if
    repeat with w in ws
      repeat with t in tabs of w
        repeat with s in sessions of t
          try
            tell s to write text "exit"
          end try
        end repeat
      end repeat
    end repeat
  end tell
end run
AS
}

no_windows() {
  local i
  for (( i = 0; i < 50; i++ )); do
    [[ $(/usr/bin/timeout 10 /usr/bin/osascript -e 'tell application "iTerm" to count windows' 2>/dev/null) == 0 ]] && return 0
    sleep 0.2
  done
  return 1
}

open_window() {
  local pids=$1 run=$2 cmd=$3 rows=$4 last=$5 i
  : > "$pids"
  /usr/bin/timeout 30 /usr/bin/open -g -a iTerm || { record "$pids"; return 2; }
  for (( i = 0; i < 100; i++ )); do
    record "$pids"
    [[ -n $(recorded_alive "$pids" 'MacOS/iTerm2') ]] && break
    sleep 0.2
  done
  [[ -n $(recorded_alive "$pids" 'MacOS/iTerm2') ]] || { echo "FAIL: iTerm2 did not start" >&2; return 2; }
  # the window a launch opens by itself comes a moment after the process
  for (( i = 0; i < 25; i++ )); do
    [[ $(/usr/bin/timeout 10 /usr/bin/osascript -e 'tell application "iTerm" to count windows' 2>/dev/null) =~ ^[1-9] ]] && break
    sleep 0.2
  done
  exit_all
  no_windows || { record "$pids"; echo "FAIL: iTerm2's own window did not close" >&2; return 2; }
  cat > "$run/open.applescript" <<'AS'
on two(s, vert, cmd)
  tell application "iTerm"
    if vert then
      tell s to set b to (split vertically with default profile command cmd)
    else
      tell s to set b to (split horizontally with default profile command cmd)
    end if
  end tell
  return {s, b}
end two

-- halving twice: s|b, then s|c|b, then s|c|b|d, four equal parts
on four(s, vert, cmd)
  set {s, b} to my two(s, vert, cmd)
  set {s, c} to my two(s, vert, cmd)
  set {b, d} to my two(b, vert, cmd)
  return {s, c, b, d}
end four

on run argv
  set cmd to item 1 of argv
  set nrows to (item 2 of argv) as integer
  set lastn to (item 3 of argv) as integer
  set bnds to {(item 4 of argv) as integer, (item 5 of argv) as integer, (item 6 of argv) as integer, (item 7 of argv) as integer}
  tell application "iTerm"
    set w to (create window with default profile command cmd)
    set bounds of w to bnds
    set a to current session of w
  end tell
  if nrows is 1 then set rs to {a}
  if nrows is 2 then set rs to my two(a, false, cmd)
  if nrows is 4 then set rs to my four(a, false, cmd)
  repeat with i from 1 to nrows
    set k to 4
    if i is nrows then set k to lastn
    if k is 2 then my two(item i of rs, true, cmd)
    if k is 4 then my four(item i of rs, true, cmd)
  end repeat
  tell application "iTerm"
    activate
    set out to {(id of w) as text}
    repeat with s in sessions of current tab of w
      set end of out to ((columns of s) as text) & "x" & ((rows of s) as text)
    end repeat
  end tell
  set AppleScript's text item delimiters to " "
  return out as text
end run
AS
  /usr/bin/timeout 90 /usr/bin/osascript "$run/open.applescript" "$cmd" "$rows" "$last" "${@:6}"
  i=$?
  record "$pids"
  return $(( i == 0 ? 0 : 2 ))
}

close_window() {
  local pids=$1 wid=${2:-} i alive
  [[ -f $pids ]] || return 0
  # AppleScript would launch an iTerm2 that is not running
  if [[ -n $(recorded_alive "$pids" 'MacOS/iTerm2') ]]; then
    exit_all "$wid"
    no_windows
    /usr/bin/timeout 15 /usr/bin/osascript -e 'tell application "iTerm" to quit' >/dev/null 2>&1
  fi
  for (( i = 0; i < 50; i++ )); do
    alive=$(recorded_alive "$pids" | paste -sd' ' -)
    [[ -z $alive ]] && break
    sleep 0.2
  done
  if [[ -n $alive ]]; then
    echo "    iTerm2 did not quit; SIGTERM to its recorded pids: $alive" >&2
    # shellcheck disable=SC2086
    kill -TERM $alive 2>/dev/null
  fi
  command rm -f "${pids:?}"
  return 0
}

case "${1:-}" in
  open)  shift; open_window "$@" ;;
  close) shift; close_window "$@" ;;
  *)     echo "usage: _active_iterm.sh open|close ..." >&2; exit 2 ;;
esac

# shellcheck shell=bash
#
# quit_app for the suites that walk the app through its states: the
# quit the Dock sends, to the running shell ($L1), paced so that the
# Dock keeps no tile of it.  Sourced by _ownership-lib.sh, which
# defines count, wait_count and die.

# The Dock drops the tile of an app that turned accessory (the resident)
# lazily, up to a second or so later.  Opened and quit again before
# that, the app leaves a tile the Dock keeps after the process is gone,
# one per opening (mini2, 2026-11-27: 46 of them over three runs of the
# twenty quits in test-session-ownership.sh).  So a quit waits, at most
# ten seconds, until the Dock has logged a removal since the one before.
QUIT_AT="" QUIT_REMOVALS=0
dock_removals_since() {
  /usr/bin/log show --start "$1" --style compact \
    --predicate 'process == "Dock" AND category == "dock-tiles" AND eventMessage BEGINSWITH "Removing tiles"' 2>/dev/null \
    | grep -c 'Removing tiles'
}
dock_settled() {
  local i
  [[ -n $QUIT_AT ]] || return 0
  for (( i = 0; i < 20; i++ )); do
    (( $(dock_removals_since "$QUIT_AT") > QUIT_REMOVALS )) && return 0
    sleep 0.5
  done
  echo "    (the Dock logged no tile removal within ten seconds of the last quit)"
}
quit_app() {
  local cleanups answered i
  dock_settled
  QUIT_AT="$(date '+%Y-%m-%d %H:%M:%S')"
  QUIT_REMOVALS=$(dock_removals_since "$QUIT_AT")
  cleanups=$(count SHELL_QUIT_CLEANUP)
  # the app answers on its command socket before it takes events:
  # a quit sent in that moment is not taken
  for (( i = 0; i < 10; i++ )); do
    answered="$(osascript -l JavaScript -e "ObjC.import('AppKit');
      \$.NSRunningApplication.runningApplicationWithProcessIdentifier($L1).terminate" 2>&1)"
    [[ $answered == true ]] && break
    sleep 0.5
  done
  [[ $answered == true ]] || die "the quit request was not delivered: $answered"
  (( i == 0 )) || echo "    (the quit request was refused $i time(s) first)"
  wait_count SHELL_QUIT_CLEANUP $((cleanups + 1)) 10 || die "the app did not act on the quit"
}

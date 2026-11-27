# shellcheck shell=bash
#
# quit_app for the suites that walk the app through its states: the
# quit the Dock sends, to the running shell ($L1), paced so that the
# Dock keeps no tile of it.  Sourced by _ownership-lib.sh, which
# defines count, wait_count, die and SB.
#
# The resident shell turns accessory on a quit and regular again when it
# is opened, the same process each time.  The Dock drops the old tile
# lazily; opened and quit again before it has, the app leaves a tile the Dock
# keeps after the process is gone, one per opening (mini2, 2026-11-27: 46
# of them over three runs of the twenty quits in test-session-ownership.sh).
# The shell itself keeps its tile until the Dock has dropped the one
# before (resident/dock_leave.rs), so quick quits leave nothing; the
# twenty quick ones in test-session-ownership.sh use quit_app_unpaced and
# check that.  Every other quit returns only once the Dock probe
# (bin/_dock-probe.sh) lands where it did before the app was first opened:
# the Dock holds no tile of the app any more.  Not within DOCK_SETTLE_S
# seconds fails the suite.

# shellcheck source=bin/_dock-probe.sh
source "$ROOT/bin/_dock-probe.sh"
DOCK_SETTLE_S="${DOCK_SETTLE_S:-20}"
DOCK_BASE=""

# before the app is first opened: where a new tile goes with none of ours
dock_baseline() {
  local out
  DOCK_PROBE_EXE="$(dock_probe_build "$SB/dockprobe")" || die "could not build the Dock probe"
  out="$(dock_probe "$DOCK_PROBE_EXE")" || die "the Dock probe before the first open: ${out#unread=}"
  DOCK_BASE="$(sed -n 's/^dock_pid=//p' <<<"$out"):$(sed -n 's/^index=//p' <<<"$out")"
  echo "    (the Dock puts a new tile at index ${DOCK_BASE#*:} before the app is opened)"
}

# until the probe lands at the baseline plus OWN (default 0: the app is
# quit; 1 while it is open) under the same Dock
dock_settled() {
  local own=${1:-0} out got="" want start=$SECONDS
  [[ -n $DOCK_BASE ]] || die "the Dock was not probed before the app was first opened"
  want=$(( ${DOCK_BASE#*:} + own ))
  while :; do
    out="$(dock_probe "$DOCK_PROBE_EXE")" || die "the Dock probe with $own tile(s) of ours expected: ${out#unread=}"
    got="$(sed -n 's/^dock_pid=//p' <<<"$out"):$(sed -n 's/^index=//p' <<<"$out")"
    [[ ${got%%:*} == "${DOCK_BASE%%:*}" ]] || die "the Dock restarted during the walk (pid ${DOCK_BASE%%:*}, now ${got%%:*})"
    DOCK_LAST=${got#*:}
    (( DOCK_LAST == want )) && return 0
    # below where it should be: some other tile went, and one of ours could be
    # hidden behind it; waiting cannot tell
    (( DOCK_LAST > want )) \
      || die "unreadable: the Dock probe at index $DOCK_LAST, below $want (${DOCK_BASE#*:} before the app was opened, $own of ours expected)"
    (( SECONDS - start < DOCK_SETTLE_S )) \
      || die "the Dock still holds $(( DOCK_LAST - want )) tile(s) of ours too many ${DOCK_SETTLE_S}s on: the probe at index $DOCK_LAST, $want expected (${DOCK_BASE#*:} before the app was opened, $own of ours)"
    sleep 1
  done
}

# the quit alone, not waiting for the Dock
quit_app_unpaced() {
  local cleanups answered i
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

quit_app() {
  quit_app_unpaced
  dock_settled
}

# shellcheck shell=bash
# bin/_e2e-suites.sh — what bin/test-e2e-remote.sh knows about each suite:
# whether it is a measurement, whether its app keeps a Dock tile, and how
# many tiles of it the Dock may hold at once.  Sourced; defines functions
# only.

# The suites that are measurements.  Their exit codes are the four of
# bin/measure-idle-marspot.sh: 1 is over budget, 2 the instrument
# failing, 3 a host too busy to read.
is_measurement() {
  [[ $1 == test-idle-wakeups.sh || $1 == measure-idle-marspot.sh || $1 == test-awake-cost.sh \
    || $1 == measure-active.sh ]]
}

# Exit 3 from these means a measurement in them was read on a busy host
# and everything else passed.
may_be_unmeasured() {
  is_measurement "$1" || [[ $1 == soak-l3.sh ]]
}

# The suites whose app keeps its Dock tile.  Every other suite runs the
# app with MARSPOT_TEST_NO_DOCK, as an accessory app: no tile, nothing
# for the runner's Dock to keep.  These keep it because what they test
# is the app as a regular one:
#   - its LaunchServices record and the sessions' coalition (lsappinfo
#     treats a program with no Dock tile differently: see
#     test-reseat-bound.sh), and the record going from accessory back to
#     regular when the resident is opened again;
#   - activating it and making a window key before a close
#     (test-multi-window.sh).
# Their tiles are counted by the check in the hold (_e2e-suite-clean.sh)
# against the maximum each declares (dock_tiles_max), and the walks
# among them quit only once the Dock dropped the app's tile
# (_ownership-quit.sh).  test-reopen-after-quit.sh switches the resident
# from accessory back to regular, which is what a user's open after a
# quit does; it quits only once the window is up and answered, after the
# Dock dropped the last tile (on mini2 its inserts stayed at the same
# index, two at most at once), and the probe after it says whether one
# was left.  test-quit-after-update.sh compares the shell's coalition as
# lsappinfo lists it, which it lists for an accessory app as for a
# regular one.
keeps_dock_tile() {
  case $1 in
    test-session-ownership.sh | test-one-app-record.sh | test-coalition-reclaim.sh | test-reseat-bound.sh \
      | test-reopen-after-quit.sh | test-multi-window.sh) return 0 ;;
    *) return 1 ;;
  esac
}

# dock_tiles_max FILE: the most tiles of its own the Dock may hold at once
# while the suite in FILE runs, from its one line
#   # e2e-dock-tiles-max: N  why N
# and 2 when it has none: the app's tile, and its next image's while the
# Dock still holds the last one.  N is the most tiles of the suite's
# regular apps present at one moment by design, a gone program's tile
# counted until the Dock drops it; the reason names that moment and each
# program in it.  The number is reviewed with the suite;
# nothing at run time raises it.  A line that names the key but is not of
# that form, or a second one, is refused rather than read as the default
DOCK_TILES_KEY="e2e-dock-tiles-max"
dock_tiles_max() {
  local lines n
  lines="$(grep -E "^#[[:space:]]*$DOCK_TILES_KEY" "${1:?suite file}")" || {
    [[ -r $1 ]] || { echo "${1##*/}: cannot be read" >&2; return 2; }
    echo 2
    return 0
  }
  n="$(grep -c . <<<"$lines")"
  if (( n != 1 )); then
    echo "${1##*/}: $n lines name $DOCK_TILES_KEY, one at most" >&2
    return 2
  fi
  if [[ ! $lines =~ ^#\ $DOCK_TILES_KEY:\ ([1-9][0-9]?)[[:space:]]+[^[:space:]] ]]; then
    echo "${1##*/}: malformed: '$lines' (wanted '# $DOCK_TILES_KEY: N  reason', N from 1 to 99)" >&2
    return 2
  fi
  echo "${BASH_REMATCH[1]}"
}

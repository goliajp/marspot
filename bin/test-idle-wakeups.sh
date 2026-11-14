#!/usr/bin/env bash
# bin/test-idle-wakeups.sh — an idle app must not wake on a clock.
#
# Opens a sandbox window of four panes at shell prompts, lets it settle,
# and counts the timer wake-ups of all its processes over 30 seconds
# (the kernel's own per-process counter, see bin/_idle_wakeups.py).
# Exits 1 when they exceed IDLE_WAKEUP_BUDGET per second on a quiet
# host; 3 when they do next to other work, which is no verdict; 2 when
# there was no reading (the codes of bin/measure-idle-marspot.sh).  Then
# quits the app with its sessions kept and holds the shell that stays to
# budgets of its own, wake-ups and footprint.
#
# A timer that fires whether or not there is work is how idle cost comes
# back: each one is cheap, nothing else notices it, and twelve panes
# multiply it.  One 25 ms poll in one process is 40 wake-ups a second,
# far past the budget.
#
# Opens a real window, so it runs on the runner:
#   bin/test-e2e-remote.sh test-idle-wakeups.sh
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

export IDLE_COLS=2 IDLE_ROWS=2
# The supervisor's clock comes to rest about 45 s after the last output:
# 30 s for a pane's terminal to count as quiet, then the confirming
# sweeps.  Measuring sooner measures the app while it is still awake.
export IDLE_SETTLE="${IDLE_SETTLE:-110}"
export IDLE_WINDOW=30 IDLE_READINGS=1
# 0.6 to 0.7 a second is what was measured at rest (1, 4 and 12 panes
# alike, 2026-11-12, six windows); the two a second of the supervisor's
# own clock, were it running, is already over this.
export IDLE_WAKEUP_BUDGET="${IDLE_WAKEUP_BUDGET:-1.2}"
# and a row of its own for the app quit with its sessions kept.  The
# shell that stays for them does nothing of its own; what it measures,
# 0.3 a second, is the graphics driver's memory-pool timer, which runs
# for the life of a process that has once drawn (the same 0.3 the
# shell and the core each show above)
export IDLE_RESIDENT=1 IDLE_RESIDENT_BUDGET="${IDLE_RESIDENT_BUDGET:-0.4}"
# What that shell keeps.  16.0 to 16.2 MiB measured (2026-11-14, three
# quits); an AppKit program of thirty lines that has shown one window
# and closed it keeps 15.6 MiB, so this is the platform's figure and
# half a megabyte of the graphics driver's.  A megabyte over is
# something of the app's that the quit did not put away: the table of
# the host's processes and the room their arguments are read into were
# that megabyte.
export IDLE_RESIDENT_FOOTPRINT_KIB="${IDLE_RESIDENT_FOOTPRINT_KIB:-17000}"
exec "$ROOT/bin/measure-idle-marspot.sh"

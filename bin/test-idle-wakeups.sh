#!/usr/bin/env bash
# bin/test-idle-wakeups.sh — an idle app must not wake on a clock.
#
# Opens a sandbox window of four panes at shell prompts, lets it settle,
# and counts the timer wake-ups of all its processes over 30 seconds
# (the kernel's own per-process counter, see bin/_idle_wakeups.py).
# Fails when they exceed IDLE_WAKEUP_BUDGET per second.
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
exec "$ROOT/bin/measure-idle-marspot.sh"

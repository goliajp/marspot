#!/usr/bin/env bash
# bin/_runner-jobs.sh — how many test threads a suite takes on the runner.
#
# One per core when the suite is the only heavy job there.  The runner
# is shared: with three suites at one per core each, forty-two test
# processes ran on fourteen cores and tests failed on their time limits
# in every one of them.  So the cores are divided among the heavy jobs
# holding the host's lock at this moment, this one included, and a
# suite never takes fewer than four.
#
# Run inside the lock, on the runner.  Prints the number.
set -euo pipefail

cores="$(sysctl -n hw.ncpu)"
holders="$(/usr/local/bin/bench-lock status 2>/dev/null \
  | grep -cE '^[0-9]+[[:space:]]+heavy[[:space:]]+holding' || true)"
(( holders >= 1 )) || holders=1
jobs=$(( cores / holders ))
(( jobs >= 4 )) || jobs=4
echo "$jobs"

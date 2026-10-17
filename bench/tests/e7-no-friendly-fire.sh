#!/usr/bin/env bash
# bench/tests/e7-no-friendly-fire.sh — no script kills marspot / mcli by name.
#
# Asserts that no script under bin/ kills our binaries by name or by
# command line (`pkill`, `killall`).  By-name killing is friendly fire:
# the runners are shared, and other sessions run their own marspot,
# mcli and sandbox shells there.  A sanity bench.sh during a 30-min
# --extended run once killed the soak's binary at t=41 s (2026-05-05)
# and turned the rest of the run into zero-RSS rows that passed the
# drift gate.  Scripts stop the pids they recorded instead (kill_app,
# dev_kill_shell_core).
#
# install-remote.sh is the one exception: on request
# (MARSPOT_REMOTE_FORCE=1) it stops the installed app on the host it is
# about to install over.

set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

fail=0

echo "==> E7: no by-name kill of marspot/mcli in any script"

for f in bin/*.sh bin/*/*.sh; do
  [[ -f "$f" ]] || continue
  [[ $f == bin/install-remote.sh ]] && continue
  # Exclude comment lines — those may document past behaviour
  matches=$(grep -nE '(pkill|killall)[^#]*(marspot|mcli|SESSION_BIN|SHELL_BIN|CORE_BIN|DEV_TARGET|\$bin)' "$f" 2>/dev/null \
    | grep -vE '^[0-9]+:[[:space:]]*#')
  if [[ -n "$matches" ]]; then
    echo "  ✗ $f kills by name:"
    echo "$matches" | sed 's/^/      /'
    fail=1
  fi
done

echo
if [[ $fail -eq 0 ]]; then
  echo "PASS: no script kills marspot/mcli by name"
  exit 0
else
  echo "FAIL: a script would kill other sessions' marspot/mcli instances"
  exit 1
fi

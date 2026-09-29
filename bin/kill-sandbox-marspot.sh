#!/usr/bin/env bash
# bin/kill-sandbox-marspot.sh — stop every sandbox marspot, never the real one.
#
#   bin/kill-sandbox-marspot.sh          # list what would be killed
#   bin/kill-sandbox-marspot.sh --yes    # kill them
#
# `pkill -f <sandbox-path>` does NOT work and looks like it does: the
# sandbox is selected by MARSPOT_STATE_DIR, which lives in the process
# ENVIRONMENT, not in its command line.  So the pattern matches nothing,
# pkill exits happily, and a stray marspot keeps running with the user
# none the wiser (twice in one session, 2026-09-05/06).
#
# Selection is by state dir, read from the environment via `ps eww`.
# A process whose MARSPOT_STATE_DIR is unset — the installed app — is
# never a candidate, so this cannot take the user's terminal down.
set -uo pipefail

CONFIRM="${1:-}"
found=0

for p in $(pgrep -x marspot-shell; pgrep -x marspot-core; pgrep -x marspot-session); do
  sd=$(ps eww -p "$p" 2>/dev/null | tr ' ' '\n' | grep '^MARSPOT_STATE_DIR=' | head -1)
  # Unset  → the installed app.  Leave it alone, always.
  [[ -z "$sd" ]] && continue
  dir="${sd#MARSPOT_STATE_DIR=}"
  # Only sandboxes: /tmp, /private/tmp, or an explicit *sandbox* path.
  case "$dir" in
    /tmp/*|/private/tmp/*|*sandbox*) ;;
    *) continue ;;
  esac
  found=$((found + 1))
  comm=$(ps -o comm= -p "$p" 2>/dev/null | sed 's|.*/||')
  if [[ "$CONFIRM" == "--yes" ]]; then
    kill -TERM "$p" 2>/dev/null && echo "  TERM $p  $comm  ($dir)"
  else
    echo "  would kill $p  $comm  ($dir)"
  fi
done

if (( found == 0 )); then
  echo "  no sandbox marspot running"
elif [[ "$CONFIRM" != "--yes" ]]; then
  echo "  ($found found — re-run with --yes to stop them)"
fi

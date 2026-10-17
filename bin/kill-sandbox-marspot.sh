#!/usr/bin/env bash
# bin/kill-sandbox-marspot.sh — stop sandbox marspots, never the real one.
#
#   bin/kill-sandbox-marspot.sh --state-dir DIR   # stop the processes DIR's sandbox recorded
#   bin/kill-sandbox-marspot.sh                   # list every sandbox marspot of this user
#   bin/kill-sandbox-marspot.sh --yes             # stop every one of them
#
# Scripts use `--state-dir`: it stops what that one sandbox wrote down
# (shell.pid, the session registry, dev_record_pid) and what those
# started, and leaves every other sandbox on the machine alone -- other
# sessions run theirs on the same host.
#
# The bare form is for a person cleaning up by hand, and it reaches
# every sandbox this user runs.  Selection is by state dir, read from
# the environment via `ps -E`.  A process whose MARSPOT_STATE_DIR is
# unset — the installed app — is never a candidate, so this cannot take
# the user's terminal down.  Matching a sandbox path in the command line
# would not work at all: the sandbox lives in the environment, not argv.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if [[ ${1:-} == --state-dir ]]; then
  [[ -n ${2:-} ]] || { echo "usage: $0 --state-dir DIR" >&2; exit 2; }
  export MARSPOT_STATE_DIR="$2"
  # shellcheck source=/dev/null
  source "$ROOT/bin/_dev-sandbox.sh"
  dev_kill_shell_core
  exit
fi

CONFIRM="${1:-}"
found=()

for p in $(pgrep -x marspot-shell; pgrep -x marspot-core; pgrep -x marspot-session; pgrep -x marspot; pgrep -x mcli); do
  sd=$(ps -ww -E -o command= -p "$p" 2>/dev/null | tr ' ' '\n' | grep '^MARSPOT_STATE_DIR=' | head -1)
  # Unset  → the installed app.  Leave it alone, always.
  [[ -z "$sd" ]] && continue
  dir="${sd#MARSPOT_STATE_DIR=}"
  # Only sandboxes: /tmp, /private/tmp, or an explicit *sandbox* path.
  case "$dir" in
    /tmp/*|/private/tmp/*|*sandbox*) ;;
    *) continue ;;
  esac
  found+=("$p")
  comm=$(ps -o comm= -p "$p" 2>/dev/null | sed 's|.*/||')
  if [[ "$CONFIRM" == "--yes" ]]; then
    echo "  stopping $p  $comm  ($dir)"
  else
    echo "  would stop $p  $comm  ($dir)"
  fi
done

if (( ${#found[@]} == 0 )); then
  echo "  no sandbox marspot running"
  exit 0
fi
if [[ "$CONFIRM" != "--yes" ]]; then
  echo "  (${#found[@]} found — re-run with --yes to stop them)"
  exit 0
fi

kill -TERM "${found[@]}" 2>/dev/null
for _ in $(seq 1 25); do
  kill -0 "${found[@]}" 2>/dev/null || exit 0
  sleep 0.2
done
kill -KILL "${found[@]}" 2>/dev/null
sleep 0.5
left=()
for p in "${found[@]}"; do kill -0 "$p" 2>/dev/null && left+=("$p"); done
if (( ${#left[@]} > 0 )); then
  echo "  still running after KILL: ${left[*]}" >&2
  exit 1
fi

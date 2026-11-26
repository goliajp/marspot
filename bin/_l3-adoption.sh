#!/usr/bin/env bash
#
# Did the L3s take the staged session binary?
#
# Sourced by install-local.sh, which sets TREE and STATE_ROOT.  Run
# directly, it answers for one registry entry without an installed app:
#
#   bin/_l3-adoption.sh needs-update <entry.toml> <session-binary>
#
# prints `update` (exit 0) when the L3 that wrote the entry is not on
# the binary's build, `current` (exit 1) when it is or when the binary
# carries no readable build.

# The build a binary file is: its `MARSPOT_FP=<sha>|<ts>` marker without
# the `|END`, the form the L3 writes into its entry's `build`.  The
# first well-formed marker, as the binaries themselves read it: the
# bare needle can sit ahead of it in rodata.
binary_build() {
  [[ -f "$1" ]] || return 0
  LC_ALL=C grep -aoE 'MARSPOT_FP=[[:graph:]]{1,240}\|END' "$1" 2>/dev/null \
    | sed -E 's/\|END.*$//' | head -1
}

# entry_field ENTRY KEY: the value of `KEY = value` in an entry.toml,
# unquoted.  A build string holds neither quotes nor backslashes.
entry_field() {
  sed -nE "s/^$2 = \"?([^\"]*)\"?\$/\1/p" "$1" 2>/dev/null | head -1
}

# needs_update ENTRY STAGED: true when the L3 behind ENTRY is not on the
# STAGED build -- the cases in which the core asks it to move
# (`how_to_update`): another build, or no build at all (an L3 from
# before the field).  Nothing readable staged means nothing to move to.
needs_update() {
  local staged="$2" build
  [[ -n "$staged" ]] || return 1
  build="$(entry_field "$1" build)"
  [[ "$build" != "$staged" ]]
}

# The pids that were running before the fanout, recorded so the check
# afterwards can tell "adopted" from "gone".
BEFORE_L3_PIDS=""
# Every L3, by process name. `-a` because pgrep leaves out its own
# ancestors by default, and the L3 of the pane this script runs in is
# one: that pane was never signalled, and never counted, so it stayed
# on an old image through every install run from it while the check
# reported every pane up to date. `-x` rather than `-f`: with ancestors
# included, a shell whose command line merely mentions marspot-session
# would match too.
l3_pids() { pgrep -a -x marspot-session 2>/dev/null; }
# only this app's L3s: a sandbox L3 exiting during the wait is not a lost pane
snapshot_l3_pids() {
  local p
  BEFORE_L3_PIDS=""
  for p in $(l3_pids | sort); do
    [[ -n "$(entry_of_pid "$p")" ]] && BEFORE_L3_PIDS="$BEFORE_L3_PIDS$p "
  done
  return 0
}

# entry_of_pid PID: this app's newest registry entry written by PID;
# empty for an L3 of another state dir (a sandbox's).
entry_of_pid() {
  local e best=""
  while IFS= read -r e; do
    [[ -z "$best" || "$e" -nt "$best" ]] && best="$e"
  done < <(grep -lx "pid = $1" "$STATE_ROOT"/sessions/*/entry.toml 2>/dev/null)
  echo "$best"
}

# Decided by build, as the core decides whom to ask: a copy of the same
# build in a new inode is the same image.  Comparing inodes reported
# panes on the staged build as stale.
verify_l3_adoption() {
  local staged stale=0 total=0 gone=0 unentered=0 p e
  staged="$(binary_build "$TREE/current/marspot-session")"
  [[ -n "$staged" ]] || return 0
  sleep 3
  local now
  now="$(l3_pids | sort | tr '\n' ' ')"
  # A pane that DIED is not in `now` at all, so counting only what is
  # still running answers "are the survivors up to date" and calls that
  # success.  On 2026-09-07 that printed "all 8 panes are on this
  # image" directly after five of them had been killed mid-execv.  The
  # set that was there before the signal is the only honest denominator.
  for p in $BEFORE_L3_PIDS; do
    case " $now " in
      *" $p "*) ;;
      *) gone=$((gone+1)); echo "    LOST: pid $p did not come back" >&2 ;;
    esac
  done
  for p in $(l3_pids); do
    e="$(entry_of_pid "$p")"
    if [[ -z "$e" ]]; then
      case " $BEFORE_L3_PIDS " in *" $p "*) unentered=$((unentered+1)) ;; esac
      continue
    fi
    total=$((total+1))
    if needs_update "$e" "$staged"; then stale=$((stale+1)); fi
  done
  if (( unentered > 0 )); then
    echo "    $unentered L3(s) without an entry in this app's state dir (not checked)" >&2
  fi
  if (( gone > 0 )); then
    echo "    FAILED: $gone pane(s) died across the update; $total still running" >&2
    echo "            (their content is in sessions/<id>/bytelog — see" >&2
    echo "             examples/rebuild_scrollback_from_bytelog)" >&2
    return 1
  fi
  if (( stale > 0 )); then
    # Not asked again from here: an older core, after a rollback, would
    # answer with SIGTERM to every L3, and one already on the new build
    # takes that as "end".  Every core boot asks the L3s not on the
    # build, so these move at the next one.
    echo "    WARN: $stale/$total panes are still on the previous image" >&2
    echo "          (see l3.execv.probe_* in the log; the next core boot asks again)" >&2
  else
    echo "    all $total panes are on this image, none lost"
  fi
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  set -euo pipefail
  if [[ "${1:-}" == entry-of-pid && $# -eq 3 ]]; then
    STATE_ROOT="$2"; entry_of_pid "$3"; exit 0
  fi
  [[ "${1:-}" == needs-update && $# -eq 3 ]] \
    || { echo "usage: $0 needs-update <entry.toml> <session-binary> | entry-of-pid <state-root> <pid>" >&2; exit 2; }
  if needs_update "$2" "$(binary_build "$3")"; then echo update; exit 0; fi
  echo current; exit 1
fi

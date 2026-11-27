#!/bin/bash
# bin/_e2e-suite-clean.sh — did a window suite leave anything behind on
# the runner?  Run inside the host's lock, in the console login session,
# once before the suite and once after its sandbox was stopped.
#
#   snap BEFORE [--lock-pid N] [--live CACHE] [--probe DIR]
#       first the Dock probe (bin/_dock-probe.sh, built once in DIR): the
#       index the Dock puts a new tile at, and the Dock's pid, a second
#       after the Dock logged the probe's tile removed; then the
#       Foreground record count, the Dock's kept and recent tiles, the
#       marspot records already listed, the other holders of the lock
#       and this holder's command (N is this suite's holder) and the start
#       time, written to BEFORE.  Also proves the Dock and tccd log
#       queries still match anything (24 h back); a positive answer is
#       kept in CACHE for the rest of the run.
#   check BEFORE RESULT [--suite-rc N] [--lock-pid N] [--probe DIR] [--known PID:PATH]... PREFIX...
#       after the suite, with records given up to ten seconds to go:
#       - the Dock probe, run again, lands where it did before under the
#         same Dock process: a larger index is a tile the suite left, also
#         one the Dock kept after its app went, which is in no app list
#         (when larger, it is read once more five seconds later, for a
#         tile the Dock had not dropped yet).  Next to another job an
#         equal index passes only when every insert in the suite is named
#         for an app and the removals match the inserts; and the Dock held at most
#         two tiles of the suite at once (its insert indexes in the suite
#         against the probe's before it)
#       - the Foreground count is what BEFORE says, and no kept or recent
#         tile was added (nor a kept one gone)
#       - lsappinfo lists no marspot executable or bundle that was not
#         there before (or --known)
#       - bin/_ls-leftovers.sh finds nothing under the PREFIXes
#       - tccd logged no request from a marspot build or msp- scratch
#       When another job held the lock at any time in the suite, only the
#       conditions on this run's PREFIXes (and marspot names) are judged;
#       the rest is shown.  When the lock status or log could not be read
#       and no other holder was seen, the rest counts as not read.
#       RESULT gets suite_rc first (so a check stopped half way still
#       says it), then the raw counts, what was found and a last line
#       clean=0 (nothing left), 1 (something left) or 2 (could not read);
#       the exit status is the same number.
#       A tile left behind stays until the Dock restarts.  The check never
#       restarts it: it says so (`restore:`), and devops restores the
#       runner's console.
#
# E2E_CLEAN_INPUTS=DIR reads saved inputs from DIR instead of the live
# sources (lsappinfo.txt, probe-start.txt, probe-end.txt, persistent.txt, recent.txt, dock.log,
# tcc.ndjson, status.txt, lock.log, probe-dock.log, probe-tcc.ndjson; a
# missing file is a source that could not be read).
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
mode=${1:?usage: snap BEFORE ... | check BEFORE RESULT ...}
before=${2:?before file}
# with loginwindow's line naming each app launched: whose tile the Dock inserts next
DOCK_PRED='(process == "Dock" AND subsystem == "com.apple.dock" AND category == "dock-tiles") OR (process == "loginwindow" AND eventMessage CONTAINS "Got App URL")'
TCC_PRED='process == "tccd" AND eventMessage BEGINSWITH "AUTHREQ_"'
LOCK_LOG=/Users/Shared/bench-lock.log

# grab NAME CMD...: $w/NAME holds CMD's output, and is absent when CMD failed
grab() {
  local n=$1
  shift
  rm -f "${w:?}/${n:?}"
  if [[ -n ${E2E_CLEAN_INPUTS:-} ]]; then
    [[ ! -f $E2E_CLEAN_INPUTS/$n ]] || cp "$E2E_CLEAN_INPUTS/$n" "$w/$n"
    return 0
  fi
  "$@" >"$w/$n" 2>/dev/null || rm -f "${w:?}/${n:?}"
}
# an absent key is an empty list; an unreadable domain is not read
dock_key() {
  /usr/bin/defaults read com.apple.dock >/dev/null 2>&1 || return 1
  /usr/bin/defaults read com.apple.dock "$1" 2>/dev/null
  return 0
}
logwin() { /usr/bin/log show --start "$start" --end "$(date -v+1S '+%Y-%m-%d %H:%M:%S')" "$@"; }
# empty output is a dead query, which the decision reads as such
tcc_probe() { /usr/bin/log show --last 24h --style ndjson --predicate "$TCC_PRED" | head -n 200; return 0; }
# from a few seconds before the snap (LOOKBACK in _e2e-suite-clean.py):
# our own take is logged just before it
lock_lines() { /usr/bin/awk -F'\t' -v s="$start_epoch" '$1 >= s - 5' "$LOCK_LOG"; }
# the Dock probe's answer, `unread=why` included
run_probe() {
  local exe
  [[ -n $probe_dir ]] || { echo "unread=no probe directory given"; return 0; }
  # shellcheck source=bin/_dock-probe.sh
  source "$here/_dock-probe.sh"
  exe="$(dock_probe_build "$probe_dir" 2>/dev/null)" || { echo "unread=the probe could not be built"; return 0; }
  dock_probe "$exe"
  return 0
}
# the probe's tile gone before the suite's first tile comes: dock_probe returns once the Dock
# logged a removal line after it, and the suite starts a second after that (a suite tile
# logged before that removal line makes the tiles-at-once count unread)
start_probe() {
  run_probe
  sleep 1
}
# a tile of the suite the Dock had not dropped yet is gone five seconds later; a kept one is not
end_probe() {
  local out first was
  out="$(run_probe)"
  first="$(sed -n 's/^index=//p' <<<"$out")"
  was="$(sed -n 's/^probe_index=//p' "$before" 2>/dev/null)"
  if [[ -n $first && -n $was ]] && (( first > was )); then
    sleep 5
    out="$(run_probe)"$'\n'"first_index=$first"
  fi
  printf '%s\n' "$out"
}
grab_state() {
  grab lsappinfo.txt /usr/bin/lsappinfo list
  grab persistent.txt dock_key persistent-apps
  grab recent.txt dock_key recent-apps
}
grab_lock() {
  grab status.txt /usr/local/bin/bench-lock status
  grab lock.log lock_lines
}

w="$(mktemp -d)"
trap 'rm -rf "${w:?}"' EXIT
lock_pid=""
live=""
probe_dir=""
if [[ $mode == snap ]]; then
  shift 2
  while [[ $# -ge 2 ]]; do
    case $1 in --lock-pid) lock_pid=$2 ;; --live) live=$2 ;; --probe) probe_dir=$2 ;; *) break ;; esac
    shift 2
  done
  start_epoch=$(date +%s)
  # first: the probe's record is gone from the state read after it
  grab probe-start.txt start_probe
  grab_state
  grab status.txt /usr/local/bin/bench-lock status
  cached=""
  if [[ -n $live && -f $live ]] && grep -qx dock_live=1 "$live" && grep -qx tcc_live=1 "$live"; then
    cached="$(cat "$live")"
  else
    grab probe-dock.log /usr/bin/log show --last 24h --style compact --predicate "$DOCK_PRED"
    grab probe-tcc.ndjson tcc_probe
  fi
  { echo "start=$(date -r "$start_epoch" '+%Y-%m-%d %H:%M:%S')"
    echo "start_epoch=$start_epoch"
    /usr/bin/python3 "$here/_e2e-suite-clean.py" snap "$w" "$lock_pid"
    [[ -z $cached ]] || echo "$cached"; } >"$before.tmp" && mv "$before.tmp" "$before"
  if [[ -n $live && -z $cached ]] && grep -qx dock_live=1 "$before" && grep -qx tcc_live=1 "$before"; then
    grep -E '^(dock|tcc)_live=' "$before" >"$live.tmp" && mv "$live.tmp" "$live"
  fi
  exit 0
fi
result=${3:?result file}
shift 3
suite_rc=""
known=""
kargs=()
while [[ $# -ge 2 ]]; do
  case $1 in
    --suite-rc) suite_rc=$2 ;;
    --lock-pid) lock_pid=$2 ;;
    --probe) probe_dir=$2 ;;
    --known) kargs+=(--known "$2"); known+="$2"$'\n' ;;
    *) break ;;
  esac
  shift 2
done
# the records listed before the suite: the decision reads them from BEFORE
while IFS= read -r k; do kargs+=(--known "$k"); done < <(sed -n 's/^known=//p' "$before" 2>/dev/null)
head=""
[[ -z $suite_rc ]] || head="suite_rc=$suite_rc"$'\n'
printf '%s' "$head" >"$result"
start="$(sed -n 's/^start=//p' "$before" 2>/dev/null)"
start_epoch="$(sed -n 's/^start_epoch=//p' "$before" 2>/dev/null)"

grab_lock
# first: the probe's record then has the ten seconds below to go too
grab probe-end.txt end_probe
deadline=$((SECONDS + 10))
while :; do
  grab_state
  [[ -z $start ]] || grab dock.log logwin --style compact --predicate "$DOCK_PRED"
  lrc=0
  lout="$("$here/_ls-leftovers.sh" --from "$w/lsappinfo.txt" ${kargs[@]+"${kargs[@]}"} "$@" 2>&1)" || lrc=$?
  out="$(CLEAN_KNOWN="$known" /usr/bin/python3 "$here/_e2e-suite-clean.py" check "$w" "$before" "$lrc" "$lout" "$lock_pid" "$@")"
  [[ ${out##*clean=} == 1 && -z ${E2E_CLEAN_INPUTS:-} && $SECONDS -lt $deadline ]] || break
  sleep 1
done
# requests are not withdrawn and the lock is read once more: both only at the end
[[ -z $start ]] || grab tcc.ndjson logwin --style ndjson --predicate "$TCC_PRED"
grab_lock
out="$head$(CLEAN_KNOWN="$known" /usr/bin/python3 "$here/_e2e-suite-clean.py" check "$w" "$before" "$lrc" "$lout" "$lock_pid" "$@")"
clean=${out##*clean=}
# the decision itself failing proves nothing either way
case $clean in 0 | 1 | 2) ;; *) out+=$'\n'"unread: the check failed"$'\n'"clean=2"; clean=2 ;; esac
printf '%s\n' "$out" >"$result.tmp" && mv "$result.tmp" "$result"
printf '%s\n' "$out"
exit "$clean"

#!/bin/bash
# bin/_e2e-suite-clean.sh — did a window suite leave anything behind on
# the runner?  Run inside the host's lock, in the console login session,
# once before the suite and once after its sandbox was stopped.
#
#   snap BEFORE
#       the Foreground record count, the Dock's kept and recent tile
#       counts and the start time of the suite, written to BEFORE
#   check BEFORE RESULT [--suite-rc N] [--known PID:PATH]... PREFIX...
#       after the suite, with records given up to ten seconds to go:
#       - every Dock tile inserted for a launch was removed again
#       - the Foreground, kept and recent counts are what BEFORE says
#       - lsappinfo lists no marspot executable or bundle (but --known)
#       - bin/_ls-leftovers.sh finds nothing under the PREFIXes
#       - tccd logged no request from a marspot build or msp- scratch
#       RESULT gets the raw counts, what was found and a last line
#       clean=0 (nothing left), 1 (something left) or 2 (could not read);
#       the exit status is the same number.
#
# E2E_CLEAN_INPUTS=DIR reads saved inputs from DIR instead of the live
# sources (lsappinfo.txt, persistent.txt, recent.txt, dock.log,
# tcc.ndjson; a missing file is a source that could not be read).
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
mode=${1:?usage: snap BEFORE | check BEFORE RESULT ...}
before=${2:?before file}

# writes a file only for a source that was read
collect() {
  local d=$1 start=${2:-} end
  if [[ -n ${E2E_CLEAN_INPUTS:-} ]]; then
    cp "$E2E_CLEAN_INPUTS"/* "$d"/ 2>/dev/null
    return 0
  fi
  /usr/bin/lsappinfo list >"$d/lsappinfo.txt" 2>/dev/null || rm -f "$d/lsappinfo.txt"
  # an absent key is an empty list; an unreadable domain is not read
  if /usr/bin/defaults read com.apple.dock >/dev/null 2>&1; then
    /usr/bin/defaults read com.apple.dock persistent-apps >"$d/persistent.txt" 2>/dev/null
    /usr/bin/defaults read com.apple.dock recent-apps >"$d/recent.txt" 2>/dev/null
  fi
  [[ -n $start ]] || return 0
  end="$(date -v+1S '+%Y-%m-%d %H:%M:%S')"
  /usr/bin/log show --start "$start" --end "$end" --style compact --predicate \
    'process == "Dock" AND subsystem == "com.apple.dock" AND category == "dock-tiles"' \
    >"$d/dock.log" 2>/dev/null || rm -f "$d/dock.log"
  /usr/bin/log show --start "$start" --end "$end" --style ndjson --predicate \
    'process == "tccd" AND eventMessage BEGINSWITH "AUTHREQ_"' \
    >"$d/tcc.ndjson" 2>/dev/null || rm -f "$d/tcc.ndjson"
}


w="$(mktemp -d)"
trap 'rm -rf "${w:?}"' EXIT
if [[ $mode == snap ]]; then
  start="$(date '+%Y-%m-%d %H:%M:%S')"
  collect "$w"
  { echo "start=$start"; /usr/bin/python3 "$here/_e2e-suite-clean.py" snap "$w"; } >"$before.tmp" && mv "$before.tmp" "$before"
  exit 0
fi
result=${3:?result file}
shift 3
suite_rc=""
[[ ${1:-} == --suite-rc ]] && { suite_rc=$2; shift 2; }
known=""
kargs=()
while [[ ${1:-} == --known && $# -ge 2 ]]; do known+="$2"$'\n'; kargs+=(--known "$2"); shift 2; done
start="$(sed -n 's/^start=//p' "$before" 2>/dev/null)"
deadline=$((SECONDS + 10))
while :; do
  rm -f "$w"/*
  collect "$w" "${start:-}"
  lrc=0
  lout="$("$here/_ls-leftovers.sh" --from "$w/lsappinfo.txt" ${kargs[@]+"${kargs[@]}"} "$@" 2>&1)" || lrc=$?
  out="$( { [[ -n $suite_rc ]] && echo "suite_rc=$suite_rc"
    CLEAN_KNOWN="$known" /usr/bin/python3 "$here/_e2e-suite-clean.py" check "$w" "$before" "$lrc" "$lout"; } )"
  clean=${out##*clean=}
  # the decision itself failing proves nothing either way
  case $clean in 0 | 1 | 2) ;; *) out+=$'\n'"unread: the check failed"$'\n'"clean=2"; clean=2 ;; esac
  [[ $clean == 1 && -z ${E2E_CLEAN_INPUTS:-} && $SECONDS -lt $deadline ]] || break
  sleep 1
done
printf '%s\n' "$out" >"$result.tmp" && mv "$result.tmp" "$result"
printf '%s\n' "$out"
exit "$clean"

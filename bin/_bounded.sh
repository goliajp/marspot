#!/usr/bin/env bash
# bin/_bounded.sh SECS CMD [ARGS...] — run CMD, and end it if it is still
# running after SECS seconds.
#
# macOS has no timeout(1).  CMD runs in a process group of its own, so
# on timeout (or when this script is told to stop) the whole tree CMD
# started goes with it: TERM, ten seconds, then KILL.  Exit status is
# CMD's, 124 on timeout, 143 when stopped by a signal.
#
# What CMD started in another process group or session (an L3 calls
# setsid) is out of reach here; the caller's sandbox cleanup owns those.
set -u

secs=$1
shift
[[ $secs =~ ^[0-9]+$ ]] || { echo "_bounded.sh: SECS must be a number, got '$secs'" >&2; exit 2; }

set -m
"$@" &
child=$!
set +m

stop_group() {
  kill -TERM -- "-$child" 2>/dev/null || return 0
  local i
  for ((i = 0; i < 20; i++)); do
    kill -0 -- "-$child" 2>/dev/null || return 0
    sleep 0.5
  done
  echo "_bounded.sh: process group $child ignored TERM; sending KILL" >&2
  kill -KILL -- "-$child" 2>/dev/null || true
}

trap 'stop_group; exit 143' TERM INT HUP

deadline=$(( $(date +%s) + secs ))
while kill -0 "$child" 2>/dev/null; do
  if (( $(date +%s) >= deadline )); then
    echo "_bounded.sh: '$*' still running after ${secs}s; stopping it" >&2
    stop_group
    wait "$child" 2>/dev/null
    exit 124
  fi
  # a background sleep so a trapped signal is handled within a second
  sleep 1 &
  wait $! 2>/dev/null
done
wait "$child"

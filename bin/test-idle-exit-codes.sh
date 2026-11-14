#!/usr/bin/env bash
# bin/test-idle-exit-codes.sh — the idle measurement's four exit codes
# are each reachable and each mean one thing:
#
#   0  measured, and within the budget when one was given
#   1  measured on a quiet host, and over the budget
#   2  the instrument failed: no reading
#   3  not measured: the host was busy
#
# bin/_idle_wakeups.py is run over two-second windows against this
# script's own shell, the one process whose command line carries the
# token made below, with the budget and the busy line set so that each
# code has to come out whatever the host is doing.  Nothing is started
# besides the instrument itself and nothing is signalled; no window is
# opened.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Run again with a token on the command line, so the instrument's
# pattern finds this shell and no other process of the run.
if [[ ${1:-} != --token ]]; then
  exec bash "$0" --token "idle-exit-codes-$$-$RANDOM"
fi
TOKEN="$2"
WAKEUPS="$ROOT/bin/_idle_wakeups.py"
failed=0

# expect CODE WHAT COMMAND...
expect() {
  local want=$1 what=$2
  shift 2
  "$@" >/tmp/idle-exit-codes.$$.out 2>&1
  local got=$?
  if [[ $got == "$want" ]]; then
    echo "ok    exit $got  $what"
  else
    echo "FAIL  exit $got, expected $want  $what"
    sed 's/^/        /' /tmp/idle-exit-codes.$$.out
    failed=1
  fi
}
trap 'rm -f /tmp/idle-exit-codes.$$.out' EXIT

own=(--pattern "$TOKEN" --window 2 --readings 1)
# a line no load is over, and one every load is over
quiet=(--busy-load 1000000)
busy=(--busy-load -1)
# a budget nothing is over, and one everything is over
roomy=(--budget 1000000)
none=(--budget -1)

expect 0 "within its budget on a quiet host" "$WAKEUPS" "${own[@]}" "${roomy[@]}" "${quiet[@]}"
expect 0 "within its budget on a busy host: company cannot make a reading better" \
  "$WAKEUPS" "${own[@]}" "${roomy[@]}" "${busy[@]}"
expect 0 "no budget, on a quiet host" "$WAKEUPS" "${own[@]}" "${quiet[@]}"
expect 1 "over its budget on a quiet host" "$WAKEUPS" "${own[@]}" "${none[@]}" "${quiet[@]}"
expect 3 "over its budget on a busy host: no verdict" "$WAKEUPS" "${own[@]}" "${none[@]}" "${busy[@]}"
expect 3 "no budget, on a busy host" "$WAKEUPS" "${own[@]}" "${busy[@]}"
expect 3 "the busy line comes from IDLE_BUSY_LOAD" env IDLE_BUSY_LOAD=-1 "$WAKEUPS" "${own[@]}" "${none[@]}"
expect 2 "no process matches" "$WAKEUPS" --pattern "$TOKEN-nobody" --window 2 --readings 1 "${roomy[@]}"
expect 2 "bad usage" "$WAKEUPS" --pattern "$TOKEN" --window soon
expect 2 "measure-idle.sh: bad usage" "$ROOT/bin/measure-idle.sh" --app nothing-by-this-name --sessions 1
expect 2 "measure-idle.sh: no --sessions" "$ROOT/bin/measure-idle.sh" --app marspot-dev

if (( failed )); then
  echo "FAIL: an exit code did not mean what it is documented to mean"
  exit 1
fi
echo "PASS: 0, 1, 2 and 3 each came out where they should"

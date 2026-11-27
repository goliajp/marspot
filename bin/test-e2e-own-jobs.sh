#!/usr/bin/env bash
# bin/test-e2e-own-jobs.sh — a window suite does not share the runner with
# another job of ours, and runs with the Dock tile maximum its file
# declares:
#
#   jobs      : bin/_e2e-own-jobs.py tells our holders of the lock (by the
#               run's prefix, a remote copy under ~/work named for marspot
#               in the command, a run's note, or the lock log's directory
#               of that take) from other projects' and from the asking one
#   own-wait  : a job of ours holds the lock all along: the driver waits
#               up to E2E_MAX_WAIT, starts no suite and says why
#   own-gone  : a job of ours holds it at first, then only another
#               project's: the driver waits, then starts the suite with
#               the maximum its file declares
#   own-inlock: one takes the lock between the driver's look and the
#               suite's hold: the hold starts nothing (no snapshot), and
#               the driver waits again and then stops
#   malformed : a suite whose declaration is malformed is refused before
#               anything is copied
#
# Runs on this machine in seconds with the stand-ins of
# bin/_e2e-driver-stubs.sh; reaches no runner and launches nothing.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d /tmp/marspot-e2e-own.XXXXXX)"
NAME="own-test-$$"
LOCK="/tmp/marspot-e2e-remote.$NAME.lock.d"
drv=""
failed=0
# shellcheck source=bin/_e2e-driver-stubs.sh
source "$ROOT/bin/_e2e-driver-stubs.sh"
trap cleanup EXIT

SINCE="2026-11-28 10:00:00"
AT="$(date -j -f '%Y-%m-%d %H:%M:%S' "$SINCE" +%s)"
# a test run of ours: its command names no copy, its take's directory does
OURS=$'9999\theavy\tholding since '"$SINCE"$'\tbin/_bounded.sh 600 bin/test.sh -E x'
OURS_TAKE="$AT"$'\theavy\t0\ttook\tbin/_bounded.sh 600 bin/test.sh -E x\t/Users/u/work/test-marspot'
FOREIGN=$'8888\theavy\tholding since '"$SINCE"$'\tcargo build --release'
FOREIGN_TAKE="$AT"$'\theavy\t0\ttook\tcargo build --release\t/Users/u/work/torajs'

echo "jobs: which holders are ours"
J="$T/jobs"
mkdir -p "$J"
printf '#!/bin/bash\n[[ -e %s/fail ]] && exit 1\ncat %s/status\n' "$J" "$J" >"$J/bench-lock"
chmod +x "$J/bench-lock"
jobs() { # name, expected exit, text the output must have ("" for none), status, then arguments
  local name=$1 want=$2 has=$3 got=0 out
  printf '%s\n' "$4" >"$J/status"
  shift 4
  out="$(E2E_BENCH_LOCK="$J/bench-lock" E2E_LOCK_LOG="${LOG:-$J/lock.log}" /usr/bin/python3 "$ROOT/bin/_e2e-own-jobs.py" "$@" 2>&1)" \
    || got=$?
  # with nothing wanted, nothing but a note
  if [[ $got == "$want" && ( -z $has && $out != *ours:* && $out != *unread:* || -n $has && $out == *"$has"* ) ]]; then
    echo "  ok    $name"
  else echo "  FAIL  $name: exit $got (wanted $want), printed: $out (wanted: ${has:-nothing})"; failed=1; fi
}
printf '%s\n%s\n' "$FOREIGN_TAKE" "$OURS_TAKE" >"$J/lock.log"
jobs by-prefix 1 "ours: pid=7777" $'7777\theavy\tholding since '"$SINCE"$'\tsh -c stopped=; bin/_e2e-suite-clean.sh snap /tmp/msp-e2e-1-1.before'
jobs self-left-out 0 "" $'7777\theavy\tholding since '"$SINCE"$'\tsh -c bin/_e2e-suite-clean.sh snap /tmp/msp-e2e-1-1.before' --self 7777
jobs by-copy-in-command 1 "ours: pid=7776" $'7776\tbench\tholding since '"$SINCE"$'\tsh -c cd /Users/u/work/bench-marspot && bin/bench.sh'
jobs by-note 1 "ours: pid=7775" $'7775\theavy\tholding since '"$SINCE"', until 2026-11-28 11:00 marspot e2e e2e-marspot-x'$'\tbin/_build_then.sh'
jobs by-take-directory 1 "(asked from /Users/u/work/test-marspot)" "$OURS"$'\n'"$FOREIGN"
jobs stopped 1 "ours: pid=9999" "$OURS [stopped]"
jobs foreign 0 "" "$FOREIGN"$'\n'$'5555\theavy\twaiting since '"$SINCE"$'\tsh -c /tmp/msp-e2e-2-1'
printf '%s\n' "$(( AT - 60 ))"$'\theavy\t0\ttook\tbin/_bounded.sh 600 bin/test.sh -E x\t/Users/u/work/test-marspot' >"$J/lock.log"
jobs take-at-another-time 0 "" "$OURS"
LOG="$J/none" jobs log-unread 0 "" "$OURS"
LOG="$J/none" jobs log-unread-said 0 "note: lock log $J/none unread" "$OURS"
: >"$J/fail"
jobs status-unread 2 "unread: bench-lock status exited 1" "$OURS"

# $1 scenario; runs the driver to its end with E2E_MAX_WAIT=2, sets rc
run_driver() {
  local S="$T/$1"
  rc=0
  PATH="$S/stubs:$PATH" HOST=stub.invalid REMOTE_NAME="$NAME" E2E_MAX_WAIT=2 timeout 60 \
    "$S/root/bin/test-e2e-remote.sh" test-first.sh test-second.sh >"$S/driver.log" 2>&1 || rc=$?
}

echo "own-wait: a job of ours holds the lock all along"
before_case=$failed
setup own-wait
S="$T/own-wait"
printf '%s\n%s\n' "$OURS" "$FOREIGN" >"$S/status"
printf '%s\n%s\n' "$FOREIGN_TAKE" "$OURS_TAKE" >"$S/lock.log"
t0=$SECONDS
run_driver own-wait
check "exit 1 (got $rc)" test "$rc" -eq 1
check "it waited about E2E_MAX_WAIT ($((SECONDS - t0)) s)" test $((SECONDS - t0)) -lt 20
check "it names our job" grep -q 'ours: pid=9999 heavy' "$S/driver.log"
check "it says why the suite did not start" grep -q "FAIL: test-first.sh not started: a job of ours held" "$S/driver.log"
check "the lock was never asked for" bash -c "! grep -q 'bench-lock holding' '$S/events' 2>/dev/null"
check "the second suite was never started" bash -c "! grep -q 'test-second.sh' '$S/ssh.log'"
check "the local lock was removed" test ! -e "$LOCK"
((failed > before_case)) && dump own-wait

echo "own-gone: ours goes, another project's stays"
before_case=$failed
setup own-gone
S="$T/own-gone"
printf '%s\n%s\n' "$OURS" "$FOREIGN" >"$S/status.1"
printf '%s\n' "$FOREIGN" >"$S/status"
printf '%s\n%s\n' "$FOREIGN_TAKE" "$OURS_TAKE" >"$S/lock.log"
printf '#!/bin/bash\n# e2e-dock-tiles-max: 4  app 2 + probe 1 + instance 1\n' >"$S/root/bin/test-first.sh"
run_driver own-gone
check "it waited for our job" grep -q '==> waiting (up to' "$S/driver.log"
check "then the suite ran in its hold" grep -q 'suite-clean check held=1' "$S/events"
check "the hold looked once more (status read $(cat "$S/status.n") times)" test "$(cat "$S/status.n")" -ge 3
# (the command reaches ssh quoted for bash -lc: spaces escaped)
check "the check got the declared maximum" grep -qE -- '--dock-max(\\)? 4(\\)? --suite-rc' "$S/ssh.log"
((failed > before_case)) && dump own-gone

echo "own-inlock: ours takes the lock between the driver's look and the hold"
before_case=$failed
setup own-inlock
S="$T/own-inlock"
printf '%s\n' "$FOREIGN" >"$S/status.1"
printf '%s\n%s\n' "$OURS" "$FOREIGN" >"$S/status"
printf '%s\n%s\n' "$FOREIGN_TAKE" "$OURS_TAKE" >"$S/lock.log"
run_driver own-inlock
check "exit 1 (got $rc)" test "$rc" -eq 1
check "the hold was taken" grep -q 'bench-lock holding' "$S/events"
check "nothing ran in it (no snapshot)" bash -c "! grep -q 'suite-clean' '$S/events'"
check "the driver waited again" grep -q "a job of ours took stub.invalid's lock first" "$S/driver.log"
check "then stopped" grep -q "FAIL: test-first.sh not started" "$S/driver.log"
check "the second suite was never started" bash -c "! grep -q 'test-second.sh' '$S/ssh.log'"
((failed > before_case)) && dump own-inlock

echo "malformed: a suite's declaration that cannot be read"
before_case=$failed
setup malformed
S="$T/malformed"
printf "#!/bin/bash\ntouch %s/synced\n" "$S" >"$S/stubs/remote-sync"
run_driver malformed
check "exit 2 (got $rc)" test "$rc" -eq 2
check "it says what is malformed" grep -q "test-first.sh: malformed: '# e2e-dock-tiles-max: x'" "$S/driver.log"
check "nothing was copied" test ! -e "$S/synced"
((failed > before_case)) && dump malformed

((failed)) && exit 1
echo "PASS: test-e2e-own-jobs"

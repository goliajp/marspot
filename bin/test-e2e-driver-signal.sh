#!/usr/bin/env bash
# bin/test-e2e-driver-signal.sh — bin/test-e2e-remote.sh told to stop
# exits 130 within seconds, starts no other suite and removes its local
# lock, in the two places it waits longest:
#
#   suite : during the first suite.  The remote process group is stopped,
#           the suite's sandbox reaped, and on the far side the cleanup
#           inside the host's lock runs before bench-lock lets go.
#   sync  : during the copy to the host.  The copy's whole process group
#           (the sync tool and what it started) is stopped.
#
# and, not told to stop, that a suite whose check inside the lock finds
# something left fails, prints what was found and starts no other suite
# (left).  A stopped run skips that check but not the cleanup.  A host
# whose ssh login cannot see the console session stops the run before
# the copy, with what lsappinfo printed (noconsole).
#
# Runs on this machine in a few seconds and reaches no runner: ssh, scp,
# remote-sync and bench-lock are stand-ins.  The ssh stand-in runs
# the suite's command and the stop command locally, each in a process
# group of its own as sshd would, with the "remote" tree under a scratch
# HOME and /tmp/ moved into the scratch directory; every other remote
# call is only written down.  Nothing is built and no app is launched.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d /tmp/marspot-e2e-signal.XXXXXX)"
NAME="signal-test-$$"
LOCK="/tmp/marspot-e2e-remote.$NAME.lock.d"
drv=""
failed=0

# shellcheck source=bin/_e2e-driver-stubs.sh
source "$ROOT/bin/_e2e-driver-stubs.sh"
trap cleanup EXIT

# $1 scenario, $2 the file whose appearance says the driver is where it
# should be stopped.  Sets rc to the driver's exit status.
run_until_term() {
  local S="$T/$1" i
  PATH="$S/stubs:$PATH" HOST=stub.invalid REMOTE_NAME="$NAME" \
    "$S/root/bin/test-e2e-remote.sh" test-first.sh test-second.sh >"$S/driver.log" 2>&1 &
  drv=$!
  for ((i = 0; i < 100; i++)); do
    [[ -f $S/$2 ]] && break
    sleep 0.1
  done
  [[ -f $S/$2 ]] || { echo "FAIL: $1: the driver never got to $2"; cat "$S/driver.log"; exit 1; }
  kill -TERM "$drv"
  for ((i = 0; i < 100; i++)); do
    kill -0 "$drv" 2>/dev/null || break
    sleep 0.1
  done
  if kill -0 "$drv" 2>/dev/null; then
    echo "FAIL: $1: the driver still runs 10s after TERM"
    exit 1
  fi
  rc=0
  wait "$drv" || rc=$?
  drv=""
}

echo "suite: TERM while the first suite runs"
setup suite
run_until_term suite suite.pid
S="$T/suite"
# the far side finishes on its own after the driver is gone
for ((i = 0; i < 150; i++)); do
  grep -q 'bench-lock released' "$S/events" 2>/dev/null && break
  sleep 0.1
done
check "exit 130 (got $rc)" test "$rc" -eq 130
check "the second suite was never started" bash -c "! grep -q 'test-second.sh' '$S/ssh.log'"
check "the remote process group was told to stop" grep -q 'kill -TERM -- -.*marspot-e2e-.*\.pgid' "$S/ssh.log"
check "the suite's sandbox was reaped" grep -qE 'kill-sandbox-marspot.sh --state-dir /tmp/msp-e2e-[0-9]+-1$' "$S/ssh.log"
check "the suite was stopped" gone "$(cat "$S/suite.pid")"
check "the sandbox was cleaned inside the lock" grep -q 'kill-sandbox held=1' "$S/events"
check "bench-lock let go only after that cleanup" before "$S/events" 'kill-sandbox held=1' 'bench-lock released'
check "the before snapshot was taken inside the lock" grep -q 'suite-clean snap held=1' "$S/events"
check "the stopped run skipped the check" bash -c "! grep -q 'suite-clean check' '$S/events'"
check "the local lock was removed" test ! -e "$LOCK"
((failed)) && dump suite

echo "snap: TERM while the before snapshot is taken"
before_snap=$failed
setup snap
run_until_term snap snap.pid
S="$T/snap"
for ((i = 0; i < 150; i++)); do
  grep -q 'bench-lock released' "$S/events" 2>/dev/null && break
  sleep 0.1
done
check "exit 130 (got $rc)" test "$rc" -eq 130
check "the snapshot was stopped" gone "$(cat "$S/snap.pid")"
check "the suite was never started" test ! -e "$S/suite.pid"
check "the sandbox was cleaned inside the lock" grep -q 'kill-sandbox held=1' "$S/events"
check "bench-lock let go only after that cleanup" before "$S/events" 'kill-sandbox held=1' 'bench-lock released'
check "the stopped run skipped the check" bash -c "! grep -q 'suite-clean check' '$S/events'"
check "the second suite was never started" bash -c "! grep -q 'test-second.sh' '$S/ssh.log'"
check "the local lock was removed" test ! -e "$LOCK"
((failed > before_snap)) && dump snap

echo "left: the first suite passes but leaves a record"
before_left=$failed
setup left
S="$T/left"
rc=0
PATH="$S/stubs:$PATH" HOST=stub.invalid REMOTE_NAME="$NAME" timeout 60 \
  "$S/root/bin/test-e2e-remote.sh" test-first.sh test-second.sh >"$S/driver.log" 2>&1 || rc=$?
check "exit 1 (got $rc)" test "$rc" -eq 1
check "the check ran inside the lock, after the cleanup" before "$S/events" 'kill-sandbox held=1' 'suite-clean check held=1'
check "bench-lock let go only after the check" before "$S/events" 'suite-clean check held=1' 'bench-lock released'
check "what was left is printed" grep -q 'left: record: Foreground pid=1' "$S/driver.log"
check "the suite failed for it" grep -q 'FAIL: test-first.sh: passed, but left something' "$S/driver.log"
check "the second suite was never started" bash -c "! grep -q 'test-second.sh' '$S/ssh.log'"
((failed > before_left)) && dump left

echo "sync: TERM while the tree is copied to the host"
before_sync=$failed
setup sync
run_until_term sync sync.pid
S="$T/sync"
check "exit 130 (got $rc)" test "$rc" -eq 130
check "no suite was started" bash -c "! grep -q 'bench-lock' '$S/ssh.log' 2>/dev/null"
check "the sync tool was stopped" gone "$(cat "$S/sync.pid")"
check "what the sync tool started was stopped" gone "$(cat "$S/sync-child.pid")"
check "the local lock was removed" test ! -e "$LOCK"
((failed > before_sync)) && dump sync

echo "noconsole: the ssh login does not see the console session"
before_nc=$failed
setup noconsole
S="$T/noconsole"
: >"$S/noconsole"
printf "#!/bin/bash\ntouch %s/synced\n" "$S" >"$S/stubs/remote-sync"
rc=0
PATH="$S/stubs:$PATH" HOST=stub.invalid REMOTE_NAME="$NAME" timeout 60 \
  "$S/root/bin/test-e2e-remote.sh" test-first.sh >"$S/driver.log" 2>&1 || rc=$?
check "exit 1 (got $rc)" test "$rc" -eq 1
check "it names the command and what it printed" \
  bash -c "grep -q 'lsappinfo list.*lists no Dock' '$S/driver.log' && grep -q 'no console here' '$S/driver.log'"
check "nothing was copied" test ! -e "$S/synced"
check "no suite was started" bash -c "! grep -q bench-lock '$S/ssh.log'"
check "the local lock was removed" test ! -e "$LOCK"
((failed > before_nc)) && dump noconsole

if ((failed)); then
  exit 1
fi
echo "PASS: test-e2e-driver-signal"

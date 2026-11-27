#!/usr/bin/env bash
# bin/test-e2e-driver-signal.sh — bin/test-e2e-remote.sh told to stop
# while its first suite runs: it exits 130, starts no other suite, stops
# the remote process group, reaps the suite's sandbox and removes its
# local lock.
#
# Runs on this machine in a few seconds and reaches no runner: ssh, scp
# and remote-sync are stand-ins on PATH that write down what they were
# asked, and the one asked to run a suite waits until it is stopped.
# The driver and its helpers are copied into a scratch git repository so
# bin/_git-sha.sh has a commit to name; nothing is built or launched.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d /tmp/marspot-e2e-signal.XXXXXX)"
NAME="signal-test-$$"
LOCK="/tmp/marspot-e2e-remote.$NAME.lock.d"
drv=""

cleanup() {
  local p
  [[ -n $drv ]] && kill -TERM "$drv" 2>/dev/null
  if [[ -f $T/suite.pid ]]; then
    p="$(cat "$T/suite.pid")"
    kill -TERM "$p" 2>/dev/null || true
  fi
  rm -rf "${T:?}" "${LOCK:?}"
}
trap cleanup EXIT

mkdir -p "$T/root/bin" "$T/stubs"
cp "$ROOT/bin/test-e2e-remote.sh" "$ROOT/bin/_e2e-remote-lib.sh" "$ROOT/bin/_git-sha.sh" "$T/root/bin/"
: >"$T/root/bin/test-first.sh"
: >"$T/root/bin/test-second.sh"
git -C "$T/root" init -q
git -C "$T/root" add -A
git -C "$T/root" -c user.name=t -c user.email=t@localhost commit -qm t

cat >"$T/stubs/ssh" <<EOF
#!/bin/bash
printf '%s\n' "\$*" >>"$T/ssh.log"
case "\$*" in
  *bench-lock*bin/test-first.sh*)
    echo \$\$ >"$T/suite.pid"
    exec sleep 60 ;;
esac
exit 0
EOF
printf '#!/bin/bash\nexit 0\n' >"$T/stubs/remote-sync"
printf '#!/bin/bash\nexit 0\n' >"$T/stubs/scp"
chmod +x "$T/stubs/ssh" "$T/stubs/remote-sync" "$T/stubs/scp"

PATH="$T/stubs:$PATH" HOST=stub.invalid REMOTE_NAME="$NAME" \
  "$T/root/bin/test-e2e-remote.sh" test-first.sh test-second.sh >"$T/driver.log" 2>&1 &
drv=$!

for ((i = 0; i < 100; i++)); do
  [[ -f $T/suite.pid ]] && break
  sleep 0.1
done
[[ -f $T/suite.pid ]] || { echo "FAIL: the first suite never started"; cat "$T/driver.log"; exit 1; }
suite_pid="$(cat "$T/suite.pid")"

kill -TERM "$drv"
for ((i = 0; i < 100; i++)); do
  kill -0 "$drv" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$drv" 2>/dev/null; then
  echo "FAIL: the driver still runs 10s after TERM"
  exit 1
fi
rc=0
wait "$drv" || rc=$?
drv=""

failed=0
check() { # description, then a command that holds
  local what=$1
  shift
  if "$@"; then echo "  ok    $what"; else echo "  FAIL  $what"; failed=1; fi
}
check "exit 130 (got $rc)" test "$rc" -eq 130
check "the second suite was never started" bash -c "! grep -q 'test-second.sh' '$T/ssh.log'"
check "the remote process group was told to stop" grep -q 'kill -TERM -- -.*marspot-e2e-.*\.pgid' "$T/ssh.log"
check "the suite's sandbox was reaped" grep -qE 'kill-sandbox-marspot.sh --state-dir /tmp/msp-e2e-[0-9]+-1$' "$T/ssh.log"
# reparented once the driver is gone, and reaped a moment later
gone() {
  for ((i = 0; i < 20; i++)); do
    kill -0 "$1" 2>/dev/null || return 0
    sleep 0.1
  done
  return 1
}
check "the suite's ssh is gone" gone "$suite_pid"
check "the local lock was removed" test ! -e "$LOCK"

if (( failed )); then
  echo "--- driver output"
  cat "$T/driver.log"
  echo "--- ssh calls"
  cat "$T/ssh.log"
  exit 1
fi
echo "PASS: test-e2e-driver-signal"

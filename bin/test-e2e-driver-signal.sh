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
# Runs on this machine in a few seconds and reaches no runner: ssh, scp,
# sudo, remote-sync and bench-lock are stand-ins.  The ssh stand-in runs
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

# only the pids the stand-ins wrote down
cleanup() {
  local f
  [[ -n $drv ]] && { kill -TERM "$drv" 2>/dev/null || true; }
  for f in "$T"/*/*.pid; do
    [[ -f $f ]] && { kill -TERM "$(cat "$f")" 2>/dev/null || true; }
  done
  rm -rf "${T:?}" "${LOCK:?}"
}
trap cleanup EXIT

check() { # description, then a command that holds
  local what=$1
  shift
  if "$@"; then echo "  ok    $what"; else echo "  FAIL  $what"; failed=1; fi
}

gone() {
  local i
  for ((i = 0; i < 30; i++)); do
    kill -0 "$1" 2>/dev/null || return 0
    sleep 0.1
  done
  return 1
}

# the line of the first match of $2 in $1, or 0
line_of() { grep -n -m1 -- "$2" "$1" | cut -d: -f1 || true; }

before() { # file, first, then: both present, first earlier
  local a b
  a="$(line_of "$1" "$2")"
  b="$(line_of "$1" "$3")"
  [[ -n $a && -n $b ]] && ((a < b))
}

# $1 scenario; writes the stand-ins and the driver's copy under $T/$1
setup() {
  local S="$T/$1" R="$T/$1/home/work/$NAME"
  mkdir -p "$S/root/bin" "$S/stubs" "$S/tmp" "$R/bin"
  cp "$ROOT/bin/test-e2e-remote.sh" "$ROOT/bin/_e2e-remote-lib.sh" "$ROOT/bin/_git-sha.sh" "$S/root/bin/"
  : >"$S/root/bin/test-first.sh"
  : >"$S/root/bin/test-second.sh"
  git -C "$S/root" init -q
  git -C "$S/root" add -A
  git -C "$S/root" -c user.name=t -c user.email=t@localhost commit -qm t

  cp "$ROOT/bin/_bounded.sh" "$R/bin/"
  printf '#!/bin/bash\necho $$ >"%s/suite.pid"\nexec sleep 60\n' "$S" >"$R/bin/test-first.sh"
  printf '#!/bin/bash\necho "kill-sandbox held=${STUB_LOCK_HELD:-0} $*" >>"%s/events"\n' "$S" \
    >"$R/bin/kill-sandbox-marspot.sh"

  cat >"$S/stubs/ssh" <<EOF
#!/bin/bash
printf '%s\n' "\$*" >>"$S/ssh.log"
cmd="\${!#}"
cmd="\${cmd//\/tmp\//$S/tmp/}"
cmd="\${cmd//\/usr\/local\/bin\/bench-lock/$S/stubs/bench-lock}"
case "\$cmd" in
  *bench-lock*bin/test-first.sh*)
    exec env HOME="$S/home" perl -e 'setpgrp(0, 0); exec @ARGV or die' bash -c "\$cmd" ;;
  *"kill -TERM -- -"*)
    exec bash -c "\$cmd" ;;
esac
exit 0
EOF
  # sudo -n launchctl asuser UID sudo -u USER /bin/bash -lc CMD: run CMD,
  # without the login shell's profile
  cat >"$S/stubs/sudo" <<'EOF'
#!/bin/bash
while [[ $# -gt 0 && $1 != /bin/bash ]]; do shift; done
exec bash -c "$3"
EOF
  cat >"$S/stubs/bench-lock" <<EOF
#!/bin/bash
# like the real one: a signal is acted on once the command returns
echo \$\$ >"$S/bench-lock.pid"
trap 'exit 143' TERM INT HUP
trap 'echo "bench-lock released" >>"$S/events"' EXIT
shift
while [[ \$1 == --* ]]; do
  case \$1 in --brief) shift ;; *) shift 2 ;; esac
done
echo "bench-lock holding" >>"$S/events"
STUB_LOCK_HELD=1 "\$@"
exit \$?
EOF
  if [[ $1 == sync ]]; then
    printf '#!/bin/bash\nsleep 60 &\necho $! >"%s/sync-child.pid"\necho $$ >"%s/sync.pid"\nwait\n' "$S" "$S" \
      >"$S/stubs/remote-sync"
  else
    printf '#!/bin/bash\nexit 0\n' >"$S/stubs/remote-sync"
  fi
  printf '#!/bin/bash\nexit 0\n' >"$S/stubs/scp"
  chmod +x "$S/stubs/"* "$R/bin/"*
}

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

dump() {
  local S="$T/$1"
  echo "--- $1: driver output"
  cat "$S/driver.log"
  echo "--- $1: ssh calls"
  cat "$S/ssh.log" 2>/dev/null || true
  echo "--- $1: far-side events"
  cat "$S/events" 2>/dev/null || true
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
check "the local lock was removed" test ! -e "$LOCK"
((failed)) && dump suite

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

if ((failed)); then
  exit 1
fi
echo "PASS: test-e2e-driver-signal"

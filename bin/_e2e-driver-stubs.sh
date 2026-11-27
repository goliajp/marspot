# shellcheck shell=bash
# shellcheck disable=SC2034 # failed is read by the tests that source it
# The stand-ins of bin/test-e2e-remote.sh's tests (test-e2e-driver-signal.sh,
# test-e2e-own-jobs.sh): sourced after ROOT, T (a scratch directory), NAME
# (the remote copy's name), LOCK (the driver's local lock), drv="" and
# failed=0 are set.  The ssh stand-in runs the suite's command, the stop
# command and the check for jobs of ours locally, each in a process group
# of its own as sshd would, with the "remote" tree under a scratch HOME and
# /tmp/ moved into the scratch directory; every other remote call is only
# written down.  `bench-lock status` prints $S/status.N on its Nth call
# (or $S/status), and the lock log is $S/lock.log.

# only the pids the stand-ins wrote down
cleanup() {
  local f
  [[ -n $drv ]] && { kill -TERM "$drv" 2>/dev/null || true; }
  for f in "$T"/*/*.pid; do
    [[ -f $f ]] && { kill -TERM "$(cat "$f")" 2>/dev/null || true; }
  done
  rm -rf "${T:?}" "${LOCK:?}"
}

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
  cp "$ROOT/bin/test-e2e-remote.sh" "$ROOT/bin/_e2e-remote-lib.sh" "$ROOT/bin/_e2e-suites.sh" \
    "$ROOT/bin/_e2e-host-share.sh" "$ROOT/bin/_lock-mark.sh" "$ROOT/bin/_git-sha.sh" "$S/root/bin/"
  : >"$S/root/bin/test-first.sh"
  [[ $1 != malformed ]] || echo '# e2e-dock-tiles-max: x' >"$S/root/bin/test-first.sh"
  : >"$S/root/bin/test-second.sh"
  git -C "$S/root" init -q
  git -C "$S/root" add -A
  git -C "$S/root" -c user.name=t -c user.email=t@localhost commit -qm t

  cp "$ROOT/bin/_bounded.sh" "$ROOT/bin/_e2e-own-jobs.py" "$R/bin/"
  if [[ $1 == left || $1 == own-* ]]; then
    printf '#!/bin/bash\nexit 0\n' >"$R/bin/test-first.sh"
  else
    printf '#!/bin/bash\necho $$ >"%s/suite.pid"\nexec sleep 60\n' "$S" >"$R/bin/test-first.sh"
  fi
  printf '#!/bin/bash\necho "kill-sandbox held=${STUB_LOCK_HELD:-0} $*" >>"%s/events"\n' "$S" \
    >"$R/bin/kill-sandbox-marspot.sh"
  # the check finds a record left behind; in "snap" the snapshot takes
  # long enough to be stopped in
  local slow=""
  [[ $1 == snap ]] && slow="[[ \$1 == snap ]] && { echo \$\$ >\"$S/snap.pid\"; exec sleep 30; }"
  cat >"$R/bin/_e2e-suite-clean.sh" <<EOF
#!/bin/bash
echo "suite-clean \$1 held=\${STUB_LOCK_HELD:-0}" >>"$S/events"
$slow
[[ \$1 == check ]] || exit 0
printf 'suite_rc=0\nleft: record: Foreground pid=1 /tmp/msp-e2e-x/Marspot.app\nclean=1\n' >"\$3"
exit 1
EOF

  cat >"$S/stubs/ssh" <<EOF
#!/bin/bash
printf '%s\n' "\$*" >>"$S/ssh.log"
cmd="\${!#}"
cmd="\${cmd//\/tmp\//$S/tmp/}"
cmd="\${cmd//\/usr\/local\/bin\/bench-lock/$S/stubs/bench-lock}"
# the login shell's profile is this machine's, not the far side's
cmd="\${cmd//\/bin\/bash -lc/bash -c}"
case "\$cmd" in
  *"lsappinfo list"*)
    [[ -e "$S/noconsole" ]] && { echo 'no console here'; exit 0; }
    echo '1) "Dock" ASN:0x0-0x1:'
    echo '    bundle path="/System/Library/CoreServices/Dock.app"'
    exit 0 ;;
  *bench-lock*bin/test-first.sh*)
    exec env HOME="$S/home" E2E_BENCH_LOCK="$S/stubs/bench-lock" E2E_LOCK_LOG="$S/lock.log" perl -e 'setpgrp(0, 0); exec @ARGV or die' bash -c "\$cmd" ;;
  *"kill -TERM -- -"*)
    exec bash -c "\$cmd" ;;
  *"cat "*.clean)
    exec env HOME="$S/home" bash -c "\$cmd" ;;
  *_e2e-own-jobs.py)
    exec env HOME="$S/home" E2E_BENCH_LOCK="$S/stubs/bench-lock" E2E_LOCK_LOG="$S/lock.log" bash -c "\$cmd" ;;
esac
exit 0
EOF
  cat >"$S/stubs/bench-lock" <<EOF
#!/bin/bash
if [[ \$1 == status ]]; then
  n=\$(( \$(cat "$S/status.n" 2>/dev/null || echo 0) + 1 ))
  echo \$n >"$S/status.n"
  f="$S/status.\$n"
  [[ -f \$f ]] || f="$S/status"
  cat "\$f" 2>/dev/null
  exit 0
fi
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

dump() {
  local S="$T/$1"
  echo "--- $1: driver output"
  cat "$S/driver.log"
  echo "--- $1: ssh calls"
  cat "$S/ssh.log" 2>/dev/null || true
  echo "--- $1: far-side events"
  cat "$S/events" 2>/dev/null || true
}

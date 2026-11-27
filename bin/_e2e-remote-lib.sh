# bin/_e2e-remote-lib.sh — what bin/test-e2e-remote.sh does on the far
# side, sourced by it after HOST, REMOTE_DIR, REMOTE_PIDF and RUN_ID
# are set.

# For the cleanup calls, which run while the run is stopping: a runner
# that went away is noticed within ten seconds instead of when TCP
# gives up, and each remote command is under bin/_bounded.sh.
SSH_CLEANUP=(-o ConnectTimeout=5 -o ServerAliveInterval=5 -o ServerAliveCountMax=2)

# One tool does the copying, for every session and every repo: it takes
# the exclude list from git itself, so a path this repo ignores is never
# sent and never deleted on the far side -- which is how the remote
# target/ survives a sync from a tree that has none.
#
# In a process group of its own and waited for, like remote_tracked: the
# receiving rsync may wait up to half an hour for a measurement on the
# host, and a signal to this script must not wait with it.
SYNC_PGID=""
remote_sync() {
  local tool rc=0
  tool="$(command -v remote-sync || true)"
  [[ -n $tool ]] || tool="$HOME/workspace/goliajp/golia-claude-configs/bin/remote-sync"
  [[ -x $tool ]] || {
    echo "remote-sync not found (looked on PATH and in $tool)" >&2
    exit 1
  }
  set -m
  "$tool" "$@" </dev/null &
  SYNC_PGID=$!
  set +m
  wait "$SYNC_PGID" || rc=$?
  SYNC_PGID=""
  return $rc
}

# The command for the console login, run straight in the ssh login of
# the same user: that login reaches the console session's window server
# and LaunchServices (`open`, `lsappinfo`, NSRunningApplication) with no
# prompt.  It is the only way in that a runner allows, so check_console
# proves it before the first suite and nothing falls back to another.
gui_cmd() {
  printf '/bin/bash -lc %q' "$1"
}

# Before anything is copied: a plain ssh login that cannot see the
# console session (lsappinfo lists no Dock) can open no window and read
# no leftover, so the run stops here and says what it read.
check_console() {
  local out rc=0
  out="$(ssh "${SSH_CLEANUP[@]}" "$HOST" \
    "perl -e 'alarm shift; exec @ARGV' 20 /usr/bin/lsappinfo list" </dev/null 2>&1)" || rc=$?
  grep -q 'path="/System/Library/CoreServices/Dock.app' <<<"$out" && return 0
  echo "FAIL: the ssh login on $HOST does not see the console session:" \
    "\`/usr/bin/lsappinfo list\` (exit $rc) lists no Dock; is the same user logged in" \
    "at the console? What it printed:" >&2
  printf '%s\n' "$out" | head -20 | sed 's/^/    /' >&2
  return 1
}

remote_gui() {
  remote_tracked "$(gui_cmd "$1")"
}

# The ssh login shell leads a process group of its own, and everything
# the command starts is in it unless it leaves on purpose.  Its pid is
# written down for as long as the command runs, so an interrupted run
# stops that group and nothing else on the host.
#
# ssh runs in the background and is waited for: bash runs a trap only
# once the foreground command returns, so a signal sent to this script
# while a suite ran used to wait for the whole suite.  `wait` returns
# as soon as a trapped signal arrives.
SSH_PID=""
remote_tracked() {
  local rc
  ssh "$HOST" "echo \$\$ > $REMOTE_PIDF; $1; rc=\$?; rm -f $REMOTE_PIDF; exit \$rc" </dev/null &
  SSH_PID=$!
  wait "$SSH_PID"
  rc=$?
  SSH_PID=""
  return $rc
}

stop_remote() {
  [[ -n $SYNC_PGID ]] && kill -TERM -- "-$SYNC_PGID" 2>/dev/null || true
  [[ -n $SSH_PID ]] && kill -TERM "$SSH_PID" 2>/dev/null || true
  ssh "${SSH_CLEANUP[@]}" "$HOST" "
    if [ -f $REMOTE_PIDF ]; then
      kill -TERM -- -\$(cat $REMOTE_PIDF) 2>/dev/null
      rm -f $REMOTE_PIDF
    fi" </dev/null >/dev/null 2>&1 || true
}

# Stops the processes the suite's sandbox recorded, and only those.
reap_sandbox() {
  ssh "${SSH_CLEANUP[@]}" "$HOST" \
    "cd ~/$REMOTE_DIR && bin/_bounded.sh 30 bin/kill-sandbox-marspot.sh --state-dir $1" </dev/null || true
}

# A suite's state dir and its scratch (dev_tmp, under the same prefix),
# removed once it passed or was not measured; a failed one is kept.
# sh expands the glob: the runner's login zsh would refuse the whole rm
# when no scratch matches
drop_sandbox() {
  ssh "${SSH_CLEANUP[@]}" "$HOST" \
    "sh -c 'rm -rf \"\${1:?}\" \"\${1:?}\"-* \"\${1:?}\".before \"\${1:?}\".clean' _ ${1:?}" </dev/null || true
}

# What bin/_e2e-suite-clean.sh found inside the suite's hold, read from
# STATE_DIR.clean and printed.  0: nothing left; 4: something left; 5: it
# could not be read, or the check never ran (a stopped run).
read_clean() {
  local out
  out="$(ssh "${SSH_CLEANUP[@]}" "$HOST" "cd ~/$REMOTE_DIR && bin/_bounded.sh 10 cat ${1:?}.clean" </dev/null 2>&1)"
  printf '%s\n' "${out:-no result file: the check did not run}"
  case $out in
    *clean=0) return 0 ;;
    *clean=1) return 4 ;;
    *) return 5 ;;
  esac
}

# What LaunchServices lists under this run's paths, before the first
# suite (each suite's own check runs inside its hold, see read_clean),
# read through gui_cmd; records go a
# moment after their process, so it is looked at for up to ten seconds.
# Only this run's paths count: its remote tree, and the state dirs and
# scratch under /tmp/msp-e2e-<run>- (what another run on the host left
# is not this one's), less the records listed before the first suite
# (KNOWN_ARGS, see note_known_leftovers).  Prints bin/_ls-leftovers.sh's
# lines.  0: nothing left; 4: something left; 5: the list could not be
# read, which proves nothing either way.
KNOWN_ARGS=""
check_leftovers() {
  local out rc
  out="$(ssh "${SSH_CLEANUP[@]}" "$HOST" "cd ~/$REMOTE_DIR && bin/_bounded.sh 30 $(gui_cmd "cd ~/$REMOTE_DIR && \
    for i in 1 2 3 4 5 6 7 8 9 10; do \
    out=\$(bin/_ls-leftovers.sh$KNOWN_ARGS \$HOME/$REMOTE_DIR/ /tmp/msp-e2e-$RUN_ID-); rc=\$?; \
    [ \$rc -eq 1 ] || break; sleep 1; done; echo \"\$out\"; exit \$rc")" </dev/null 2>&1)"
  rc=$?
  printf '%s\n' "$out"
  case $rc in
    0) return 0 ;;
    1) return 4 ;;
    *) return 5 ;;
  esac
}

# Before the first suite: what the remote tree already has listed was
# left by an earlier run.  It still needs cleaning, so it is said once
# here, but no suite of this run made it: each suite's check leaves
# those records (by pid and path) out.
note_known_leftovers() {
  local out rc=0 line rec n=0
  out="$(check_leftovers)" || rc=$?
  case $rc in
    0) return 0 ;;
    4) ;;
    *)
      echo "==> warning: what LaunchServices lists on $HOST could not be read before the first suite;" \
        "every record a suite's check finds counts against that suite"
      printf '%s\n' "$out" | sed 's/^/    /'
      return 0 ;;
  esac
  while IFS= read -r line; do
    [[ $line =~ ^[^\ ]+\ pid=([^\ ]+)\ (.+)$ ]] || continue
    rec="${BASH_REMATCH[1]}:${BASH_REMATCH[2]}"
    # the arguments end up inside a single-quoted sh -c string, where a
    # quote breaks the command and %q's $'...' means nothing to sh
    if [[ $line == *"'"* || $line =~ [[:cntrl:]] ]]; then
      echo "==> warning: not excluding a record whose path has a quote or control character: $line"
      continue
    fi
    KNOWN_ARGS+=" --known $(printf '%q' "$rec")"
    n=$((n + 1))
  done <<<"$out"
  echo "==> warning: $HOST already lists $n app record(s) under ~/$REMOTE_DIR, left by an earlier run;" \
    "they need cleaning, and no suite of this run is blamed for them:"
  printf '%s\n' "$out" | sed 's/^/    /'
}

# A suite's exit status that says it was stopped from outside (the
# host's owner, or the runner going away) rather than that it failed:
# the run ends there instead of starting the next suite.  _bounded.sh
# gives 124 for its own time limit and 143 when it was signalled.
stopped_from_outside() {
  case $1 in
    129 | 130 | 137 | 143 | 255) return 0 ;;
    *) return 1 ;;
  esac
}

# The command a suite runs on the far side, inside the host's lock:
# locked_cmd STATE_DIR LOCK RUN PRE_CLEAN ENV.  The state dir is created
# first so the sandbox helpers find it and give its sessions ids of their
# own (see dev_seed_session_base).  What the suite left running is
# stopped before the lock is let go, so nothing it opened outlives the
# hold; the reap after it is for a stopped run.  Still inside the hold,
# bin/_e2e-suite-clean.sh compares the runner with what it was before the
# suite (Dock tiles, app records, TCC requests; $PPID is bench-lock, this
# suite's holder) and writes STATE_DIR.clean; the command then gives 4
# when something was left and 5 when that could not be read.
# A stopped run signals the whole remote group, bench-lock and this sh
# included: bench-lock lets go only once its command returns, so the sh
# waits for _bounded.sh to stop what runs and still runs the cleanup, but
# neither the suite (when the signal came during the snapshot) nor the
# check.  Its output then goes nowhere, the ssh it wrote to being gone,
# so the cleanup cannot die of SIGPIPE.
locked_cmd() {
  local sd=$1 lock=$2 run=$3 pre=$4 env=$5
  printf '%s' "cd ~/$REMOTE_DIR && rm -rf $sd $sd.before $sd.clean && mkdir -p $sd && \
    env MARSPOT_STATE_DIR=$sd$env /usr/local/bin/bench-lock $lock \
    sh -c 'stopped=; trap \"stopped=1; exec >/dev/null 2>&1\" TERM INT HUP; \
    bin/_bounded.sh 30 bin/_e2e-suite-clean.sh snap $sd.before --lock-pid \$PPID --live /tmp/msp-e2e-$RUN_ID-live; \
    if [ -z \"\$stopped\" ]; then $run; rc=\$?; else rc=143; fi; $pre\
    bin/_bounded.sh 30 bin/kill-sandbox-marspot.sh --state-dir $sd; \
    [ -n \"\$stopped\" ] && exit \$rc; \
    bin/_bounded.sh 90 bin/_e2e-suite-clean.sh check $sd.before $sd.clean \
      --suite-rc \$rc --lock-pid \$PPID$KNOWN_ARGS \$HOME/$REMOTE_DIR/ /tmp/msp-e2e-$RUN_ID- >/dev/null; \
    case \$? in 0) exit \$rc ;; 1) exit 4 ;; *) exit 5 ;; esac'"
}

# The run's cached answer of whether the log queries still match anything.
drop_run_files() {
  ssh "${SSH_CLEANUP[@]}" "$HOST" "rm -f /tmp/msp-e2e-${RUN_ID:?}-live" </dev/null || true
}

# One suite's verdict from suite_rc and left_rc (with left_why): appended
# to results, said at once (a run that is cut short prints no summary),
# and rc / unmeasured set.  A passed or unmeasured suite's sandbox goes.
record_result() {
  local why
  if (( suite_rc == 0 && left_rc == 0 )); then
    drop_sandbox "$current_sd"
    results+=("PASS  ${secs}s  $suite")
    echo "==> PASS  ${secs}s  $suite"
  elif (( suite_rc == 3 && left_rc == 0 )) && may_be_unmeasured "$suite"; then
    drop_sandbox "$current_sd"
    results+=("BUSY  ${secs}s  $suite  (not measured: $HOST was busy)")
    echo "==> BUSY  ${secs}s  $suite: not measured, $HOST was busy; neither a pass nor a regression"
    unmeasured=1
  elif (( suite_rc == 75 && left_rc == 0 )) && is_measurement "$suite"; then
    # the exclusive hold waits for every other heavy job on the host;
    # not getting it in time says the host was busy, not the suite failed
    drop_sandbox "$current_sd"
    results+=("BUSY  ${secs}s  $suite  (not measured: no exclusive hold of $HOST's lock within ${MAX_WAIT}s)")
    echo "==> BUSY  ${secs}s  $suite: not measured, the exclusive lock was not free within ${MAX_WAIT}s;" \
      "neither a pass nor a regression"
    unmeasured=1
  else
    case $suite_rc in
      0) why="passed" ;;
      124) why="timed out after ${SUITE_TIMEOUT}s" ;;
      75) why="could not get the host's lock within ${MAX_WAIT}s" ;;
      *) why="exit $suite_rc" ;;
    esac
    if is_measurement "$suite"; then
      case $suite_rc in
        1) why="over budget, measured on a quiet host" ;;
        2) why="no reading: the sandbox app did not come up as asked, or the instrument failed" ;;
      esac
    fi
    [[ -n $left_why ]] && why+=", but $left_why"
    echo "FAIL: $suite: $why (state dir kept: $HOST:$current_sd)" >&2
    results+=("FAIL  ${secs}s  $suite  ($why)")
    rc=1
  fi
}

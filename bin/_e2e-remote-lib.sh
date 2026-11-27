# bin/_e2e-remote-lib.sh — what bin/test-e2e-remote.sh does on the far
# side, sourced by it after HOST, REMOTE_DIR, REMOTE_PIDF, GUI_UID and
# RUN_ID are set.

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

# The command in the console login session: a plain ssh process is in
# another bootstrap namespace, where `NSApplication` has no window
# server to talk to and `lsappinfo` lists none of the login's apps.
gui_cmd() {
  printf 'sudo -n launchctl asuser %s sudo -u $(id -un %s) /bin/bash -lc %q' "$GUI_UID" "$GUI_UID" "$1"
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
  ssh "${SSH_CLEANUP[@]}" "$HOST" "sh -c 'rm -rf \"\${1:?}\" \"\${1:?}\"-*' _ ${1:?}" </dev/null || true
}

# What LaunchServices still lists of what a suite opened (a Dock tile on
# a machine nobody is at), read in the console session; records go a
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
  local out rc=0 line n=0
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
    KNOWN_ARGS+=" --known $(printf '%q' "${BASH_REMATCH[1]}:${BASH_REMATCH[2]}")"
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

# shellcheck shell=bash
# bin/_e2e-host-share.sh — a window suite never shares the runner with
# another job of ours.  Sourced by bin/test-e2e-remote.sh after
# bin/_e2e-remote-lib.sh, with HOST, REMOTE_DIR and MAX_WAIT set.
#
# A gate, a test run, a build, a bench or another window run of marspot
# on the same host opens programs and Dock tiles the suite's checks would
# have to tell from its own; whose cannot be told.  Jobs of other
# projects are not waited for: the check in the hold reads a window
# shared with them as such.  Who holds the host's lock is read by
# bin/_e2e-own-jobs.py, before the hold is asked for and once more
# inside it (locked_cmd), since one can take the lock in between.

# what the command in the hold exits with when another job of ours holds
# the lock beside it: the suite was not started
OWN_JOB_RC=76

# wait_own_jobs DEADLINE: returns 0 once no other job of ours holds the
# host's lock; 1 when one still does (or that cannot be read) at the
# epoch DEADLINE.  Says who, once per change
wait_own_jobs() {
  local out rc said="" left
  while :; do
    rc=0
    out="$(ssh "${SSH_CLEANUP[@]}" "$HOST" \
      "cd ~/$REMOTE_DIR && bin/_bounded.sh 30 /usr/bin/python3 bin/_e2e-own-jobs.py" </dev/null 2>&1)" || rc=$?
    ((rc == 0)) && return 0
    if [[ $out != "$said" ]]; then
      echo "==> waiting (up to $(date -r "$1" '+%H:%M:%S')) for a job of ours on $HOST's lock (exit $rc):"
      printf '%s\n' "$out" | sed 's/^/    /'
      said=$out
    fi
    left=$(($1 - $(date +%s)))
    ((left > 0)) || return 1
    # in the background: a signal to the run is acted on at once
    sleep $((left < 30 ? left : 30)) &
    wait $!
  done
}

# run_held CMD: CMD (locked_cmd's) through remote_gui once no other job
# of ours holds the lock, again while the hold finds one that took it
# first, until MAX_WAIT from now.  Sets suite_rc; OWN_JOB_RC when the
# suite was never started
run_held() {
  local deadline=$(($(date +%s) + MAX_WAIT))
  suite_rc=$OWN_JOB_RC
  while wait_own_jobs "$deadline"; do
    suite_rc=0
    remote_gui "$1" || suite_rc=$?
    ((suite_rc == OWN_JOB_RC)) || return 0
    echo "==> a job of ours took $HOST's lock first; waiting for it"
    # of two window runs' holds the later yields (bin/_e2e-own-jobs.py);
    # a few random seconds keep two that yielded from asking again as one
    sleep $((1 + RANDOM % 8)) &
    wait $!
  done
}

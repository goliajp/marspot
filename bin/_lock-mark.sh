# shellcheck shell=bash
# bin/_lock-mark.sh — what a job of marspot says about itself when it
# takes a runner's bench lock.  Sourced; defines one function.
#
# `bench-lock status` shows a holder's --until note on its line, whatever
# the command is; a window suite reads it there to tell a job of ours (a
# test run, a gate, a build, a bench, a fixture check, another window
# run) from another project's (bin/_e2e-own-jobs.py), also when the lock
# log, which names the directory a take was asked from, cannot be read.
# Every script of ours that takes the lock on a runner passes this.

# lock_until WHEN KIND NAME: the `--until` words, quoted for one shell.
# WHEN is how long the hold is expected to last: seconds, or what
# bench-lock takes (+45m, 23:30, with a note after it); KIND says what
# the job is, NAME which copy or run
lock_until() {
  local when=$1
  [[ $when =~ ^[0-9]+$ ]] && when="+$when"
  printf -- '--until %q' "$when marspot-job $2 $3"
}

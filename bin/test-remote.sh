#!/usr/bin/env bash
# bin/test-remote.sh — run bin/test.sh on the remote Apple Silicon host.
#
# Why: the full suite spins up real L3 processes, PTYs and Metal/CoreText
# per test binary.  On the dev box that competes with the terminal the
# user is working in (and once, 2026-07-28, took WindowServer down with
# it); on mini it competes with nothing.  Same arch + macOS, so a pass
# there means what a pass here means.
#
# Usage:
#   bin/test-remote.sh                      # whole suite (default host=mini)
#   bin/test-remote.sh -E 'test(idle)'      # args pass through to nextest
#   HOST=other-mini bin/test-remote.sh
#
# Contract:
#   - exclusive  : mkdir lock locally (atomic; macOS has no flock(1))
#   - separate   : its own remote dir, so it never disturbs the bench
#                  mirror bench-remote.sh keeps in ~/work/bench-marspot
#   - terminating: Ctrl-C kills the remote process group
#   - honest     : exits with the remote suite's exit code

set -euo pipefail

# One tool does the copying, for every session and every repo: it takes
# the exclude list from git itself, so a path this repo ignores is never
# sent and never deleted on the far side -- which is how the remote
# target/ survives a sync from a tree that has none.
remote_sync() {
  local tool
  tool="$(command -v remote-sync || true)"
  [[ -n $tool ]] || tool="$HOME/workspace/goliajp/golia-claude-configs/bin/remote-sync"
  [[ -x $tool ]] || {
    echo "remote-sync not found (looked on PATH and in $tool)" >&2
    exit 1
  }
  "$tool" "$@"
}

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOST="${HOST:-mini}"
REMOTE_NAME="${REMOTE_NAME:-test-marspot}"
# remote-sync always lands in ~/work/<name>, so that is where the
# run has to look.
REMOTE_DIR="work/$REMOTE_NAME"
LOCK_LOCAL="/tmp/marspot-test-remote.lock.d"
REMOTE_PIDF="/tmp/marspot-test-remote.pgid"

if ! mkdir "$LOCK_LOCAL" 2>/dev/null; then
  if [[ -f "$LOCK_LOCAL/pid" ]] && kill -0 "$(cat "$LOCK_LOCAL/pid")" 2>/dev/null; then
    echo "test-remote already running locally (pid $(cat "$LOCK_LOCAL/pid"))" >&2
    exit 1
  fi
  rm -rf "$LOCK_LOCAL" && mkdir "$LOCK_LOCAL"
fi
echo "$$" > "$LOCK_LOCAL/pid"

cleanup() {
  ssh -o ConnectTimeout=5 "$HOST" "
    if [[ -f $REMOTE_PIDF ]]; then
      pgid=\$(cat $REMOTE_PIDF 2>/dev/null || true)
      [[ -n \${pgid:-} ]] && kill -- -\$pgid 2>/dev/null || true
      rm -f $REMOTE_PIDF
    fi
    rm -rf $REMOTE_LOCK" >/dev/null 2>&1 || true
  rm -rf "$LOCK_LOCAL"
}
trap cleanup EXIT INT TERM

# Same excludes as bench-remote.sh: the tree, not its build products.
# ---- remote lock -----------------------------------------------------
# Shared with bench-remote: whatever is measuring on that host, only one
# thing measures at a time.  A test run underneath a bench is what made
# an idle re-measure read 47% low (2026-09-28).
REMOTE_LOCK="/tmp/marspot-runner.lock.d"
waited=0
until ssh -o ConnectTimeout=5 "$HOST" "
  if mkdir $REMOTE_LOCK 2>/dev/null; then echo \$\$ > $REMOTE_LOCK/pid; exit 0; fi
  if [[ -f $REMOTE_LOCK/pid ]] && kill -0 \$(cat $REMOTE_LOCK/pid) 2>/dev/null; then exit 1; fi
  rm -rf $REMOTE_LOCK && mkdir $REMOTE_LOCK && echo \$\$ > $REMOTE_LOCK/pid" </dev/null 2>/dev/null; do
  if (( waited == 0 )); then
    echo "==> $HOST is measuring something else; waiting for it"
  fi
  waited=$((waited + 5))
  if (( waited > 600 )); then
    echo "gave up after 10 min: $HOST still holds $REMOTE_LOCK" >&2
    exit 1
  fi
  sleep 5
done

remote_sync "$HOST" --name "$REMOTE_NAME"

args=""
for a in "$@"; do args+=" $(printf '%q' "$a")"; done

echo "==> bin/test.sh on $HOST"
set +e
# One test thread per core on the runner, not the default six.
#
# The six protects the machine someone is working on: a test storm took
# the interactive host down on 2026-07-28, and these tests spawn real
# PTYs, processes and sockets.  This host is not that machine — it is
# the dedicated runner, and `test-remote.sh` only ever talks to it.
#
# Measured on an idle mini (2026-09-28), each setting run back to back
# with the load allowed to settle in between, 1403 tests green every
# time and no leaks: six 4.47 / 4.41 s, eight 3.71 / 3.71 s, ten 3.07 /
# 3.09 / 3.05 / 3.06 s, fourteen 2.62 / 2.56 / 2.61 / 2.60 s.  The suite
# is bounded by total work over threads now that its longest test is
# under a second, so it scales until the cores run out — which is where
# this stops.  An earlier sweep that ranked ten and fourteen LAST was
# reading the load it had made itself; load1 is a lagging average, and
# each run has to start from a quiet box or the comparison is noise.
# The run holds the host's bench lock, shared.
#
# Shared is what heavy work takes: several builds and test runs can hold
# it at once and none of them waits for another.  What it does do is
# make a measurement -- which takes the same lock exclusively -- wait
# for this to finish instead of reading a number taken while a test
# suite had the cores.  See `bin/bench-remote.sh` for the other half.
ssh "$HOST" "echo \$\$ > $REMOTE_PIDF; cd ~/$REMOTE_DIR && \
  MARSPOT_TEST_JOBS=${MARSPOT_TEST_JOBS:-14} \
  exec /usr/local/bin/bench-lock heavy bin/test.sh$args"
rc=$?
set -e
exit $rc

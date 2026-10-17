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
#   - exclusive  : mkdir lock locally (atomic; macOS has no flock(1)),
#                  so two checkouts never sync over one remote tree
#   - separate   : its own remote dir, so it never disturbs the bench
#                  mirror bench-remote.sh keeps in ~/work/bench-marspot
#   - shares     : holds the host's bench lock shared (`heavy`), so a
#                  measurement waits for the run instead of measuring it
#   - bounded    : the remote run is stopped after TEST_REMOTE_TIMEOUT
#                  seconds (default 1800, build included)
#   - terminating: Ctrl-C kills the remote process group
#   - honest     : exits with the remote suite's exit code (124: timed out)

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
REMOTE_PIDF="/tmp/marspot-test-remote-$(date +%H%M%S)$$.pgid"
TIMEOUT="${TEST_REMOTE_TIMEOUT:-1800}"
MAX_WAIT="${TEST_REMOTE_MAX_WAIT:-3600}"

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
    if [ -f $REMOTE_PIDF ]; then
      kill -TERM -- -\$(cat $REMOTE_PIDF) 2>/dev/null
      rm -f $REMOTE_PIDF
    fi" </dev/null >/dev/null 2>&1 || true
  rm -rf "$LOCK_LOCAL"
}
trap cleanup EXIT INT TERM

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
# The login shell's pid is the process group of the whole run; it stays
# written down only while the run lasts, so the cleanup never signals a
# group that has since been reused.
ssh "$HOST" "echo \$\$ > $REMOTE_PIDF; cd ~/$REMOTE_DIR && \
  MARSPOT_TEST_JOBS=${MARSPOT_TEST_JOBS:-14} \
  /usr/local/bin/bench-lock heavy --max-wait $MAX_WAIT bin/_bounded.sh $TIMEOUT bin/test.sh$args; \
  rc=\$?; rm -f $REMOTE_PIDF; exit \$rc" </dev/null
rc=$?
set -e
(( rc != 124 )) || echo "==> bin/test.sh on $HOST did not finish within ${TIMEOUT}s" >&2
exit $rc

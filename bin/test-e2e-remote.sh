#!/usr/bin/env bash
# bin/test-e2e-remote.sh — run the window-opening E2E suites on the
# remote Mac instead of the machine someone is working on.
#
# Why: these scripts launch a real sandbox marspot, with real windows.
# On the interactive machine they land on top of whatever the user is
# doing, and a crash in the sandbox pops the system's crash reporter at
# them (both happened, 2026-09-28).  They also take tens of minutes
# when that machine is busy.  The runner has a console login, so it can
# put the windows somewhere nobody is looking.
#
# Usage:
#   bin/test-e2e-remote.sh                       # the window-opening set
#   bin/test-e2e-remote.sh test-window-close-order.sh
#   HOST=other-mini bin/test-e2e-remote.sh
#
# Contract:
#   - one measurement at a time : takes the same remote lock as the
#                                 test and bench runners
#   - GUI                       : runs inside the console login session
#                                 (`launchctl asuser`), or windows
#                                 cannot be created at all
#   - leaves nothing behind     : kills the sandbox app on the way out,
#                                 whatever the outcome
#   - honest                    : exits with the suite's exit code
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
REMOTE_NAME="${REMOTE_NAME:-e2e-marspot}"
# remote-sync always lands in ~/work/<name>, so that is where the
# run has to look.
REMOTE_DIR="work/$REMOTE_NAME"
REMOTE_LOCK="/tmp/marspot-runner.lock.d"
GUI_UID="${GUI_UID:-501}"

SUITES=("$@")
if [[ ${#SUITES[@]} -eq 0 ]]; then
  SUITES=(test-window-close-order.sh test-shell-rollback-loop.sh)
fi

# Anything that reaches the GUI session has to be asked for through
# launchd; a plain ssh process is in another bootstrap namespace and
# `NSApplication` there has no window server to talk to.
remote_gui() {
  ssh "$HOST" "sudo -n launchctl asuser $GUI_UID sudo -u \$(id -un $GUI_UID) /bin/bash -lc $(printf '%q' "$1")"
}

cleanup() {
  local rc=$?
  echo "==> cleaning up on $HOST"
  remote_gui "cd ~/$REMOTE_DIR && bin/kill-sandbox-marspot.sh" >/dev/null 2>&1 || true
  ssh -o ConnectTimeout=5 "$HOST" "rm -rf $REMOTE_LOCK" >/dev/null 2>&1 || true
  exit $rc
}
trap cleanup EXIT INT TERM

echo "==> waiting for $HOST to be free"
waited=0
until ssh -o ConnectTimeout=5 "$HOST" "
  if mkdir $REMOTE_LOCK 2>/dev/null; then echo \$\$ > $REMOTE_LOCK/pid; exit 0; fi
  if [[ -f $REMOTE_LOCK/pid ]] && kill -0 \$(cat $REMOTE_LOCK/pid) 2>/dev/null; then exit 1; fi
  rm -rf $REMOTE_LOCK && mkdir $REMOTE_LOCK && echo \$\$ > $REMOTE_LOCK/pid" </dev/null 2>/dev/null; do
  waited=$((waited + 5))
  if (( waited > 900 )); then
    echo "gave up after 15 min: $HOST is still measuring something else" >&2
    exit 1
  fi
  sleep 5
done

remote_sync "$HOST" --name "$REMOTE_NAME"

echo "==> building release on $HOST"
ssh "$HOST" "source ~/.cargo/env && cd ~/$REMOTE_DIR && cargo build --release --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -2"

rc=0
for suite in "${SUITES[@]}"; do
  echo
  echo "════ $suite on $HOST ════"
  set +e
  # Shared, like every other heavy run: E2E suites build release
  # binaries and drive real windows, and a measurement taking the lock
  # exclusively has to wait for that rather than read a number through
  # it.  See bin/bench-remote.sh for the other half.
  remote_gui "export PATH=/opt/homebrew/bin:\$PATH; cd ~/$REMOTE_DIR && flock -s ${MARSPOT_BENCH_LOCK:-/Users/Shared/bench.lock} bin/$suite"
  suite_rc=$?
  set -e
  if (( suite_rc != 0 )); then
    echo "FAIL: $suite exited $suite_rc" >&2
    rc=$suite_rc
  fi
done
exit $rc

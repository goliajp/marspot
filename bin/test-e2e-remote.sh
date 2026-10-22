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
#   bin/test-e2e-remote.sh                       # every suite in DEFAULT_SUITES
#   bin/test-e2e-remote.sh test-window-close-order.sh test-multi-window.sh
#   E2E_SUITE_TIMEOUT=1800 bin/test-e2e-remote.sh test-multi-window.sh
#   HOST=other-mini bin/test-e2e-remote.sh
#
# The update suites sign their feed with a key generated for the run;
# the build they test accepts it only with the `test-update-key` feature
# on and a state dir set.
#
# Contract:
#   - shares the host     : every suite holds the host's bench lock,
#                           shared (`heavy`), so a measurement waits for
#                           it; `E2E_LOCK=bench` takes it exclusively
#                           for a suite that is itself a measurement
#   - bounded             : each suite is stopped after E2E_SUITE_TIMEOUT
#                           seconds (default 900); the build after
#                           E2E_BUILD_TIMEOUT (default 1800)
#   - GUI                 : runs inside the console login session
#                           (`launchctl asuser`), or windows cannot be
#                           created at all
#   - its own sandbox     : each suite gets a state dir of its own, and
#                           the cleanup stops the processes recorded
#                           there and nothing else on the host
#   - honest              : exits non-zero when any suite failed
set -euo pipefail

DEFAULT_SUITES=(
  test-shell-core.sh
  test-update-flow.sh
  test-rollback.sh
  test-manual-rollback.sh
  test-shell-rollback-loop.sh
  test-l3-execv-probe.sh
  test-crash-guard.sh
  test-window-close-order.sh
  test-close-last-pane.sh
  test-multi-window.sh
  test-bundle-lander.sh
  test-cc-profile-switch.sh
  test-codex-profile-switch.sh
  test-agent-handoff.sh
  test-peerbus.sh
  test-real-update.sh
  test-negative-update.sh
  test-adversarial-update.sh
)

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
GUI_UID="${GUI_UID:-501}"
SUITE_TIMEOUT="${E2E_SUITE_TIMEOUT:-900}"
BUILD_TIMEOUT="${E2E_BUILD_TIMEOUT:-1800}"
MAX_WAIT="${E2E_MAX_WAIT:-3600}"
RUN_ID="$(date +%H%M%S)$$"
# Names the remote process group of whatever is running right now, so
# an interrupted run can stop exactly that and nothing else.
REMOTE_PIDF="/tmp/marspot-e2e-$RUN_ID.pgid"
# Two runs from two checkouts would sync over each other's remote tree.
LOCK_LOCAL="/tmp/marspot-e2e-remote.lock.d"

SUITES=("$@")
if [[ ${#SUITES[@]} -eq 0 ]]; then
  SUITES=("${DEFAULT_SUITES[@]}")
fi
for suite in "${SUITES[@]}"; do
  [[ -f "$ROOT/bin/$suite" ]] || { echo "no such suite: bin/$suite" >&2; exit 2; }
done

if ! mkdir "$LOCK_LOCAL" 2>/dev/null; then
  if [[ -f "$LOCK_LOCAL/pid" ]] && kill -0 "$(cat "$LOCK_LOCAL/pid")" 2>/dev/null; then
    echo "test-e2e-remote already running locally (pid $(cat "$LOCK_LOCAL/pid"))" >&2
    exit 1
  fi
  rm -rf "$LOCK_LOCAL" && mkdir "$LOCK_LOCAL"
fi
echo "$$" > "$LOCK_LOCAL/pid"

# Anything that reaches the GUI session has to be asked for through
# launchd; a plain ssh process is in another bootstrap namespace and
# `NSApplication` there has no window server to talk to.
remote_gui() {
  remote_tracked "sudo -n launchctl asuser $GUI_UID sudo -u \$(id -un $GUI_UID) /bin/bash -lc $(printf '%q' "$1")"
}

# The ssh login shell leads a process group of its own, and everything
# the command starts is in it unless it leaves on purpose.  Its pid is
# written down for as long as the command runs, so an interrupted run
# stops that group and nothing else on the host.
remote_tracked() {
  ssh "$HOST" "echo \$\$ > $REMOTE_PIDF; $1; rc=\$?; rm -f $REMOTE_PIDF; exit \$rc" </dev/null
}

stop_remote() {
  ssh -o ConnectTimeout=5 "$HOST" "
    if [ -f $REMOTE_PIDF ]; then
      kill -TERM -- -\$(cat $REMOTE_PIDF) 2>/dev/null
      rm -f $REMOTE_PIDF
    fi" </dev/null >/dev/null 2>&1 || true
}

# Stops the processes the suite's sandbox recorded, and only those.
reap_sandbox() {
  ssh -o ConnectTimeout=5 "$HOST" \
    "cd ~/$REMOTE_DIR && bin/kill-sandbox-marspot.sh --state-dir $1" </dev/null || true
}

current_sd=""
interrupted() {
  echo "==> interrupted; stopping the run on $HOST"
  stop_remote
  [[ -n $current_sd ]] && reap_sandbox "$current_sd"
  exit 130
}
trap interrupted INT TERM
trap 'rm -rf "$LOCK_LOCAL"' EXIT

remote_sync "$HOST" --name "$REMOTE_NAME"

echo "==> building release on $HOST"
remote_tracked "source ~/.cargo/env && cd ~/$REMOTE_DIR && \
  /usr/local/bin/bench-lock heavy --max-wait $MAX_WAIT bin/_bounded.sh $BUILD_TIMEOUT \
  sh -c 'cargo build --release --bin marspot-shell --bin marspot-core --bin marspot-session \
    && cargo build --release -p marspot-peer'" \
  2>&1 | tail -2 || { echo "FAIL: release build on $HOST" >&2; exit 1; }
# macOS assesses a freshly built executable on its first exec, for an
# unbounded time on a busy host; pay that here, outside every suite's
# stopwatch.
ssh "$HOST" "cd ~/$REMOTE_DIR && for b in marspot-shell marspot-core marspot-session; do \
  MARSPOT_NO_REDIRECT=1 target/release/\$b --version >/dev/null 2>&1; done" </dev/null || true

# A measuring suite's knobs (`IDLE_*`) are set here and read there, and
# so is anything listed in E2E_ENV ("K=V K=V", e.g. a soak's duration).
fwd=""
while IFS= read -r kv; do fwd+=" $(printf '%q' "$kv")"; done < <(env | grep -E '^IDLE_[A-Z_]+=' || true)
for kv in ${E2E_ENV:-}; do fwd+=" $(printf '%q' "$kv")"; done

rc=0
results=()
i=0
for suite in "${SUITES[@]}"; do
  i=$((i + 1))
  echo
  echo "════ $suite on $HOST ════"
  if [[ ${E2E_LOCK:-heavy} == bench ]]; then
    lock="bench --max-wait $MAX_WAIT"
  else
    lock="heavy --max-wait $MAX_WAIT"
  fi
  # Short: the session sockets live under it and sun_path is 104 bytes.
  current_sd="/tmp/msp-e2e-$RUN_ID-$i"
  start=$(date +%s)
  set +e
  remote_gui "cd ~/$REMOTE_DIR && rm -rf $current_sd && env MARSPOT_STATE_DIR=$current_sd$fwd \
    /usr/local/bin/bench-lock $lock bin/_bounded.sh $SUITE_TIMEOUT bin/$suite"
  suite_rc=$?
  set -e
  secs=$(( $(date +%s) - start ))
  reap_sandbox "$current_sd"
  if (( suite_rc == 0 )); then
    ssh -o ConnectTimeout=5 "$HOST" "rm -rf $current_sd" </dev/null || true
    results+=("PASS  ${secs}s  $suite")
  else
    case $suite_rc in
      124) why="timed out after ${SUITE_TIMEOUT}s" ;;
      75) why="could not get the host's lock within ${MAX_WAIT}s" ;;
      *) why="exit $suite_rc" ;;
    esac
    echo "FAIL: $suite: $why (state dir kept: $HOST:$current_sd)" >&2
    results+=("FAIL  ${secs}s  $suite  ($why)")
    rc=1
  fi
  current_sd=""
done

echo
echo "════ summary ($HOST) ════"
printf '  %s\n' "${results[@]}"
exit $rc

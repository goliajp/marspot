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
#   HOST=<host> bin/test-e2e-remote.sh
#   E2E_ENV=NAME=value bin/test-e2e-remote.sh      # one more variable for every suite
#   E2E_UNTIL=+40m bin/test-e2e-remote.sh          # how long each hold of the host's lock
#                                                  # is expected to last, for whoever queues
#                                                  # behind it (`bench-lock status`)
#   E2E_FEATURES=latency-trace bin/test-e2e-remote.sh measure-latency.sh
#   REMOTE_NAME=my-tree bin/test-e2e-remote.sh     # a remote tree of its own
#
# The update suites sign their feed with a key generated for the run;
# the build they test accepts it only with the `test-update-key` feature
# on and a state dir set.
#
# Contract:
#   - shares the host     : every suite holds the host's bench lock,
#                           shared (`heavy`), so a measurement waits for
#                           it; a measuring suite (is_measurement) takes
#                           it exclusively, and `E2E_LOCK=bench` takes it
#                           exclusively for every suite and builds inside
#                           that hold
#   - not beside our own  : a suite starts only while no other job of
#                           ours holds the host's lock (run_held), waited
#                           for up to E2E_MAX_WAIT; the Dock may hold as
#                           many of its tiles at once as its file says
#                           (dock_tiles_max)
#   - bounded             : each suite is stopped after E2E_SUITE_TIMEOUT
#                           seconds (default 900); the build after
#                           E2E_BUILD_TIMEOUT (default 1800)
#   - GUI                 : runs in the ssh login of the console user,
#                           which reaches the console session; checked
#                           before the copy (check_console), or windows
#                           cannot be created at all
#   - its own sandbox     : each suite gets a state dir of its own, and
#                           the cleanup stops the processes recorded
#                           there and nothing else on the host
#   - honest              : exits 1 when any suite failed; 3 when none
#                           failed but a measuring suite was read next
#                           to other work or never got the exclusive
#                           lock, which is neither a pass nor a
#                           regression: take it again on a quiet host
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
  test-all-shells-exit.sh
  test-multi-window.sh
  test-panel-behaviour.sh
  test-pane-new-window.sh
  test-commands.sh
  test-theme-switch.sh
  test-language-switch.sh
  test-bundle-lander.sh
  test-single-instance.sh
  test-two-sandboxes.sh
  test-session-ownership.sh
  test-one-app-record.sh
  test-coalition-reclaim.sh
  test-reseat-bound.sh
  test-reopen-after-quit.sh
  test-quit-after-update.sh
  test-quit-right-after-update.sh
  test-cc-profile-switch.sh
  test-codex-profile-switch.sh
  test-codex-refresh.sh
  test-codex-refresh-real.sh
  test-codex-follow.sh
  test-agent-handoff.sh
  test-peerbus.sh
  test-zombie-watch.sh
  test-model-guard.sh
  test-real-update.sh
  test-negative-update.sh
  test-adversarial-update.sh
  test-state-map.sh
  test-idle-wakeups.sh
  test-idle-exit-codes.sh
  test-awake-cost.sh
)

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REMOTE_NAME="${REMOTE_NAME:-e2e-marspot}"
HOST="${HOST:-$(source "$ROOT/bin/_lib.sh" && marspot_host gui "$REMOTE_NAME")}"
# remote-sync always lands in ~/work/<name>, so that is where the
# run has to look.
REMOTE_DIR="work/$REMOTE_NAME"
SUITE_TIMEOUT="${E2E_SUITE_TIMEOUT:-900}"
BUILD_TIMEOUT="${E2E_BUILD_TIMEOUT:-1800}"
MAX_WAIT="${E2E_MAX_WAIT:-3600}"
RUN_ID="$(date +%H%M%S)$$"
# Names the remote process group of whatever is running right now, so
# an interrupted run can stop exactly that and nothing else.
REMOTE_PIDF="/tmp/marspot-e2e-$RUN_ID.pgid"
# one run per remote copy: two runs would sync over each other's tree
LOCK_LOCAL="/tmp/marspot-e2e-remote.$REMOTE_NAME.lock.d"

SUITES=("$@")
(( ${#SUITES[@]} )) || SUITES=("${DEFAULT_SUITES[@]}")
# shellcheck source=bin/_e2e-suites.sh
source "$ROOT/bin/_e2e-suites.sh"
# shellcheck source=bin/_lock-mark.sh
source "$ROOT/bin/_lock-mark.sh"
# a hold's note in `bench-lock status`: ours, lasting E2E_UNTIL or its limit
until_for() { lock_until "${E2E_UNTIL:-$1}" e2e "$REMOTE_NAME"; }
MOST=() # each suite's Dock tile maximum, read only here, before the copy
for suite in "${SUITES[@]}"; do
  [[ -f "$ROOT/bin/$suite" ]] || { echo "no such suite: bin/$suite" >&2; exit 2; }
  most="$(dock_tiles_max "$ROOT/bin/$suite")" || { echo "refusing to run bin/$suite" >&2; exit 2; }
  MOST+=("$most")
done

if ! mkdir "$LOCK_LOCAL" 2>/dev/null; then
  if [[ -f "$LOCK_LOCAL/pid" ]] && kill -0 "$(cat "$LOCK_LOCAL/pid")" 2>/dev/null; then
    echo "test-e2e-remote already running locally (pid $(cat "$LOCK_LOCAL/pid"))" >&2
    exit 1
  fi
  rm -rf "$LOCK_LOCAL" && mkdir "$LOCK_LOCAL"
fi
echo "$$" > "$LOCK_LOCAL/pid"

# shellcheck source=bin/_e2e-remote-lib.sh
source "$ROOT/bin/_e2e-remote-lib.sh"
# shellcheck source=bin/_e2e-host-share.sh
source "$ROOT/bin/_e2e-host-share.sh"

# Told to stop (Ctrl-C, a TERM, the terminal or ssh going away): stop
# the suite running now, clean its sandbox, and start no other.
current_sd=""
interrupted() {
  echo "==> interrupted; stopping the run on $HOST"
  stop_remote
  [[ -n $current_sd ]] && reap_sandbox "$current_sd"
  drop_run_files
  exit 130
}
trap interrupted INT TERM HUP
trap 'rm -rf "$LOCK_LOCAL"' EXIT

# the remote copy has no .git; the binaries built there and a suite that
# records readings name the commit by this, read before and after the sync
commit="$("$ROOT/bin/_git-sha.sh")"
check_console || exit 1
remote_sync "$HOST" --name "$REMOTE_NAME"
commit="$("$ROOT/bin/_git-sha.sh" "$commit")"

# With E2E_LOCK=bench every suite builds inside its own exclusive hold
# (bin/_build_then.sh): built under a shared hold first, it would wait
# behind every queued measurement and then queue again itself.  By
# default the build is one shared hold here, and a measuring suite then
# queues once for the exclusive hold it is read under.
if [[ ${E2E_LOCK:-heavy} != bench ]]; then
  echo "==> building release on $HOST"
  remote_tracked "source ~/.cargo/env && cd ~/$REMOTE_DIR && export MARSPOT_GIT_SHA=$commit && \
    /usr/local/bin/bench-lock heavy --max-wait $MAX_WAIT $(until_for "$BUILD_TIMEOUT") bin/_bounded.sh $BUILD_TIMEOUT \
    sh -c 'cargo build --release ${E2E_FEATURES:+--features $E2E_FEATURES} --bin marspot-shell --bin marspot-core --bin marspot-session \
      && cargo build --release -p marspot-peer'" \
    >"$LOCK_LOCAL/build.log" 2>&1 || { tail -5 "$LOCK_LOCAL/build.log"; echo "FAIL: release build on $HOST" >&2; exit 1; }
  tail -2 "$LOCK_LOCAL/build.log"
  # macOS assesses a freshly built executable on its first exec, for an
  # unbounded time on a busy host; pay that here, outside every suite's
  # stopwatch.  Bounded, and waited for so a signal is not held up by it.
  ssh "${SSH_CLEANUP[@]}" "$HOST" "cd ~/$REMOTE_DIR && bin/_bounded.sh 120 sh -c \
    'for b in marspot-shell marspot-core marspot-session; do \
    MARSPOT_NO_REDIRECT=1 target/release/\$b --version >/dev/null 2>&1; done'" </dev/null &
  SSH_PID=$!
  wait "$SSH_PID" || true
  SSH_PID=""
fi

note_known_leftovers

# A measuring suite's knobs (`IDLE_*`) are set here and read there, and
# so is anything listed in E2E_ENV ("K=V K=V", e.g. a soak's duration).
fwd=""
while IFS= read -r kv; do fwd+=" $(printf '%q' "$kv")"; done < <(env | grep -E '^IDLE_[A-Z_]+=' || true)
for kv in ${E2E_ENV:-}; do fwd+=" $(printf '%q' "$kv")"; done
fwd+=" E2E_COMMIT=$commit MARSPOT_GIT_SHA=$commit"

rc=0
unmeasured=0
results=()
i=0
for suite in "${SUITES[@]}"; do
  i=$((i + 1))
  echo
  echo "════ $suite on $HOST ════"
  if [[ ${E2E_LOCK:-heavy} == bench ]]; then
    lock="bench --max-wait $MAX_WAIT $(until_for $((BUILD_TIMEOUT + SUITE_TIMEOUT)))"
    run="bin/_build_then.sh $BUILD_TIMEOUT $SUITE_TIMEOUT ${E2E_FEATURES:--} $suite"
  elif is_measurement "$suite"; then
    # built above already; a reading taken next to other heavy work
    # would only ever come back busy
    lock="bench --max-wait $MAX_WAIT $(until_for "$SUITE_TIMEOUT")"
    run="bin/_bounded.sh $SUITE_TIMEOUT bin/$suite"
  else
    lock="heavy --max-wait $MAX_WAIT $(until_for "$SUITE_TIMEOUT")"
    run="bin/_bounded.sh $SUITE_TIMEOUT bin/$suite"
  fi
  # Short: the session sockets live under it and sun_path is 104 bytes.
  current_sd="/tmp/msp-e2e-$RUN_ID-$i"
  # the suite's scratch goes under the same prefix (dev_tmp), which is
  # all check_leftovers looks at besides the remote tree
  suite_env=" E2E_TMP=$current_sd-"
  # A window suite serves the recorded toolbar icons instead of starting
  # a drawer (one app registration less per launch); a measurement reads
  # the app as it ships.
  if [[ $suite == test-* ]] && ! is_measurement "$suite"; then
    suite_env+=" MARSPOT_TEST_ICON_MASKS=\$PWD/tests/fixtures/appkit/icon-masks.bin"
  fi
  keeps_dock_tile "$suite" || suite_env+=" MARSPOT_TEST_NO_DOCK=1"
  # a measure-active run that timed out can leave iTerm2 up: quit it and
  # SIGTERM the pids the run wrote down (a no-op when it cleaned up)
  pre_clean=""
  [[ $suite == measure-active.sh ]] && pre_clean="bin/_active_iterm.sh close $current_sd/iterm.pids; "
  start=$(date +%s)
  # the suite, its cleanup and the leftover check, all in one hold, taken
  # only while no other job of ours holds the lock (sets suite_rc)
  run_held "$(locked_cmd "$current_sd" "$lock" "$run" "$pre_clean" "$suite_env$fwd" "${MOST[i - 1]}")"
  if (( suite_rc == OWN_JOB_RC )); then
    echo "FAIL: $suite not started: a job of ours held $HOST's lock for ${MAX_WAIT}s; starting no other suite" >&2
    results+=("FAIL  $suite  (not started: a job of ours held $HOST's lock)")
    drop_sandbox "$current_sd"
    rc=1
    break
  fi
  secs=$(( $(date +%s) - start ))
  # for a stopped run, which skipped both inside the hold
  if [[ $suite == measure-active.sh ]]; then
    remote_gui "cd ~/$REMOTE_DIR && bin/_active_iterm.sh close $current_sd/iterm.pids" || true
  fi
  reap_sandbox "$current_sd"
  # nothing a suite opened may outlive it: the check made inside the hold
  left_rc=0
  left_ls="$(read_clean "$current_sd")" || left_rc=$?
  # the suite's own status, where the hold's command gave the check's
  if [[ $left_ls =~ (^|$'\n')suite_rc=([0-9]+) ]]; then
    suite_rc=${BASH_REMATCH[2]}
  fi
  left_why=""
  if (( left_rc != 0 )); then
    echo "  runner after $suite:"
    printf '%s\n' "$left_ls" | sed 's/^/    /'
    case $left_rc in
      4) left_why="left something on $HOST (Dock tile, app record or TCC request)" ;;
      *) left_why="what $HOST has left could not be read" ;;
    esac
    # never restarted from here: the runner's console is devops's to restore
    [[ $left_ls != *$'\n'restore:* ]] \
      || echo "==> the Dock on $HOST keeps a tile $suite left; ask devops to run: devops console restore $HOST" >&2
  fi
  # a suite that records readings leaves them in the remote tree, which
  # nothing else copies back
  if [[ $suite == test-awake-cost.sh ]]; then
    mkdir -p "$ROOT/bench/results"
    scp -q "${SSH_CLEANUP[@]}" "${HOST}:${REMOTE_DIR}/bench/results/awake-cost-*.json" "$ROOT/bench/results/" </dev/null || true
  fi
  if [[ $suite == measure-active.sh ]]; then
    mkdir -p "$ROOT/bench/results"
    scp -q "${SSH_CLEANUP[@]}" "${HOST}:${REMOTE_DIR}/bench/results/active-*.json" "$ROOT/bench/results/" </dev/null || true
  fi
  record_result
  current_sd=""
  if (( left_rc != 0 )) && ! stopped_from_outside "$suite_rc"; then
    echo "==> $suite $left_why; starting no other suite" >&2
    break
  fi
  if stopped_from_outside "$suite_rc"; then
    echo "==> $suite was stopped from outside (exit $suite_rc); starting no other suite" >&2
    stop_remote
    break
  fi
done
drop_run_files

echo
echo "════ summary ($HOST) ════"
printf '  %s\n' "${results[@]}"
(( rc == 0 && unmeasured )) && rc=3
exit $rc

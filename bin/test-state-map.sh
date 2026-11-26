#!/usr/bin/env bash
#
# Every file a run of the app leaves under its state root has a row in
# the shell's state map (`host::state_map`), and the fact files are only
# ever appended to.
#
# The sandbox app is started with one pane, typed into twice, given time
# for its plugins to tick, copied halfway, quit the way the Dock quits
# it and stopped.  Then the shell's test `state_map_covers_every_file`
# lists the state root against the table and compares every fact file
# with its copy from halfway: the copy has to be where the file starts.
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app
# is never touched.  Exits 0 on pass, non-zero on the first failure.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

RUNLOG="$(dev_run_log test-state-map)"
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
export MARSPOT_SESSION_BIN="$ROOT/target/release/marspot-session" MARSPOT_CORE_BIN="$ROOT/target/release/marspot-core"
BEFORE="$(mktemp -d /tmp/msp-state-map.XXXXXX)"

cleanup() {
  dev_kill_shell_core
  rm -rf "${BEFORE:?}"
}
trap cleanup EXIT
fail() {
  echo "FAIL: $*"
  echo "  --- last 30 lines of $APPLOG:"
  tail -30 "$APPLOG" 2>/dev/null | cut -c1-240 | sed 's/^/    /'
  exit 1
}
count() { local n; n=$(grep -c $'\t'"$1"$'\t' "$APPLOG" 2>/dev/null); echo "${n:-0}"; }

for b in "$SHELL_BIN" "$MARSPOT_SESSION_BIN" "$MARSPOT_CORE_BIN"; do
  [[ -x $b ]] || fail "$b is not built"
done

# the shell's test binary, built before the app runs so the build's
# load is not part of the run
echo "==> building the shell's tests"
"$ROOT/bin/_bounded.sh" 1500 cargo test -q --no-run --bin marspot-shell >>"$RUNLOG" 2>&1 \
  || fail "the shell's tests did not build (see $RUNLOG)"

dev_kill_shell_core
dev_wipe_state
rm -rf "${MARSPOT_STATE_DIR:?}/sessions" "${MARSPOT_STATE_DIR:?}/retired"
mkdir -p "$MARSPOT_STATE_DIR/logs"

echo "==> starting the sandbox app"
env nohup "$SHELL_BIN" >>"$RUNLOG" 2>&1 </dev/null &
dev_record_pid $!
disown
for (( i = 0; i < 300; i++ )); do
  SID="$("$SHELL_BIN" --panes 2>/dev/null | awk 'NR > 1 { print $1; exit }')"
  [[ -n $SID ]] && break
  sleep 0.2
done
[[ -n ${SID:-} ]] || fail "the app showed no pane within 60 s"
echo "    pane $SID"

"$SHELL_BIN" --send "$SID" $'echo state-map-one\n' >/dev/null || fail "pane $SID was not typed into"
sleep 20

echo "==> copying the state root halfway"
( cd "$MARSPOT_STATE_DIR" && find . -type f ) | while IFS= read -r f; do
  mkdir -p "$BEFORE/$(dirname "$f")" && cp "$MARSPOT_STATE_DIR/$f" "$BEFORE/$f"
done
echo "    $(find "$BEFORE" -type f | wc -l | tr -d ' ') files"

"$SHELL_BIN" --send "$SID" $'echo state-map-two\n' >/dev/null || fail "pane $SID was not typed into the second time"
sleep 10

echo "==> quitting"
SHELL_PID="$(dev_shell_pid)"
[[ -n $SHELL_PID ]] || fail "no shell to quit"
q=$(count SHELL_QUIT_CLEANUP)
answered="$(osascript -l JavaScript -e "ObjC.import('AppKit');
  \$.NSRunningApplication.runningApplicationWithProcessIdentifier($SHELL_PID).terminate" 2>&1)"
[[ $answered == true ]] || fail "the quit request was not taken: $answered"
for (( i = 0; i < 50; i++ )); do (( $(count SHELL_QUIT_CLEANUP) > q )) && break; sleep 0.2; done
(( $(count SHELL_QUIT_CLEANUP) > q )) || fail "the app did not act on the quit within 10 s"
sleep 2
dev_kill_shell_core || fail "the sandbox's processes did not stop"

echo "==> checking the state root against the state map"
MARSPOT_STATE_MAP_ROOT="$MARSPOT_STATE_DIR" MARSPOT_STATE_MAP_BEFORE="$BEFORE" \
  "$ROOT/bin/_bounded.sh" 600 cargo test -q --bin marspot-shell -- --ignored --exact --nocapture \
  host::state_map::tests::state_map_covers_every_file \
  || fail "the state root has files the state map does not know, or a fact file was rewritten (above)"

echo "PASS: every file of the run is in the state map; fact files only appended to"

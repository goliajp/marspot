#!/usr/bin/env bash
#
# The app takes the sessions' coalition back from a program that was
# given it.
#
# A shell that takes the place of a dead one registers a moment after
# the dead shell's record went, and a program a pane starts in that
# moment is given the coalition instead: LaunchServices then lists
# every session under that program, and goes on doing so after the
# program has exited, on a dead record that only the same program run
# again takes over (measured; see `reclaim.rs`).
#
# The moment is too short to hit on purpose, so the state is made the
# long way: with nobody able to start a shell, the shell is killed, the
# dead record is taken over, and the program in pane 1 registers.  Then
# the app is opened, and the walk goes through what can happen next:
#   held      while the program runs: the app is one living record, and
#             its log names the pid it is waiting for
#   kept      the program has exited: its dead record keeps the
#             coalition, and the app's log says so
#   taken     the program is run once more, from outside the panes: its
#             old record goes, and without anything being asked of the
#             app the three checks of the walk hold again (see
#             test-session-ownership.sh), with the app one record
#
# A failed check is recorded and the walk goes on; the exit code is
# non-zero when any failed.  Sandbox-only: the installed app is never
# touched.

set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

OWN_SCRATCH=msp-rcl OWN_BUNDLE_ID=com.goliajp.marspot.e2e-reclaim OWN_BUNDLE_NAME=MarspotReclaimTest
source "$ROOT/bin/_ownership-lib.sh"

answers() { local i; for (( i = 0; i < 150; i++ )); do L1="$(dev_shell_pid)"; [[ -n $L1 ]] && "$SHELL_BIN" --panes >/dev/null 2>&1 && return 0; sleep 0.2; done; return 1; }
said() { grep $'\t'"$1"$'\t' "$APPLOG" | tail -1 | grep -q $'\t'"pid=$2"; }

# Leaves the two sessions listed under the program pane 1 started, and
# no shell running.  Sets GUI.
give_away() {
  local i w
  rm -f "$MARSPOT_STATE_DIR/owner"; sleep 0.5
  kill -9 "$L1"; sleep 1
  [[ -z "$(dev_shell_pid)" ]] || { bad "could not make the state: a shell is running although nothing could start one"; return 1; }
  open -n -g -j -W "$APP" --args --ls-refresh 2>/dev/null; sleep 0.5
  rm -f "$SB/gui.pid"; ( echo 600 > "$SB/go" ) & w=$!
  wait_file "$SB/gui.pid" 10 || { kill "$w" 2>/dev/null; bad "could not make the state: the launcher did not start the program"; return 1; }
  GUI="$(cat "$SB/gui.pid")"; STRANGERS+=("$GUI")
  for (( i = 0; i < 100; i++ )); do [[ -n "$(ls_record "$GUI")" ]] && break; sleep 0.1; done
  sleep 0.7
  if ls_coalition_pids "$GUI" | grep -q -x "$S1"; then echo "    made: the program $GUI in pane $P1 has the sessions' coalition $(coalition "$S1")"
  else bad "could not make the state: the program was not given the coalition ($(ls_record "$GUI" | grep -o 'coalition:.*' | head -1))"; return 1; fi
}

start_walk
state "fresh launch"; check_one_record

STATE="the program runs"; echo; echo "── $STATE: a program a pane started has the sessions' coalition ──"
if give_away; then
  open_app; reattached; sleep 2
  STATE="the program runs"
  check_one_record
  if said SHELL_COALITION_HELD_BY "$GUI"; then ok "held: the app waits for the program $GUI to exit"
  else bad "held: the app does not say that it waits for the program $GUI that has its coalition"; fi

  STATE="the program has exited"; echo; echo "── $STATE ──"
  kill "$GUI" 2>/dev/null; sleep 4
  check_one_record
  if said SHELL_COALITION_KEPT_BY_DEAD "$GUI"; then ok "kept: the app says the dead record of $GUI keeps the coalition"
  else bad "kept: the app does not say what became of the coalition when the program $GUI exited"; fi

  STATE="the program was run again"; echo; echo "── $STATE ──"
  before="$L1"
  "$PROBE" gui 3 & again=$!; STRANGERS+=("$again")
  for (( i = 0; i < 150; i++ )); do
    L1="$(dev_shell_pid)"
    [[ -n $L1 && $L1 != "$before" ]] && [[ " $(ls_coalition_pids "$L1" | tr '\n' ' ')" == *" $S1 "* ]] && break
    sleep 0.1
  done
  sleep 3; answers || bad "no app is running after the coalition was free to take"
  [[ $L1 != "$before" ]] || bad "no new shell took the place of $before"
  reattached
  state "the app took the coalition back"; check_one_record
fi

echo
echo "════ summary ════"
if (( ${#FAILED[@]} == 0 )); then
  echo "PASS: the app took the sessions' coalition back"
  exit 0
fi
printf '  %s\n' "${FAILED[@]}"
echo "FAIL: ${#FAILED[@]} check(s) failed"
exit 1

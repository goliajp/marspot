#!/usr/bin/env bash
#
# A coalition kept by a record that LaunchServices no longer lists does
# not send the app round and round.
#
# A program a pane started can be given the sessions' coalition (see
# test-coalition-reclaim.sh for how), and keeps it on a dead record
# when it exits.  `lsappinfo` stops listing some such records while
# they still keep the coalition: one of a program that registered with
# no Dock icon is gone from the list once the app has been opened.  A
# shell that took "listed under nothing" for "free" had a new shell
# started to be given the coalition, which was refused it and did the
# same: three windows one after another on an update, stopped only by
# the limit on how often a session starts a shell.
#
# The state is made the long way, as in test-coalition-reclaim.sh, with
# a program that registers with no Dock icon.  Then, in that state:
#   opened    the app is opened: one shell takes the place of the one
#             LaunchServices started, and no other after it; the one
#             shell it asks for is refused the coalition and leaves
#             without a window
#   updated   the shell moves to a staged image of itself: the same
#             process, one look at the coalition, one shell asked for
#             that leaves the same way
#   appeared  another application starts: the app does not look again
#   reopened  quit and opened again: the same process, no shell started
# and in each of them the app is one living record with its windows,
# the sessions name the shell as the one that answers, and a program a
# pane starts is still not given the sessions.
#
# RESEAT_DRY=1 builds the bundle and the probe, says what a run would
# do, and starts and signals nothing.
#
# A failed check is recorded and the walk goes on; the exit code is
# non-zero when any failed.  Sandbox-only: the installed app is never
# touched.

set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUN_START="$(date +%s)"
source "$ROOT/bin/_dev-sandbox.sh"

OWN_SCRATCH=msp-rsb OWN_BUNDLE_ID=com.goliajp.marspot.e2e-reseat OWN_BUNDLE_NAME=MarspotReseatTest
source "$ROOT/bin/_ownership-lib.sh"
trap 'exit 1' HUP INT TERM

if [[ -n ${RESEAT_DRY:-} ]]; then
  echo "dry: would open $APP with two panes, kill its shell, have pane 1 start"
  echo "dry: '$SB/ownagent agent 200', end that program, open the app, stage and trigger"
  echo "dry: an update, run '$SB/ownother gui 2', quit and open the app"
  echo "dry: process limit on file for the probe: $(cat "$SB/nproc")"
  echo "PASS: dry run, nothing was started and no signal was sent"
  exit 0
fi

answers() { local i; for (( i = 0; i < 150; i++ )); do L1="$(dev_shell_pid)"; [[ -n $L1 ]] && "$SHELL_BIN" --panes >/dev/null 2>&1 && return 0; sleep 0.2; done; return 1; }

# What the log has counted so far, to be compared with `since`.
mark() {
  M_HANDOVER=$(count SHELL_HANDOVER); M_STARTED=$(count OWNER_STARTED); M_CORES=$(count shell.core.spawning)
  M_REFUSED=$(count SHELL_COALITION_REFUSED); M_UNLISTED=$(count SHELL_COALITION_UNLISTED); M_ASKED=$(count SHELL_COALITION_FREE)
}
since() { # sets D_*
  D_HANDOVER=$(( $(count SHELL_HANDOVER) - M_HANDOVER )); D_STARTED=$(( $(count OWNER_STARTED) - M_STARTED ))
  D_CORES=$(( $(count shell.core.spawning) - M_CORES )); D_REFUSED=$(( $(count SHELL_COALITION_REFUSED) - M_REFUSED ))
  D_UNLISTED=$(( $(count SHELL_COALITION_UNLISTED) - M_UNLISTED )); D_ASKED=$(( $(count SHELL_COALITION_FREE) - M_ASKED ))
  echo "    since the last state: $D_STARTED shell(s) started by a session, of which $D_REFUSED left without a window because it was not given the coalition; $D_HANDOVER hand-over(s); $D_CORES start(s) of a core (a window each); a shell asked for in the running one's place $D_ASKED time(s)"
}
expect() { # what, got, wanted
  if [[ $2 == "$3" ]]; then ok "$1: $2"; else bad "$1: $2, expected $3"; fi
}
# What holds in every state here: one record, the shell named by the
# sessions, a stranger kept out, the two windows the walk began with.
held() {
  local w
  check_one_record; check_owner; check_stranger
  w="$(dev_windows_of "$(dev_shell_pid)" | grep -c '^0 ')"
  expect "windows the app shows" "$w" 2
}
update_app() { # sets D_*
  local updates; updates=$(count SHELL_SELF_UPDATE)
  printf '%s\n' "$APP" > "$MARSPOT_STATE_DIR/bundle"
  stage_shell
  "$SHELL_BIN" --trigger >/dev/null || bad "the update trigger found no shell"
  wait_count SHELL_SELF_UPDATE $((updates + 1)) 60 || bad "the shell did not move to the staged image"
  sleep 10; answers || bad "no app is running after the update"
  reattached
  since
}

# Leaves the sessions' coalition kept by the dead record of a program
# with no Dock icon, and no shell running.  Sets AG and C.
make_kept() {
  local i w
  C="$(coalition "$S1")"
  rm -f "$MARSPOT_STATE_DIR/owner"; sleep 0.5
  sig -KILL "$L1"; sleep 1
  [[ -z "$(dev_shell_pid)" ]] || { bad "could not make the state: a shell is running although nothing could start one"; return 1; }
  open -n -g -j -W "$APP" --args --ls-refresh 2>/dev/null; sleep 0.5
  rm -f "$SB/gui.pid"; ( echo "200 agent" > "$SB/go" ) & w=$!
  wait_file "$SB/gui.pid" 10 || { kill "$w" 2>/dev/null; bad "could not make the state: the launcher did not start the program"; return 1; }
  AG="$(cat "$SB/gui.pid")"
  for (( i = 0; i < 100; i++ )); do [[ -n "$(ls_record "$AG")" ]] && break; sleep 0.1; done
  sleep 0.7
  if ls_coalition_pids "$AG" | grep -q -x "$S1"; then echo "    made: the program $AG ($(ls_type "$AG")) in pane $P1 has the sessions' coalition $C"
  else bad "could not make the state: the program was not given the coalition ($(ls_record "$AG" | grep -o 'coalition:.*' | head -1))"; return 1; fi
  sig -TERM "$AG"; wait_gone "$AG" 5 || { bad "could not make the state: the program did not exit"; return 1; }
  sleep 1
  echo "    the program has exited; its record: $(ls_record "$AG" | grep -o -E 'type="[A-Za-z]*"|\(exited[a-z-]*\)|coalition: [0-9]*' | tr '\n' ' ')"
}
# Whether any record lists the coalition C, by `lsappinfo`.
listed_anywhere() { lsappinfo list | grep -q -E "^ *coalition: $C( |\$)"; }

start_walk
state "fresh launch"; check_one_record

STATE="opened"; echo; echo "── $STATE: the coalition is kept by a dead record, and the app is opened ──"
if make_kept; then
  mark
  open_app; reattached
  # a shell that is refused the coalition looks again three seconds on,
  # and one that asks for another does so within seconds of its start
  sleep 12; answers || bad "no app is running after it was opened"
  STATE="opened"
  if [[ -n "$(ls_record "$AG")" ]]; then echo "    (the dead record of the program $AG is still listed: $(ls_record "$AG" | grep -o -E '\(exited[a-z-]*\)|coalition: [0-9]*' | tr '\n' ' '))"
  elif listed_anywhere; then bad "could not make the state: the coalition $C is listed under a record again"
  else echo "    made: no record lists the coalition $C, and none is left of the program $AG"; fi
  since
  expect "hand-overs since the app was opened (the shell LaunchServices started, to one in the sessions' coalition)" "$D_HANDOVER" 1
  expect "shells a session started (the one that took over, and one that was refused the coalition and left)" "$D_STARTED" 2
  expect "shells that left because they were not given the coalition" "$D_REFUSED" 1
  expect "starts of a core" "$D_CORES" 1
  expect "times a shell was asked for in the running one's place" "$D_ASKED" 1
  keep="$L1"; sleep 5; answers
  [[ $L1 == "$keep" ]] && ok "the shell $keep stays" || bad "the shell changed again, from $keep to $L1"
  held

  STATE="updated"; echo; echo "── $STATE: the shell moves to a staged image of itself ──"
  mark; keep="$L1"
  # as more than two minutes later: a shell that was refused the
  # coalition is not asked for again sooner
  rm -f "$MARSPOT_STATE_DIR/owner.reseats"
  update_app
  [[ $L1 == "$keep" ]] && ok "the update kept the process $keep" || bad "the update changed the shell from $keep to $L1"
  expect "hand-overs" "$D_HANDOVER" 0
  expect "shells a session started (one, refused the coalition)" "$D_STARTED" 1
  expect "shells that left because they were not given the coalition" "$D_REFUSED" 1
  expect "starts of a core (the update's own)" "$D_CORES" 1
  expect "times the updated shell looked, asked, and said the coalition is not free" "$D_UNLISTED" 1
  held

  STATE="appeared"; echo; echo "── $STATE: another application starts ──"
  mark
  "$SB/ownother" gui 2 & other=$!
  sleep 5; wait "$other" 2>/dev/null
  since
  expect "hand-overs" "$D_HANDOVER" 0
  expect "shells a session started" "$D_STARTED" 0
  expect "times a shell was asked for in the running one's place" "$D_ASKED" 0
  expect "starts of a core" "$D_CORES" 0

  STATE="reopened"; echo; echo "── $STATE: quit with the sessions kept, and opened again ──"
  mark; keep="$L1"; residents=$(count SHELL_RESIDENT)
  quit_app
  if expect_resident "$residents"; then
    open_app; reattached
    sleep 8; answers || bad "no app is running after it was opened again"
    since
    [[ $L1 == "$keep" ]] && ok "the same process $keep is the app again" || bad "opening the app changed the shell from $keep to $L1"
    expect "hand-overs" "$D_HANDOVER" 0
    expect "starts of a core" "$D_CORES" 1
    expect "shells a session started (one was refused less than two minutes ago)" "$D_STARTED" 0
    expect "times a shell was asked for in the running one's place" "$D_ASKED" 0
    held
  fi
fi

echo
echo "════ summary ════"
if (( ${#FAILED[@]} == 0 )); then
  echo "PASS: a coalition kept by an unlisted record leaves the app as it is"
  exit 0
fi
printf '  %s\n' "${FAILED[@]}"
echo "FAIL: ${#FAILED[@]} check(s) failed"
exit 1

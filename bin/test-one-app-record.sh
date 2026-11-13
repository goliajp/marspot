#!/usr/bin/env bash
#
# The app is one record in LaunchServices, under the bundle, alive, in
# every state -- and an update makes it so where it is not.
#
# LaunchServices lists a process it did not start by the file it runs
# when it registers.  A shell that a session started from the copy in
# `binaries/current/` was a second application, "marspot-shell", with a
# Dock tile of its own; clicking the app's icon then started the bundle
# beside it, which stood aside and left a dead record, "running in the
# background", with every session listed as related.  Nothing a person
# could do short of closing every pane put that right.
#
# The app is opened through LaunchServices, as a bundle of its own
# name, with two panes (see test-session-ownership.sh for what the
# walk checks in each state).  On top of those checks, in each state:
#   one record  LaunchServices lists exactly one record for the app:
#               the running shell, alive, under the bundle's id and
#               path, and none under the name of the shell's file
#
# States: launched, updated in place, quit and opened, a second
# instance asked for; the shell killed while the record of who answers
# names the copy outside the bundle; then the three states an update
# has to heal, each made here and then updated:
#   S1  the living shell is listed under the name of its file (healed
#       by an update, and once more by being opened after an install)
#   S2  S1, and a dead record of the app is left listing the sessions
#   S3  S2, with sessions in two coalitions, one of them listed under
#       the dead record of a program a pane started
#
# A failed check is recorded and the walk goes on; the exit code is
# non-zero when any failed.  Sandbox-only: the installed app is never
# touched.

set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

OWN_SCRATCH=msp-one OWN_BUNDLE_ID=com.goliajp.marspot.e2e-one-record OWN_BUNDLE_NAME=MarspotOneRecordTest
source "$ROOT/bin/_ownership-lib.sh"

OUTSIDE="$MARSPOT_STATE_DIR/binaries/current/marspot-shell"
one() { state "$1"; check_one_record; }
answers() { local i; for (( i = 0; i < 150; i++ )); do L1="$(dev_shell_pid)"; [[ -n $L1 ]] && "$SHELL_BIN" --panes >/dev/null 2>&1 && return 0; sleep 0.2; done; return 1; }

# The record of who answers, rewritten in place: what a session starts
# is the copy outside the bundle.
name_outside() {
  python3 - "$MARSPOT_STATE_DIR/owner" "$OUTSIDE" <<'PY'
import sys
path, exe = sys.argv[1:]
lines = ['exe=' + exe if l.startswith('exe=') else l for l in open(path).read().split('\n')]
with open(path, 'r+') as f:
    f.seek(0); f.truncate(); f.write('\n'.join(lines))
PY
}

# What a second launch asks of the shell that holds the lock, without a launch.
ask_forward() {
  python3 - "$MARSPOT_STATE_DIR/front.sock" <<'PY'
import os, socket, sys
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1]); s.sendall(b"%d\n" % os.getpid()); s.close()
PY
}

# Leaves a shell from the copy outside the bundle answering for the
# sessions, with no window, listed under the name of its file.  A
# session started long enough ago has nothing that says where the
# bundle is; one started by a shell that had been updated does, and
# the shell it starts finds its way back when it is opened.
misname() {
  mkdir -p "$(dirname "$OUTSIDE")"; [[ -x $OUTSIDE ]] || cp "$SHELL_BIN" "$OUTSIDE"
  rm -f "$MARSPOT_STATE_DIR/bundle"
  # the walk kills shells faster than a session is allowed to start them
  : > "$MARSPOT_STATE_DIR/owner.spawn"
  name_outside; sleep 0.5
  kill_shell || { bad "no shell took the place of the one that was killed"; return 1; }
  sleep 1.5
  if app_records | grep -q "^$L1"$'\t'"alive"$'\t'"marspot-shell"$'\t'; then
    echo "    made: the shell $L1 that answers is listed as \"marspot-shell\", not as the app"
  else
    bad "could not make the state: the shell from the copy outside the bundle is listed as: $(app_records | tr '\t\n' ' ;')"
    return 1
  fi
}

# Opens the misnamed shell without a launch, and says whether it is
# still misnamed once it is the app.
open_misnamed() {
  ask_forward || { bad "the shell a session started does not listen for a second launch"; return 1; }
  sleep 8; answers || { bad "the shell a session started did not open when asked"; return 1; }
  sleep 1
  app_records | grep -q $'\t'"alive"$'\t'"marspot-shell"$'\t'
}

# Leaves a dead record of the app that lists the sessions as related.
leave_dead_record() {
  open -n -g -j -W "$APP" --args --ls-refresh 2>/dev/null; sleep 1
  if app_records | grep -q $'\t'"dead"$'\t'; then echo "    made: a dead record of the app is left: $(app_records | grep $'\tdead\t' | tr '\t\n' ' ;')"
  else echo "    (no dead record of the app could be left; the state is the one before)"; fi
}

# What an install does, then the update itself.
update_app() {
  local updates; updates=$(count SHELL_SELF_UPDATE)
  printf '%s\n' "$APP" > "$MARSPOT_STATE_DIR/bundle"
  stage_shell
  "$SHELL_BIN" --trigger >/dev/null || bad "the update trigger found no shell"
  wait_count SHELL_SELF_UPDATE $((updates + 1)) 60 || bad "the shell did not move to the staged image"
  # a shell that was refused the coalition looks again three seconds on
  sleep 8; answers || { bad "the app did not come back after the update"; open_app; }
  sleep 1
}

start_walk
one "fresh launch"

keep="$L1"; updates=$(count SHELL_SELF_UPDATE)
stage_shell
"$SHELL_BIN" --trigger >/dev/null || bad "the update trigger found no shell"
wait_count SHELL_SELF_UPDATE $((updates + 1)) 60 || bad "the shell did not move to the staged image"
answers; reattached
[[ $L1 == "$keep" ]] || bad "the update of a shell listed as the app changed its pid ($keep to $L1)"
one "shell updated in place"

residents=$(count SHELL_RESIDENT)
quit_app
if expect_resident "$residents"; then one "quit, sessions kept"; fi
open_app; reattached
one "opened again"

open_app -n
sleep 2
one "a second instance asked for"

# the record of who answers names the copy outside the bundle, as one
# written by a shell from before the bundle was put on file does
name_outside; sleep 0.5
if kill_shell; then
  sleep 1.5
  one "shell killed, the record naming a copy outside the bundle"
  open_app; reattached
  one "opened after that"
else
  STATE="shell killed, the record naming a copy outside the bundle"; echo; echo "── $STATE ──"; bad "no shell took the dead one's place"
  open_app; reattached
fi

STATE="S1"; echo; echo "── $STATE: the living shell is listed under the name of its file ──"
if misname; then
  if open_misnamed; then update_app; else echo "    (a session knew where the bundle is: the shell became the app by name when it was opened)"; fi
  reattached
  one "S1 healed"
fi

# the same shell, opened by a launch through LaunchServices as a click
# on the Dock icon is: the bundle is started beside it and stands aside
STATE="S1, opened"; echo; echo "── $STATE: the misnamed shell is opened through LaunchServices after an install ──"
if misname; then
  printf '%s\n' "$APP" > "$MARSPOT_STATE_DIR/bundle"
  open_app; sleep 8; answers || bad "the app did not come up"
  reattached
  one "S1 opened through LaunchServices"
fi

STATE="S2"; echo; echo "── $STATE: and a dead record of the app lists the sessions ──"
if misname; then
  leave_dead_record
  if open_misnamed; then update_app; else echo "    (a session knew where the bundle is: the shell became the app by name when it was opened)"; fi
  reattached
  one "S2 healed"
fi

# Sessions in two coalitions.  The two of the walk stay where they are,
# unable to start a shell; the app is opened elsewhere and gets a third
# pane there; a program started in pane 1 is given the first coalition
# and exits, leaving its dead record with the two sessions on it.
STATE="S3"; echo; echo "── $STATE: sessions in two coalitions, one under a dead stranger's record ──"
had="$("$SHELL_BIN" --panes 2>/dev/null | awk 'NR>1 {print $1}' | sort -n | tr '\n' ' ')"
rm -f "$MARSPOT_STATE_DIR/owner" "$MARSPOT_STATE_DIR"/sessions/*/guard
sleep 0.5
old="$L1"; kill -9 "$old"; sleep 1
[[ -z "$(dev_shell_pid)" ]] || bad "a shell is running although nothing could start one"
open_app --env MARSPOT_DEV_EXTRA_WINDOWS=1
for (( i = 0; i < 100; i++ )); do
  sids="$("$SHELL_BIN" --panes 2>/dev/null | awk 'NR>1 {print $1}' | sort -n | tr '\n' ' ')"
  [[ $(wc -w <<<"$sids") -gt $(wc -w <<<"$had") ]] && break; sleep 0.3
done
P3="$(comm -13 <(tr ' ' '\n' <<<"$had" | grep . | sort) <(tr ' ' '\n' <<<"$sids" | grep . | sort) | head -1)"
if [[ -z $P3 ]]; then
  bad "could not make the state: no new pane came up (panes before: $had now: $sids)"
else
  S3="$(session_pid "$P3")"; Z3="$(shell_of "$P3")"
  CA="$(coalition "$S1")"; CB="$(coalition "$S3")"
  [[ $CA != "$CB" && "$(coalition "$L1")" == "$CB" ]] || bad "could not make the state: sessions in $CA and $CB, the app in $(coalition "$L1")"
  # the first coalition is given to the program once no dead record of
  # the app has it; each instance of the app takes one over, the oldest first
  given=""
  for (( t = 0; t < 4; t++ )); do
    rm -f "$SB/gui.pid"; ( echo 600 > "$SB/go" ) & w=$!
    wait_file "$SB/gui.pid" 10 || { kill "$w" 2>/dev/null; bad "could not make the state: the launcher did not start the program"; break; }
    gui="$(cat "$SB/gui.pid")"; STRANGERS+=("$gui")
    for (( i = 0; i < 100; i++ )); do [[ -n "$(ls_record "$gui")" ]] && break; sleep 0.1; done
    sleep 0.7
    ls_coalition_pids "$gui" | grep -q -x "$S1" && given=1
    kill "$gui" 2>/dev/null; sleep 1.5
    [[ -n $given ]] && break
    open -n -g -j -W "$APP" --args --ls-refresh 2>/dev/null; sleep 0.5
  done
  if [[ -n $given ]]; then echo "    made: sessions $S1 $S2 (coalition $CA) are on the dead record of the program pane $P1 started; session $S3 is in $CB"
  else echo "    (the program a pane started was not given the first coalition)"; fi
  if misname; then
    leave_dead_record
    if open_misnamed; then update_app; fi
    for p in "$S1" "$S2" "$S3" "$Z1" "$Z2" "$Z3" "$BY" "$LAUNCHER"; do alive "$p" || bad "pid $p of a pane is gone"; done
    now="$("$SHELL_BIN" --panes 2>/dev/null | awk 'NR>1 {print $1}' | sort -n | tr '\n' ' ')"
    [[ $now == "$sids" ]] || bad "the app did not attach to the sessions it had (had: $sids now: $now)"
    check_one_record
    listed=" $(ls_coalition_pids "$L1" | tr '\n' ' ')"
    if [[ $listed == *" $S3 "* && $listed == *" $Z3 "* ]]; then ok "record: the shell's record lists the session and the pane's shell of its own coalition"
    else bad "record: the shell's record (pid $L1) does not list session $S3 and shell $Z3 of its own coalition; it lists:${listed:- nothing}"; fi
  fi
fi

echo
echo "════ summary ════"
if (( ${#FAILED[@]} == 0 )); then
  echo "PASS: the app was one record, the running shell under the bundle, in every state"
  exit 0
fi
printf '  %s\n' "${FAILED[@]}"
echo "FAIL: ${#FAILED[@]} check(s) failed"
exit 1

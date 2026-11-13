#!/usr/bin/env bash
#
# Every session is in a coalition the app answers for, in every state
# the app can be in.
#
# LaunchServices holds one registered application answerable for the
# members of a coalition.  With that application gone and the app
# started again elsewhere, the coalition belongs to the next program
# that registers from inside it: on 2026-11-11 an emulator started in a
# pane had every session on its record, and quitting it from the Dock
# signalled them all.  So the shell stays with its sessions.
#
# The app is opened through LaunchServices, as a bundle of its own
# name, with two panes.  A launcher left in pane 1 starts, on request,
# a program that registers as a foreground application; pane 2 holds a
# background process.  Then the app is walked through its states (the
# `state` calls below name them), and in each one:
#   owner     the last SESSION_OWNER line of each session names the
#             running shell, never `none`
#   stranger  the program started in pane 1 is listed by LaunchServices
#             with no process of the app or of a pane, while it runs
#             and after it has exited
#   record    the shell's own LaunchServices record lists every
#             session and every pane's shell
#
# Then: twenty quits leave one shell, the same pid and coalition, no
# larger; the end of the login session ends the shell; closing the last
# kept session ends a resident shell and leaves nothing behind.
#
# A failed check is recorded and the walk goes on; the exit code is
# non-zero when any failed.  Nothing here clicks a Dock icon or asks
# for a permission.  Sandbox-only: the installed app is never touched.

set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

OWN_SCRATCH=msp-own OWN_BUNDLE_ID=com.goliajp.marspot.e2e-ownership OWN_BUNDLE_NAME=MarspotOwnershipTest
source "$ROOT/bin/_ownership-lib.sh"

start_walk

state "fresh launch"
residents=$(count SHELL_RESIDENT)
quit_app
if expect_resident "$residents"; then
  [[ $L1 == "$FIRST" && "$(coalition "$L1")" == "$C0" ]] || bad "the resident is not the process that was quit"
  state "quit, sessions kept"
  FOOT_FIRST="$("$PROBE" footprint "$L1")"
  echo "    resident shell: phys_footprint ${FOOT_FIRST} KiB"
else
  STATE="quit, sessions kept"; echo; echo "── $STATE ──"; echo "    (the shell exited; nothing answers for the sessions until the app is opened)"
fi

open_app
reattached
[[ $L1 == "$FIRST" ]] || bad "opening the app started another shell ($L1) instead of bringing back $FIRST"
state "opened again"

open_app -n
sleep 2
[[ "$(shells_running | grep -c .)" == 1 ]] || bad "a second instance left $(shells_running | grep -c .) shells running"
state "a second instance asked for, app running"

residents=$(count SHELL_RESIDENT)
quit_app
if expect_resident "$residents"; then
  keep="$L1"
  open_app -n
  reattached
  [[ $L1 == "$keep" ]] || bad "a second instance replaced the resident shell $keep with $L1"
  [[ "$(shells_running | grep -c .)" == 1 ]] || bad "a second instance of a quit app left $(shells_running | grep -c .) shells"
  state "a second instance asked for, app quit"
else
  open_app; reattached
fi

# the shell moves to a staged image of itself
keep="$L1"; updates=$(count SHELL_SELF_UPDATE)
stage_shell
"$SHELL_BIN" --trigger >/dev/null || bad "the update trigger found no shell"
wait_count SHELL_SELF_UPDATE $((updates + 1)) 60 || bad "the shell did not move to the staged image"
for (( i = 0; i < 150; i++ )); do "$SHELL_BIN" --panes >/dev/null 2>&1 && break; sleep 0.2; done
L1="$(dev_shell_pid)"; reattached
[[ $L1 == "$keep" ]] || bad "the shell's update changed its pid ($keep to $L1)"
state "shell updated in place"

residents=$(count SHELL_RESIDENT)
quit_app
if expect_resident "$residents"; then
  # a staged shell waits for the app to be opened: the resident does
  # not replace its image while it waits for that event
  updates=$(count SHELL_SELF_UPDATE); waits=$(count SHELL_UPDATE_WAITS)
  stage_shell
  kill -USR1 "$L1"
  wait_count SHELL_UPDATE_WAITS $((waits + 1)) 60 || bad "the resident shell did not note the staged shell"
  [[ "$(dev_shell_pid)" == "$keep" ]] && (( $(count SHELL_SELF_UPDATE) == updates )) || bad "the resident replaced its image while quit"
  state "shell staged while quit"
  open_app; reattached
  (( $(count SHELL_SELF_UPDATE) == updates + 1 )) && [[ ! -e "$MARSPOT_STATE_DIR/binaries/pending/marspot-shell" ]] \
    || bad "opening the app did not take the staged shell"
  [[ $L1 == "$keep" ]] || bad "taking the staged shell changed the pid ($keep to $L1)"
  state "opened as the staged shell"
else
  open_app; reattached
fi

# the sessions move to another build of themselves
resumed=$(count L3_EXECV_RESUMED)
mkdir -p "$MARSPOT_STATE_DIR/binaries/current"
cp "$SESSION_BIN" "$SB/session-other"
python3 - "$SB/session-other" <<'PY'
import sys
p = sys.argv[1]; b = bytearray(open(p, 'rb').read())
i = b.find(b'MARSPOT_FP=') + len(b'MARSPOT_FP=')
b[i] = ord('1') if b[i] == ord('0') else ord('0')
open(p, 'wb').write(b)
PY
codesign --force --sign - "$SB/session-other" 2>/dev/null && codesign -v "$SB/session-other" || die "could not sign the other session build"
mv "$SB/session-other" "$MARSPOT_STATE_DIR/binaries/current/marspot-session"
kill -TERM "$S1" "$S2"
wait_count L3_EXECV_RESUMED $((resumed + 2)) 60 || bad "the sessions did not move to the other build ($(( $(count L3_EXECV_RESUMED) - resumed )) of 2)"
sleep 1
state "sessions updated in place"

core="$(dev_core_pids | head -1)"; boots=$(count CORE_BOOT)
kill -9 "$core"
wait_count CORE_BOOT $((boots + 1)) 60 || bad "no core came back after the core was killed"
for (( i = 0; i < 150; i++ )); do "$SHELL_BIN" --panes >/dev/null 2>&1 && break; sleep 0.2; done
reattached
state "core killed and restarted"

# the shell dies: a session starts one where the sessions are
if kill_shell; then
  started="$(grep $'\tOWNER_STARTED\t' "$APPLOG" | tail -1 | cut -f2)"
  up="$(grep $'\tSHELL_RESIDENT\t' "$APPLOG" | tail -1 | cut -f2)"
  echo "    the shell was killed at $KILLED_AT; a session started another $(( started - KILLED_AT )) ms later, which answered $(( up - KILLED_AT )) ms after the kill"
  [[ "$(coalition "$L1")" == "$(coalition "$S1")" ]] || bad "the shell a session started is in coalition $(coalition "$L1"), the sessions in $(coalition "$S1")"
  sleep 1.5
  rec_at="$(grep $'\tSHELL_RESIDENT_RECORD\t' "$APPLOG" | tail -1 | cut -f2)"; taken="$(grep $'\tSHELL_COALITION_TAKEN\t' "$APPLOG" | tail -1)"
  echo "    the dead shell's record was displaced by $(( rec_at - KILLED_AT )) ms after the kill; the new shell was registered $(sed -n 's/.*registered_after_ms=\([0-9]*\).*/\1/p' <<<"$taken") ms after that, listed under: $(sed -n 's/.*listed_under=\([A-Za-z]*\).*/\1/p' <<<"$taken")"
  state "shell killed"
  keep="$L1"
  open_app; reattached
  [[ $L1 == "$keep" ]] || bad "opening the app started another shell ($L1) beside the one a session started ($keep)"
  state "opened after the shell was killed"
else
  STATE="shell killed"; echo; echo "── $STATE ──"; bad "no shell took the dead one's place"
  open_app; reattached
  state "opened after the shell was killed"
fi

# the shell dies with nobody able to answer, and the user opens the app:
# it comes up outside the sessions' coalition and must not stay there
rm -f "$MARSPOT_STATE_DIR/owner"
sleep 0.5
old="$L1"; handovers=$(count SHELL_HANDOVER)
kill -9 "$old"; sleep 1
if [[ -n "$(dev_shell_pid)" ]]; then bad "a shell is running although the record of who answers was removed"; fi
open_app; reattached
if (( $(count SHELL_HANDOVER) > handovers )); then ok "the shell that was opened outside the sessions' coalition handed over to one inside it"
else bad "the shell that was opened did not hand over to one in the sessions' coalition"; fi
[[ "$(coalition "$L1")" == "$(coalition "$S1")" ]] || bad "the app runs in coalition $(coalition "$L1"), its sessions are in $(coalition "$S1")"
state "opened from outside the sessions' coalition"

# twenty quits and openings: one process throughout.  The graphics driver hands
# back what the closed windows held over the next seconds: read when it has
settled_footprint() { local f i; for (( i = 0; i < 50; i++ )); do f=$("$PROBE" footprint "$1"); (( f <= RESIDENT_KIB )) && break; sleep 0.2; done; echo "$f"; }
RESIDENT_KIB=32768
keep="$L1"; c_keep="$(coalition "$L1")"; n_ok=0; foots=""
for (( round = 1; round <= 20; round++ )); do
  residents=$(count SHELL_RESIDENT)
  quit_app
  wait_count SHELL_RESIDENT $((residents + 1)) 10 || break
  foots+=" $(settled_footprint "$L1")"
  open_app
  [[ $L1 == "$keep" && "$(shells_running | tr '\n' ' ')" == "$keep " ]] || break
  n_ok=$round
done
STATE="quit and opened twenty times"; echo; echo "── $STATE ──"
if (( n_ok == 20 )); then
  ok "one shell throughout: pid $keep, coalition $(coalition "$L1")"
  [[ "$(coalition "$L1")" == "$c_keep" && "$(coalition "$S1")" == "$c_keep" ]] || bad "a coalition changed over twenty quits"
  echo "    resident footprint after each quit, KiB:$foots; descriptors open: $(lsof -p "$L1" 2>/dev/null | grep -c .)"
  most=$(tr ' ' '\n' <<<"$foots" | sort -n | tail -1)
  (( most <= RESIDENT_KIB )) || bad "a resident shell was still at ${most} KiB ten seconds after its quit (budget ${RESIDENT_KIB})"
else
  bad "round $((n_ok + 1)) of 20 did not come back as the same single shell (shells: $(shells_running | tr '\n' ' '))"
fi
reattached
state "after twenty quits"

# the end of the login session: the quit event says why, and the shell goes
STATE="login session ends"; echo; echo "── $STATE ──"
old="$L1"; exits_before=$(count SHELL_RESIDENT_ENTER)
if "$PROBE" logout-quit "$old" 2>"$SB/logout.err"; then
  if wait_gone "$old" 10; then ok "the shell exited on the quit event of a logout, sessions kept"
  else bad "the shell stayed after the quit event of a logout"; fi
  (( $(count SHELL_RESIDENT_ENTER) == exits_before )) || bad "the shell became a resident on the quit event of a logout"
else
  bad "the quit event of a logout could not be sent: $(cat "$SB/logout.err")"
fi
# the sessions were not ended, as on a real logout launchd would end
# them: one of them starts a shell again
for (( i = 0; i < 50; i++ )); do L1="$(dev_shell_pid)"; [[ -n $L1 && $L1 != "$old" ]] && break; sleep 0.1; done

# the last kept session ends: the resident shell goes too, and that
# exit is the one an armed bundle lander gets
STATE="last kept session closed"; echo; echo "── $STATE ──"
L1="$(dev_shell_pid)"
mkdir -p "$MARSPOT_STATE_DIR/pending-bundle"
printf '#!/bin/sh\nwhile kill -0 "$1" 2>/dev/null; do sleep 0.2; done\necho "landed after $1" > "%s/landed.txt"\n' \
  "$MARSPOT_STATE_DIR" > "$MARSPOT_STATE_DIR/pending-bundle/land.sh"
chmod +x "$MARSPOT_STATE_DIR/pending-bundle/land.sh"
if [[ -n $L1 ]]; then
  [[ -z "$(dev_core_pids)" ]] || { quit_app; sleep 1; }
  kept=($(dev_session_pids))
  (( ${#kept[@]} == 2 )) || bad "expected the two sessions of the walk to be kept, found ${#kept[@]}"
  for (( i = 0; i < ${#kept[@]}; i++ )); do
    alive "$L1" || bad "the resident shell exited while a session was still kept"
    kill -HUP "${kept[$i]}"; wait_gone "${kept[$i]}" 10 || bad "session ${kept[$i]} did not close"
  done
  if wait_gone "$L1" 10; then ok "the resident shell exited with the last session"
  else bad "the resident shell is still running with no session left"; fi
  if wait_file "$MARSPOT_STATE_DIR/landed.txt" 5; then ok "the bundle lander was handed that exit: $(cat "$MARSPOT_STATE_DIR/landed.txt")"
  else bad "the armed bundle lander did not land after the resident shell exited"; fi
  sleep 1
  [[ -z "$(shells_running)" ]] || bad "a shell is running with no session left: $(shells_running | tr '\n' ' ')"
  left=""; for p in "$Z1" "$Z2" "$S1" "$S2"; do alive "$p" && left+=" $p"; done
  [[ -z $left ]] || bad "left running after the sessions closed:$left"
else
  bad "no shell was running to see the sessions end"
fi

echo
echo "════ summary ════"
if (( ${#FAILED[@]} == 0 )); then
  echo "PASS: every session was answered for by a living shell of the app in every state"
  exit 0
fi
printf '  %s\n' "${FAILED[@]}"
echo "FAIL: ${#FAILED[@]} check(s) failed"
exit 1

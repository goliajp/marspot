#!/usr/bin/env bash
#
# Update the shell in place, then quit the app: the quit is taken, every
# time.
#
# The shell updates itself by replacing its own image (`execv`, same
# pid).  Apple events reach an app through appleeventsd, which keeps one
# registration per process: the old image's goes when its connection
# breaks, and the new image registers again once AppKit is up.  The two
# arrive at appleeventsd on different connections and it handles them
# on different threads; when the new registration is handled first, the
# old connection's end then removes it, and the app gets no Apple events
# at all -- a quit from the Dock or from another app is refused for as
# long as the process lives.
#
# The shell asks appleeventsd once before AppKit starts, which narrows
# that race, and a second after its first frame has a child of itself
# ask whether the shell can be sent an event; "no such process" means
# the registration was lost, and the shell replaces its image again.
#
# The app is opened through LaunchServices, as a bundle of its own name.
# Then ROUNDS times (default 30): stage the shell as a pending update,
# trigger it, wait for the new image's window and its registration
# check, and ask the app to quit the way the Dock does
# (`NSRunningApplication terminate`); then open it again (the resident
# shell replaces its image on that path too) and quit once more the same
# way.  A refused quit is the failure; nothing is retried.  At the end:
# the checking children never registered with appleeventsd or
# LaunchServices, every request tccd got about them was a preflight that
# could not prompt (read from a `log stream` the run keeps), tccd has no
# decision on record for the bundle, and the shell kept its coalition.
#
# QAU_BUSY=N starts N busy loops for the run (each ends itself after
# QAU_BUSY_SECS, default 900, and is also stopped by pid at the end):
# the race is lost far more often on a loaded machine.  Take the host's
# bench lock when using it (E2E_LOCK=bench).  QAU_COUNT=1 is for
# counting, not for passing: a refused quit is recorded, the sandbox app
# is stopped and opened again, and the run goes on to the next round;
# at the end it reports how many quits were refused.
#
# Sandbox-only: the installed app is never touched.  Exits 0 on pass.

set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

RUN_START="$(date +%s)"
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
SB="$(mktemp -d /tmp/msp-qau.XXXXXX)"
APP="$SB/Marspot.app"
BID="com.goliajp.marspot.e2e-quit-after-update"
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
ROUNDS="${QAU_ROUNDS:-30}"
BUSY="${QAU_BUSY:-0}"
BUSY_SECS="${QAU_BUSY_SECS:-900}"
COUNTING="${QAU_COUNT:-0}"
BUSY_PIDS=()
TCC_STREAM_PID=""

cleanup() {
  local p
  for p in ${BUSY_PIDS[@]+"${BUSY_PIDS[@]}"}; do kill -TERM "$p" 2>/dev/null; done
  [[ -n $TCC_STREAM_PID ]] && kill -TERM "$TCC_STREAM_PID" 2>/dev/null
  dev_kill_shell_core
  "$LSREGISTER" -u "$APP" >/dev/null 2>&1
  rm -rf "$SB"
}
trap cleanup EXIT
aed_since() { # epoch seconds, pid: what appleeventsd did with that pid since then
  /usr/bin/log show --start "$(date -r "$1" '+%Y-%m-%d %H:%M:%S')" --info --debug --style compact \
    --predicate "process == \"appleeventsd\" AND eventMessage CONTAINS \"$2\"" 2>/dev/null \
    | grep -E 'kAECheckinApp|releasing app|reconnect' \
    | sed -E 's/appleeventsd\[[^]]*\] \[com.apple.appleevents:connection\] CONNECTION: //; s/port=xpc object:<[^>]*>[^,]*,//' \
    | cut -c1-230 | sed 's/^/    aed| /'
}
fail() {
  echo "FAIL: $*"
  echo "  --- last 25 lines of $APPLOG:"
  tail -25 "$APPLOG" 2>/dev/null | cut -c1-260 | sed 's/^/    /'
  exit 1
}

for b in "$SHELL_BIN" "$ROOT/target/release/marspot-core" "$ROOT/target/release/marspot-session"; do
  [[ -x $b ]] || fail "$b is not built"
done
sw_vers | tr '\n' ' '; echo
"$ROOT/bin/build-app-bundle.sh" --bin-dir "$ROOT/target/release" --out "$APP" >/dev/null || fail "could not assemble the sandbox bundle"
plutil -replace CFBundleIdentifier -string "$BID" "$APP/Contents/Info.plist"
plutil -replace CFBundleName -string MarspotQuitAfterUpdateTest "$APP/Contents/Info.plist"

count() { local n; n=$(grep -c $'\t'"$1"$'\t' "$APPLOG" 2>/dev/null); echo "${n:-0}"; }
ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time*1000'; }
answers() { local i; for (( i = 0; i < 300; i++ )); do "$SHELL_BIN" --panes >/dev/null 2>&1 && return 0; sleep 0.05; done; return 1; }
wait_more() { # marker, count before, seconds
  local i
  for (( i = 0; i < $3 * 20; i++ )); do (( $(count "$1") > $2 )) && return 0; sleep 0.05; done
  return 1
}
app_env=(--env "MARSPOT_STATE_DIR=$MARSPOT_STATE_DIR" --env "MARSPOT_SESSION_BIN=$ROOT/target/release/marspot-session"
  --env "MARSPOT_CORE_BIN=$ROOT/target/release/marspot-core" --env MARSPOT_DEV_SANDBOX=1 --env MARSPOT_MANUAL_UPDATE_ONLY=1)

dev_kill_shell_core
rm -rf "$MARSPOT_STATE_DIR"; mkdir -p "$MARSPOT_STATE_DIR/logs"
open -g "$APP" "${app_env[@]}" || fail "LaunchServices did not open the sandbox bundle"
for (( i = 0; i < 300; i++ )); do L1="$(dev_shell_pid)"; [[ -n $L1 ]] && "$SHELL_BIN" --panes >/dev/null 2>&1 && break; sleep 0.2; done
[[ -n ${L1:-} ]] && answers || fail "the app never came up"
wait_more WINDOW_FIRST_PRESENT 0 30 || fail "the app showed no window"
sleep 1

# tccd's access requests as they happen: its detailed lines do not stay
# in the log for as long as a run lasts
/usr/bin/log stream --style compact --info --debug \
  --predicate 'process == "tccd" AND eventMessage CONTAINS "kTCCServiceAppleEvents"' >"$SB/tcc.stream" 2>&1 &
TCC_STREAM_PID=$!

if (( BUSY > 0 )); then
  for (( i = 0; i < BUSY; i++ )); do
    "$ROOT/bin/_bounded.sh" "$BUSY_SECS" sh -c 'while :; do :; done' >/dev/null 2>&1 &
    BUSY_PIDS+=($!)
  done
  sleep 3
fi
echo "==> app $L1 up; $ROUNDS rounds of update, quit, open, quit; load: $(sysctl -n vm.loadavg)"

quits=0 refused=0
# the coalition LaunchServices lists the shell's record with
coalition() { lsappinfo info "$(lsappinfo find pid="$1")" 2>/dev/null | awk '/coalition:/ {print $2; exit}'; }
C0="$(coalition "$L1")"
[[ -n $C0 ]] || fail "LaunchServices lists no coalition for the shell $L1"
L1S=("$L1")
restart() { # the app of a refused quit is stopped and opened afresh
  local shown
  dev_kill_shell_core
  shown=$(count WINDOW_FIRST_PRESENT)
  open -g "$APP" "${app_env[@]}" || fail "LaunchServices did not open the sandbox bundle again"
  for (( i = 0; i < 300; i++ )); do L1="$(dev_shell_pid)"; [[ -n $L1 ]] && "$SHELL_BIN" --panes >/dev/null 2>&1 && break; sleep 0.2; done
  answers || fail "the app never came back after a restart"
  wait_more WINDOW_FIRST_PRESENT "$shown" 30 || fail "the restarted app showed no window"
  L1S+=("$L1")
  sleep 1
}
# The shell checks, a second after its first frame, that it is still
# registered for Apple events, and replaces its image once more when it
# is not.  A quit is asked once that check has come out registered.
# `checks` is counted before the step that replaces the image, so only
# a check of the new image counts: a check left over from an earlier
# image once let a quit go to an image whose check later found it
# unregistered.  Every image a round starts is waited for, so none is
# left over.
checks=0
checked() { # label
  local i
  for (( i = 0; i < 600; i++ )); do (( $(count SHELL_AE_CHECKED) > checks )) && break; sleep 0.05; done
  (( $(count SHELL_AE_CHECKED) > checks )) || fail "[$1] the shell never reported its registration check"
}
quit() { # label; returns 1 when the quit was refused (counting only)
  local q ans since
  since="$(date +%s)"
  q=$(count SHELL_QUIT_CLEANUP)
  quits=$((quits + 1))
  ans="$(osascript -l JavaScript -e "ObjC.import('AppKit');
    \$.NSRunningApplication.runningApplicationWithProcessIdentifier($L1).terminate" 2>&1)"
  if [[ $ans != true ]]; then
    echo "  refused [$1] (load $(sysctl -n vm.loadavg | awk '{print $2}')): $ans"
    sleep 1; aed_since "$((since - 30))" "$L1"
    (( COUNTING )) || fail "[$1] the quit request was not taken: $ans"
    refused=$((refused + 1))
    restart
    return 1
  fi
  wait_more SHELL_QUIT_CLEANUP "$q" 10 || fail "[$1] the app did not act on the quit"
}

for (( r = 1; r <= ROUNDS; r++ )); do
  shown=$(count WINDOW_FIRST_PRESENT); updates=$(count SHELL_SELF_UPDATE)
  checks=$(count SHELL_AE_CHECKED)
  mkdir -p "$MARSPOT_STATE_DIR/binaries/pending"
  cp "$SHELL_BIN" "$MARSPOT_STATE_DIR/binaries/pending/marspot-shell"
  "$SHELL_BIN" --trigger >/dev/null || fail "[round $r] the update trigger found no shell"
  wait_more SHELL_SELF_UPDATE "$updates" 60 || fail "[round $r] the shell did not move to the staged image"
  answers || fail "[round $r] the updated shell does not answer"
  [[ "$(dev_shell_pid)" == "$L1" ]] || fail "[round $r] the update changed the shell's pid"
  wait_more WINDOW_FIRST_PRESENT "$shown" 30 || fail "[round $r] the updated shell showed no window"
  checked "round $r, after the update"
  quit "round $r, after the update" || continue

  shown=$(count WINDOW_FIRST_PRESENT)
  checks=$(count SHELL_AE_CHECKED)
  open -g "$APP" 2>"$SB/err" || fail "[round $r] open was refused: $(head -1 "$SB/err")"
  answers || fail "[round $r] the app did not come back"
  [[ "$(dev_shell_pid)" == "$L1" ]] || fail "[round $r] the app came back as another process"
  wait_more WINDOW_FIRST_PRESENT "$shown" 30 || fail "[round $r] the app came back and showed no window"
  checked "round $r, after opening again"
  quit "round $r, after opening again" || continue

  shown=$(count WINDOW_FIRST_PRESENT)
  checks=$(count SHELL_AE_CHECKED)
  open -g "$APP" 2>"$SB/err" || fail "[round $r] open was refused: $(head -1 "$SB/err")"
  answers || fail "[round $r] the app did not come back"
  wait_more WINDOW_FIRST_PRESENT "$shown" 30 || fail "[round $r] the app came back and showed no window"
  checked "round $r, after the last open"
done

if (( COUNTING )); then
  echo "  --- the shell's image changes, for matching against appleeventsd:"
  grep -E $'\t(SHELL_AE_SETTLED|SHELL_UPDATE_APPLY|SHELL_RESIDENT_RESUME|SHELL_IDENTITY|SHELL_SELF_UPDATE)\t' "$APPLOG" \
    | cut -f1,7,9- | cut -c1-200 | sed 's/^/    log| /'
  echo "  --- appleeventsd for $L1:"
  aed_since "$RUN_START" "$L1"
fi

# The checking child asks appleeventsd about the shell and must never
# become an app, nor make anyone ask the user: no Apple-event
# registration, no LaunchServices check-in, and every access request
# tccd gets about it is a preflight that may not prompt (appleeventsd
# looks its permission up, and that is all); and the shell keeps its
# coalition.
kids="$(grep -E $'\t(SHELL_AE_CHECKED|shell.ae_check.lost)\t' "$APPLOG" | grep -o 'child=[0-9]*' | cut -d= -f2 | sort -u)"
nkids=$(grep -c . <<<"$kids")
since="$(date -r "$RUN_START" '+%Y-%m-%d %H:%M:%S')"
aed_in="$(/usr/bin/log show --start "$since" --info --debug --style compact \
  --predicate 'process == "appleeventsd" AND eventMessage CONTAINS "kAECheckinApp"' 2>/dev/null)"
ls_in="$(/usr/bin/log show --start "$since" --info --debug --style compact \
  --predicate 'process == "launchservicesd" AND (eventMessage CONTAINS "applicationCheckIn" OR eventMessage CONTAINS "leader pid")' 2>/dev/null)"
sleep 1
kill -TERM "$TCC_STREAM_PID" 2>/dev/null; wait "$TCC_STREAM_PID" 2>/dev/null; TCC_STREAM_PID=""
tcc_in="$(cat "$SB/tcc.stream")"
registered=0
for k in $kids; do
  n=$(grep -cE "peer-pid=$k got event" <<<"$aed_in")
  m=$(grep -cE "client=$k |leader pid $k " <<<"$ls_in")
  # each tccd request about the child, as one line: an access request
  # that is not a preflight, or that may prompt, counts against it
  t=$(awk -v k="target_token={pid:$k," '
    /REQUEST_MSG: / { block = ""; inside = 1 }
    inside { block = block $0 }
    inside && /^}/ {
      inside = 0
      if (index(block, k) && index(block, "TCCAccessRequest") &&
          (!index(block, "preflight=true") || !index(block, "target_prompt=0"))) n++
    }
    END { print n + 0 }' <<<"$tcc_in")
  if (( n + m + t > 0 )); then
    echo "  the checking child $k: $n registrations with appleeventsd, $m with LaunchServices, $t tccd requests that could prompt"
    registered=$((registered + 1))
  fi
done
# the same search finds the shell's own registrations, or it proves nothing
(( $(grep -cE "client=${L1S[0]} " <<<"$ls_in") > 0 && $(grep -cE "peer-pid=${L1S[0]} got event" <<<"$aed_in") > 0 )) \
  || fail "the logs do not show the shell's own registrations: the children's cannot be judged from them"
echo "  registration checks: $(count SHELL_AE_CHECKED) registered, $(count shell.ae_check.lost) found unregistered," \
  "$(count SHELL_AE_REPLACE) replacements made, $(count shell.ae_check.gave_up) given up; $nkids checking children, $registered of them registered or could have prompted;" \
  "$(grep -c "target_token={pid:" <<<"$tcc_in") tccd access requests in all"
(( nkids > 0 )) || fail "no checking child ran"
# requests about any checking child: none seen means the stream measured nothing
seen=$(awk -v kids=" $(tr '\n' ' ' <<<"$kids")" '
  /REQUEST_MSG: / { b = ""; i = 1 }
  i { b = b $0 }
  i && /^}/ {
    i = 0
    if (index(b, "TCCAccessRequest") && match(b, /target_token=\{pid:[0-9]+,/)) {
      pid = substr(b, RSTART + 18, RLENGTH - 19)
      if (index(kids, " " pid " ")) n++
    }
  }
  END { print n + 0 }' <<<"$tcc_in")
echo "  tccd access requests about the checking children: $seen"
(( seen > 0 )) || fail "tccd's stream holds no access request about a checking child: whether a check could have prompted was not measured"
if [[ "${L1S[*]}" == "$L1" ]]; then
  c="$(coalition "$L1")"
  [[ $c == "$C0" ]] || fail "the shell's coalition changed from $C0 to '$c' over the run"
  echo "  the shell $L1 is listed in coalition $C0 at the start and at the end"
fi
(( registered == 0 )) || fail "$registered checking children registered, or made a request that could prompt"
# tccd answers each check from what it has on record for the bundle; an
# answer of 1 is "nothing on record".  A prompt, or a record written for
# one, would turn up as another value on a later check.  tccd's detailed
# lines do not all survive a long run, so these are what is left.
auths="$(grep -E "Handling access request: kTCCServiceAppleEvents:$BID" <<<"$tcc_in" | grep -oE 'authValue: [0-9]+' | sort | uniq -c | tr -s ' ' | tr '\n' ';')"
echo "  tccd's answers about the bundle's Apple events: ${auths:-none left in the log}"
! grep -qE 'authValue: [^1]' <<<"$auths" || fail "tccd has a decision on record for the bundle's Apple events: $auths"
echo
if (( refused > 0 )); then
  echo "FAIL: $refused of $quits quits refused"
  exit 1
fi
echo "PASS: $quits quits, $ROUNDS after an update in place and $ROUNDS after opening again, none refused"

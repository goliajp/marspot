#!/usr/bin/env bash
#
# Every session is in a coalition the app answers for, in every state
# the app can be in.
#
# macOS puts an app launch and all it starts into one coalition, for
# life, and LaunchServices holds one registered application answerable
# for a coalition's members.  When that application is gone and the app
# has been started again elsewhere, the coalition belongs to the next
# program that registers from inside it: on 2026-11-11 that was an
# emulator started in a pane, its record listed every session, and
# quitting it from the Dock signalled them all.  Sessions outlive a
# quit, so the app's shell has to as well: it stays with no window, and
# when it dies anyway a session starts another one where they are.
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
# larger than after the first; the end of the login session ends the
# shell with sessions kept; closing the last kept session ends a
# resident shell and leaves nothing behind.
#
# A failed check is recorded and the walk goes on, so one run says
# which states hold and which do not; the exit code is non-zero when
# any failed.  Nothing here clicks a Dock icon or asks for a
# permission.  Sandbox-only: MARSPOT_STATE_DIR is redirected and the
# installed app is never touched.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"
SB="$(mktemp -d /tmp/msp-own.XXXXXX)"
APP="$SB/Marspot.app"
PROBE="$SB/ownprobe"
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
FAILED=()
STRANGERS=()

die() {
  echo "FAIL: $*"
  echo "  --- last 40 lines of $APPLOG:"
  tail -40 "$APPLOG" 2>/dev/null | cut -c1-300 | sed 's/^/    /'
  exit 1
}
bad() { echo "  FAIL  [$STATE] $*"; FAILED+=("[$STATE] $*"); }
ok() { echo "  ok    [$STATE] $*"; }

cleanup() {
  local p
  for p in ${STRANGERS[@]+"${STRANGERS[@]}"} $(cat "$SB/launcher.pid" "$SB/by.pid" 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
  dev_kill_shell_core
  # a shell a session started while the sandbox was going down
  sleep 0.5; dev_kill_shell_core
  "$LSREGISTER" -u "$APP" >/dev/null 2>&1
  rm -rf "$SB"
}
trap cleanup EXIT

sw_vers | tr '\n' ' '; echo

for b in "$SHELL_BIN" "$CORE_BIN" "$SESSION_BIN" "$ROOT/target/release/marspot-peer"; do
  [[ -x $b ]] || { echo "FAIL: $b is not built (cargo build --release, and -p marspot-peer)"; exit 1; }
done

# A bundle of its own name: LaunchServices must not take it for the
# installed app, nor the installed app for it.
"$ROOT/bin/build-app-bundle.sh" --bin-dir "$ROOT/target/release" --out "$APP" >/dev/null \
  || { echo "FAIL: could not assemble the sandbox bundle"; exit 1; }
plutil -replace CFBundleIdentifier -string com.goliajp.marspot.e2e-ownership "$APP/Contents/Info.plist"
plutil -replace CFBundleName -string MarspotOwnershipTest "$APP/Contents/Info.plist"
APP_EXE="$APP/Contents/MacOS/marspot-shell"

# Reads a pid's coalition and footprint, registers as a foreground
# application, and sends a quit event the way the login window does.
cat > "$SB/ownprobe.m" <<'EOF'
#import <AppKit/AppKit.h>
#include <libproc.h>
struct coalinfo { uint64_t id[2]; uint64_t reserved[3]; };
int main(int argc, char **argv) {
    if (argc < 3) return 2;
    if (!strcmp(argv[1], "coalition")) {
        struct coalinfo ci = {0};
        if (proc_pidinfo(atoi(argv[2]), 20, 0, &ci, sizeof ci) != sizeof ci) return 1;
        printf("%llu\n", ci.id[1]);
        return 0;
    }
    if (!strcmp(argv[1], "footprint")) {
        struct rusage_info_v4 ri = {0};
        if (proc_pid_rusage(atoi(argv[2]), RUSAGE_INFO_V4, (rusage_info_t *)&ri)) return 1;
        printf("%llu\n", ri.ri_phys_footprint / 1024);
        return 0;
    }
    if (!strcmp(argv[1], "gui")) {
        @autoreleasepool {
            NSApplication *app = [NSApplication sharedApplication];
            [app setActivationPolicy:NSApplicationActivationPolicyRegular];
            [app finishLaunching];
            NSDate *end = [NSDate dateWithTimeIntervalSinceNow:atof(argv[2])];
            while ([end timeIntervalSinceNow] > 0) {
                NSEvent *e = [app nextEventMatchingMask:NSEventMaskAny untilDate:[NSDate dateWithTimeIntervalSinceNow:0.5]
                                                 inMode:NSDefaultRunLoopMode dequeue:YES];
                if (e) [app sendEvent:e];
            }
        }
        return 0;
    }
    // the quit event of a logout: it says why
    if (!strcmp(argv[1], "logout-quit")) {
        pid_t pid = atoi(argv[2]);
        NSAppleEventDescriptor *target = [NSAppleEventDescriptor descriptorWithProcessIdentifier:pid];
        NSAppleEventDescriptor *ev = [NSAppleEventDescriptor appleEventWithEventClass:kCoreEventClass eventID:kAEQuitApplication
                                      targetDescriptor:target returnID:kAutoGenerateReturnID transactionID:kAnyTransactionID];
        [ev setAttributeDescriptor:[NSAppleEventDescriptor descriptorWithEnumCode:kAELogOut] forKeyword:kAEQuitReason];
        NSError *err = nil;
        [ev sendEventWithOptions:NSAppleEventSendNoReply timeout:5 error:&err];
        if (err) { fprintf(stderr, "%s\n", err.description.UTF8String); return 1; }
        return 0;
    }
    return 2;
}
EOF
cc -fobjc-arc -o "$PROBE" "$SB/ownprobe.m" -framework AppKit || { echo "FAIL: could not build the probe"; exit 1; }
"$PROBE" coalition $$ >/dev/null || { echo "FAIL: this system does not report a pid's coalition"; exit 1; }

count() { local n; n=$(grep -c $'\t'"$1"$'\t' "$APPLOG" 2>/dev/null); echo "${n:-0}"; }
wait_count() { local i; for (( i = 0; i < $3 * 10; i++ )); do (( $(count "$1") >= $2 )) && return 0; sleep 0.1; done; return 1; }
wait_file() { local i; for (( i = 0; i < $2 * 10; i++ )); do [[ -s $1 ]] && return 0; sleep 0.1; done; return 1; }
alive() { kill -0 "$1" 2>/dev/null; }
wait_gone() { local i; for (( i = 0; i < $2 * 10; i++ )); do alive "$1" || return 0; sleep 0.1; done; return 1; }
coalition() { "$PROBE" coalition "$1"; }
session_pid() { sed -n 's/^pid = \([0-9][0-9]*\)$/\1/p' "$MARSPOT_STATE_DIR/sessions/$1/entry.toml"; }
shell_of() { sed -n 's/^shell_child_pid = \([0-9][0-9]*\)$/\1/p' "$MARSPOT_STATE_DIR/sessions/$1/entry.toml"; }
shells_running() { pgrep -x marspot-shell | while read -r p; do _dev_is_ours "$p" && echo "$p"; done; }
now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }

ls_record() {
  lsappinfo list | awk -v pid="$1" '
    /^ *[0-9]+\) / { if (hit) exit; block = "" }
    { block = block $0 "\n" }
    $0 ~ "pid = " pid " " { hit = 1 }
    END { if (hit) printf "%s", block }'
}
ls_coalition_pids() { ls_record "$1" | sed -n 's/.*coalition: [0-9]* *{\([^}]*\)}.*/\1/p' | tr ' ' '\n' | grep -E '^[0-9]+$'; }
ls_type() { ls_record "$1" | sed -n 's/.*type="\([^"]*\)".*/\1/p' | head -1; }

app_env=(--env "MARSPOT_STATE_DIR=$MARSPOT_STATE_DIR" --env "MARSPOT_SESSION_BIN=$SESSION_BIN"
  --env "MARSPOT_CORE_BIN=$CORE_BIN" --env MARSPOT_DEV_SANDBOX=1 --env MARSPOT_MANUAL_UPDATE_ONLY=1)

# Opens the app the way the Dock does; returns once a shell answers.
open_app() { # extra `open` flags...
  local i
  open -g "$@" "$APP" "${app_env[@]}" || die "LaunchServices did not open the sandbox bundle"
  for (( i = 0; i < 300; i++ )); do
    L1="$(dev_shell_pid)"
    [[ -n $L1 ]] && "$SHELL_BIN" --panes >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  die "the app never came up"
}

quit_app() {
  local cleanups answered
  cleanups=$(count SHELL_QUIT_CLEANUP)
  answered="$(osascript -l JavaScript -e "ObjC.import('AppKit');
    \$.NSRunningApplication.runningApplicationWithProcessIdentifier($L1).terminate" 2>&1)"
  [[ $answered == true ]] || die "the quit request was not delivered: $answered"
  wait_count SHELL_QUIT_CLEANUP $((cleanups + 1)) 10 || die "the app did not act on the quit"
}

# After a quit with sessions kept: the same process, with no window.
expect_resident() { # how many SHELL_RESIDENT lines there were before
  if wait_count SHELL_RESIDENT $(( $1 + 1 )) 10 && alive "$L1"; then
    sleep 0.5
    [[ -z "$(dev_windows_of "$L1")" ]] || bad "the resident shell has a window on screen"
    [[ -z "$(dev_core_pids)" ]] || bad "a core is still running under the resident shell"
    return 0
  fi
  bad "the shell did not stay after a quit with sessions kept (pid $L1 $(alive "$L1" && echo alive || echo gone))"
  return 1
}

two_panes() { # sets P1 P2
  local i sids=""
  for (( i = 0; i < 150; i++ )); do
    sids="$("$SHELL_BIN" --panes 2>/dev/null | awk 'NR>1 {print $1}' | sort -n | tr '\n' ' ')"
    [[ $(wc -w <<<"$sids") -ge 2 ]] && break
    sleep 0.4
  done
  read -r P1 P2 _ <<<"$sids"
  [[ -n "${P2:-}" ]] || die "two panes never came up (have: $sids)"
}

send_once() { # pane, text
  local i
  for (( i = 0; i < 40; i++ )); do
    "$SHELL_BIN" --send "$1" "$2" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  die "--send to pane $1 was refused 40 times"
}

# The pids that are the app's or a pane's right now.
ours() { echo "$(dev_shell_pid) $(dev_core_pids | tr '\n' ' ') $S1 $S2 $Z1 $Z2 $BY $LAUNCHER"; }

check_owner() {
  local sid line want; want="$(dev_shell_pid)"
  for sid in "$P1" "$P2"; do
    line="$(grep $'\tSESSION_OWNER\t' "$APPLOG" | grep -E $'\t'"sid=$sid("$'\t'"|\$)" | tail -1)"
    if [[ $line == *$'\t'"owner_pid=$want"* ]]; then ok "owner: session $sid says owner_pid=$want"
    else bad "owner: session $sid's last SESSION_OWNER line does not name the shell $want: $(sed -n 's/.*\(coalition=.*\)/\1/p' <<<"$line" | tr '\t' ' ')${line:+ }${line:-no such line}"; fi
  done
}

check_record() {
  local l1 listed p missing=""; l1="$(dev_shell_pid)"
  listed=" $(ls_coalition_pids "$l1" | tr '\n' ' ')"
  for p in "$S1" "$S2" "$Z1" "$Z2"; do [[ $listed == *" $p "* ]] || missing+=" $p"; done
  if [[ -z $missing ]]; then ok "record: the shell's record ($(ls_type "$l1")) lists both sessions and both panes' shells"
  else bad "record: the shell's LaunchServices record (pid $l1, $(ls_type "$l1")) does not list$missing; it lists:${listed:- nothing}"; fi
}

# Has the launcher in pane 1 start the program; says what its record lists.
check_stranger() {
  local i gui listed p hit=""
  rm -f "$SB/gui.pid"
  ( echo 600 > "$SB/go" ) & local w=$!
  wait_file "$SB/gui.pid" 10 || { kill "$w" 2>/dev/null; bad "stranger: the launcher in pane $P1 did not start the program"; return; }
  gui="$(cat "$SB/gui.pid")"; STRANGERS+=("$gui")
  for (( i = 0; i < 100; i++ )); do [[ -n "$(ls_record "$gui")" ]] && break; sleep 0.1; done
  sleep 0.7
  listed="$(ls_coalition_pids "$gui" | tr '\n' ' ')"
  for p in $(ours); do [[ " $listed" == *" $p "* ]] && hit+=" $p"; done
  if [[ -z $hit ]]; then ok "stranger: running, its record lists none of ours${listed:+ (lists: $listed)}"
  else bad "stranger: while it runs its record lists our pids:$hit (all listed: $listed)"; fi
  kill "$gui" 2>/dev/null; sleep 1.5
  hit=""; listed="$(ls_coalition_pids "$gui" | tr '\n' ' ')"
  for p in $(ours); do [[ " $listed" == *" $p "* ]] && hit+=" $p"; done
  if [[ -z $hit ]]; then ok "stranger: exited, $([[ -z "$(ls_record "$gui")" ]] && echo "its record is gone" || echo "its record lists none of ours")"
  else bad "stranger: after it exited its record stays ($(ls_record "$gui" | grep -o 'exited-with-subordinates')) and lists our pids:$hit"; fi
}

state() { # name
  STATE="$1"
  echo; echo "── $STATE ──"
  local p
  for p in "$S1" "$S2" "$Z1" "$Z2" "$BY" "$LAUNCHER"; do alive "$p" || bad "pid $p of a pane is gone"; done
  echo "    shell $(dev_shell_pid) coalition $(coalition "$(dev_shell_pid)"); sessions in $(coalition "$S1") $(coalition "$S2")"
  check_owner; check_stranger; check_record
}

# The shell again after it was opened: the same panes on the same sessions.
reattached() {
  two_panes
  [[ "$(session_pid "$P1")" == "$S1" && "$(shell_of "$P1")" == "$Z1" && "$(session_pid "$P2")" == "$S2" && "$(shell_of "$P2")" == "$Z2" ]] \
    || bad "the app did not attach to the sessions it had (now $(session_pid "$P1")/$(shell_of "$P1") $(session_pid "$P2")/$(shell_of "$P2"))"
}

# ───────────────────────────── the walk ─────────────────────────────
STATE=setup
dev_kill_shell_core
rm -rf "$MARSPOT_STATE_DIR"; mkdir -p "$MARSPOT_STATE_DIR/logs"
mkfifo "$SB/go"

# a second window, so a second pane, on this first launch only
open_app --env MARSPOT_DEV_EXTRA_WINDOWS=1
wait_count L3_SPAWNED 1 90 || die "no pane session was ever spawned"
two_panes
FIRST="$L1"; C0="$(coalition "$L1")"
S1="$(session_pid "$P1")"; S2="$(session_pid "$P2")"; Z1="$(shell_of "$P1")"; Z2="$(shell_of "$P2")"
# the launcher: started in pane 1, it starts the program when asked, also while the app has no window
send_once "$P1" "( echo \$\$ > $SB/launcher.pid; while :; do read -r n < $SB/go && { $PROBE gui \$n & echo \$! > $SB/gui.pid; }; done ) &"
send_once "$P2" "sleep 100000 & echo \$! > $SB/by.pid"
wait_file "$SB/launcher.pid" 15 && wait_file "$SB/by.pid" 15 || die "the panes' shells never ran what was typed"
LAUNCHER="$(cat "$SB/launcher.pid")"; BY="$(cat "$SB/by.pid")"
echo "==> app $L1 in coalition $C0; panes $P1 $P2: sessions $S1 $S2, shells $Z1 $Z2, launcher $LAUNCHER, bystander $BY"

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
stage_shell() { mkdir -p "$MARSPOT_STATE_DIR/binaries/pending"; cp "$SHELL_BIN" "$MARSPOT_STATE_DIR/binaries/pending/marspot-shell"; }
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
  updates=$(count SHELL_SELF_UPDATE); residents=$(count SHELL_RESIDENT)
  stage_shell
  kill -USR1 "$L1"
  wait_count SHELL_SELF_UPDATE $((updates + 1)) 60 && wait_count SHELL_RESIDENT $((residents + 1)) 10 \
    || bad "the resident shell did not move to the staged image"
  sleep 0.5
  [[ "$(dev_shell_pid)" == "$keep" ]] || bad "the resident's update changed its pid"
  state "shell updated in place while quit"
fi
open_app; reattached

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
kill_shell() { # sets KILLED_AT, and L1 to the shell a session started
  local old="$L1" i
  residents=$(count SHELL_RESIDENT)
  KILLED_AT="$(now_ms)"; kill -9 "$old"
  for (( i = 0; i < 100; i++ )); do
    L1="$(dev_shell_pid)"; [[ -n $L1 && $L1 != "$old" ]] && (( $(count SHELL_RESIDENT) > residents )) && return 0
    sleep 0.1
  done
  L1=""; return 1
}
if kill_shell; then
  started="$(grep $'\tOWNER_STARTED\t' "$APPLOG" | tail -1 | cut -f2)"
  up="$(grep $'\tSHELL_RESIDENT\t' "$APPLOG" | tail -1 | cut -f2)"
  echo "    the shell was killed at $KILLED_AT; a session started another $(( started - KILLED_AT )) ms later, which answered $(( up - KILLED_AT )) ms after the kill"
  grep $'\tSHELL_RESIDENT_RECORD\t' "$APPLOG" | tail -1 | cut -f8- | tr '\t' ' ' | sed 's/^/    /'
  [[ "$(coalition "$L1")" == "$(coalition "$S1")" ]] || bad "the shell a session started is in coalition $(coalition "$L1"), the sessions in $(coalition "$S1")"
  sleep 1.5
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

# twenty quits and openings: one process throughout
keep="$L1"; c_keep="$(coalition "$L1")"; n_ok=0; foots=""
for (( round = 1; round <= 20; round++ )); do
  residents=$(count SHELL_RESIDENT)
  quit_app
  wait_count SHELL_RESIDENT $((residents + 1)) 10 || break
  sleep 0.3
  foots+=" $("$PROBE" footprint "$L1")"
  open_app
  [[ $L1 == "$keep" && "$(shells_running | tr '\n' ' ')" == "$keep " ]] || break
  n_ok=$round
done
STATE="quit and opened twenty times"; echo; echo "── $STATE ──"
if (( n_ok == 20 )); then
  ok "one shell throughout: pid $keep, coalition $(coalition "$L1")"
  [[ "$(coalition "$L1")" == "$c_keep" && "$(coalition "$S1")" == "$c_keep" ]] || bad "a coalition changed over twenty quits"
  echo "    resident footprint after each quit, KiB:$foots; descriptors open: $(lsof -p "$L1" 2>/dev/null | grep -c .)"
  # it comes up at one of two sizes, 2 MiB apart, in any round: growth is later rounds above earlier ones
  early=$(tr ' ' '\n' <<<"$foots" | sed -n '2,11p' | sort -n | tail -1)
  late=$(tr ' ' '\n' <<<"$foots" | sed -n '12,21p' | sort -n | tail -1)
  (( late <= early + 1024 )) || bad "the resident shell grew: at most ${early} KiB in the first ten quits, ${late} in the last ten"
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

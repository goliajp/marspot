#!/usr/bin/env bash
#
# What a pane starts must not be able to take the other panes with it.
#
# 2026-11-11: a program started in a pane showed up in the Dock, the
# user quit that icon, and the Dock sent SIGTERM to every process in
# the program's coalition -- which was the app's: six sessions and
# every program in every pane.  macOS puts an app launch and all it
# starts into one coalition.  Sessions outlive the app that opened
# them, so after the app is quit and opened again they sit in a
# coalition no running app answers for, and the next program that
# registers with LaunchServices from there is listed as its owner.
# Only launchd starts a process in a coalition of its own: a pane's
# shell is a launchd job (unless MARSPOT_PANE_JOB=0), so what the pane
# starts shares a coalition with that pane and nothing else.
#
# The app is opened through LaunchServices here, as a bundle, because
# that is what gives it a coalition of its own; quit; and opened again.
# Then, with two panes:
#   0. The app names the pane a program that registers as an
#      application was started in (PANE_APP), switch on or off.
#   1. Each pane's shell is in a coalition of its own: not the other
#      pane's, not the one the sessions are in.
#   2. A program started in pane A that registers as a foreground app
#      is listed by LaunchServices without any process of pane B or of
#      the app in its coalition.
#   3. Force-quitting it the way the Dock did leaves pane B's shell and
#      what it runs, both sessions, the shell and the core alive, and
#      no session logs a signal.
#   4. When such a program exits by itself, whatever record of it
#      LaunchServices keeps lists nothing outside pane A.  A record can
#      stay (exited-with-subordinates): the icon is then pane A's own.
#   5. The app quits when asked and keeps its sessions; the next launch
#      attaches to them with the same shells.
#   6. No job stays on launchd's list once a pane's shell is up, no
#      host process stays, and a pane whose shell exits still closes
#      its session.
# And with the switch off, as the control, the same steps show the
# defect: the program is listed with the coalition both sessions and
# the other pane are in, and the force quit signals them.
#
# Nothing here asks for accessibility access and no Dock icon is
# clicked.  The Dock's own call (`_LSForceQuitApplication`) answers a
# caller without the Dock's entitlement with an error, so the force
# quit is what the Dock logged doing on the day: SIGTERM to each pid of
# the coalition LaunchServices lists for the program, then the program.
# It is sent only to pids that are both in that list and this run's.
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app
# is never touched.  Exits 0 on pass, non-zero on the first failure.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"
SB="$(mktemp -d /tmp/msp-coal.XXXXXX)"
APP="$SB/Marspot.app"
PROBE="$SB/coalprobe"

fail() {
  echo "FAIL: $*"
  echo "  --- last 40 lines of $APPLOG:"
  tail -40 "$APPLOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
cleanup() {
  dev_kill_shell_core
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
plutil -replace CFBundleIdentifier -string com.goliajp.marspot.e2e-coalition "$APP/Contents/Info.plist"
plutil -replace CFBundleName -string MarspotCoalitionTest "$APP/Contents/Info.plist"

# A program that registers as a foreground app, reads a pid's
# coalition, and force-terminates an app by pid.
cat > "$SB/coalprobe.m" <<'EOF'
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
    if (!strcmp(argv[1], "forcequit")) {
        NSRunningApplication *ra = [NSRunningApplication runningApplicationWithProcessIdentifier:atoi(argv[2])];
        if (!ra) { fprintf(stderr, "pid %s is not a registered app\n", argv[2]); return 1; }
        return [ra forceTerminate] ? 0 : 1;
    }
    return 2;
}
EOF
cc -fobjc-arc -o "$PROBE" "$SB/coalprobe.m" -framework AppKit || { echo "FAIL: could not build the probe"; exit 1; }
"$PROBE" coalition $$ >/dev/null || { echo "FAIL: this system does not report a pid's coalition"; exit 1; }

count() { # tag — how many times the log has it
  local n
  n=$(grep -c $'\t'"$1"$'\t' "$APPLOG" 2>/dev/null)
  echo "${n:-0}"
}

wait_count() { # tag, at least, seconds
  local i
  for (( i = 0; i < $3 * 10; i++ )); do
    (( $(count "$1") >= $2 )) && return 0
    sleep 0.1
  done
  return 1
}

wait_file() { # path, seconds
  local i
  for (( i = 0; i < $2 * 10; i++ )); do
    [[ -s $1 ]] && return 0
    sleep 0.1
  done
  return 1
}

alive() { kill -0 "$1" 2>/dev/null; }
word() { alive "$1" && echo alive || echo gone; }
coalition() { "$PROBE" coalition "$1"; }

# Through LaunchServices: that is what puts the app in a coalition of
# its own, as the installed one is.  Returns once a new shell answers.
open_app() { # switch (0|1)
  local before i
  before="$(dev_shell_pid)"
  open -n -g "$APP" \
    --env "MARSPOT_STATE_DIR=$MARSPOT_STATE_DIR" --env "MARSPOT_SESSION_BIN=$SESSION_BIN" \
    --env "MARSPOT_CORE_BIN=$CORE_BIN" --env MARSPOT_DEV_EXTRA_WINDOWS=1 --env "MARSPOT_PANE_JOB=$1" \
    || fail "LaunchServices did not open the sandbox bundle"
  for (( i = 0; i < 300; i++ )); do
    L1="$(dev_shell_pid)"
    [[ -n $L1 && $L1 != "$before" ]] && "$SHELL_BIN" --panes >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  fail "the app never came up"
}

quit_app() {
  local cleanups answered
  cleanups=$(count SHELL_QUIT_CLEANUP)
  answered="$(osascript -l JavaScript -e "ObjC.import('AppKit');
    \$.NSRunningApplication.runningApplicationWithProcessIdentifier($L1).terminate" 2>&1)"
  [[ $answered == true ]] || fail "the quit request was not delivered: $answered"
  wait_count SHELL_QUIT_CLEANUP $((cleanups + 1)) 5 || fail "the app did not quit when asked"
  local i
  for (( i = 0; i < 50; i++ )); do
    alive "$L1" || return 0
    sleep 0.1
  done
  fail "the shell is still running after the quit"
}

session_pid() { sed -n 's/^pid = \([0-9][0-9]*\)$/\1/p' "$MARSPOT_STATE_DIR/sessions/$1/entry.toml"; }
shell_of() { sed -n 's/^shell_child_pid = \([0-9][0-9]*\)$/\1/p' "$MARSPOT_STATE_DIR/sessions/$1/entry.toml"; }

two_panes() { # sets P1 P2
  local i sids=""
  for (( i = 0; i < 150; i++ )); do
    sids="$("$SHELL_BIN" --panes 2>/dev/null | awk 'NR>1 {print $1}' | sort -n | tr '\n' ' ')"
    [[ $(wc -w <<<"$sids") -ge 2 ]] && break
    sleep 0.4
  done
  read -r P1 P2 _ <<<"$sids"
  [[ -n "${P2:-}" ]] || fail "two panes never came up (have: $sids)"
}

shell_ready() { # pane
  local sid=$1 mark="$SB/ready-$1" attempt i
  rm -f "$mark"
  for (( attempt = 1; attempt <= 10; attempt++ )); do
    "$SHELL_BIN" --send "$sid" "echo reading > $mark" >/dev/null 2>&1 || continue
    for (( i = 0; i < 15; i++ )); do
      [[ -f "$mark" ]] && return 0
      sleep 0.2
    done
  done
  fail "pane $sid's shell never ran anything typed into it"
}

send_once() { # pane, text
  local i
  for (( i = 0; i < 40; i++ )); do
    "$SHELL_BIN" --send "$1" "$2" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  fail "--send to pane $1 was refused 40 times"
}

# The LaunchServices record of the app whose pid is $1: the lines from
# its header to the next one.
ls_record() {
  lsappinfo list | awk -v pid="$1" '
    /^ *[0-9]+\) / { if (hit) exit; block = "" }
    { block = block $0 "\n" }
    $0 ~ "pid = " pid " " { hit = 1 }
    END { if (hit) printf "%s", block }'
}
# The pids that record lists as its coalition's, one per line.
ls_coalition_pids() { ls_record "$1" | sed -n 's/.*coalition: [0-9]* *{\([^}]*\)}.*/\1/p' | tr ' ' '\n' | grep -E '^[0-9]+$'; }
ls_coalition_id() { ls_record "$1" | sed -n 's/.*coalition: \([0-9][0-9]*\).*/\1/p' | head -1; }

start_gui_in() { # pane, seconds → GUI
  rm -f "$SB/gui.pid"
  send_once "$1" "$PROBE gui $2 & echo \$! > $SB/gui.pid"
  wait_file "$SB/gui.pid" 10 || fail "pane $1 never started the program"
  GUI="$(cat "$SB/gui.pid")"
  local i
  for (( i = 0; i < 100; i++ )); do
    [[ -n "$(ls_record "$GUI")" ]] && { sleep 0.5; return 0; }
    sleep 0.1
  done
  fail "the program (pid $GUI) never registered with LaunchServices"
}

# What the Dock did to the program $GUI on 2026-11-11.  $1: the pids of
# this run that may be signalled.
dock_force_quit() {
  local p sent=""
  for p in $(ls_coalition_pids "$GUI"); do
    case " $1 " in *" $p "*) kill -TERM "$p" 2>/dev/null && sent+=" $p" ;; esac
  done
  echo "    SIGTERM sent to the pids LaunchServices lists with the program:${sent:- none}"
  "$PROBE" forcequit "$GUI"
}

jobs_of_this_sandbox() { dev_pane_job_labels | sort | tr '\n' ' '; }

# One pass: the app opened, quit, and opened again; then a program of
# pane 1 registers, is force-quit, and another exits by itself.
run_case() { # switch (0|1)
  local on=$1 p listed lsid signals_before
  dev_kill_shell_core
  rm -rf "$MARSPOT_STATE_DIR"; mkdir -p "$MARSPOT_STATE_DIR/logs"

  open_app "$on"
  wait_count L3_SPAWNED 1 90 || fail "no pane session was ever spawned"
  two_panes
  shell_ready "$P1"; shell_ready "$P2"
  local first_app=$L1 c_first
  c_first="$(coalition "$L1")"
  S1="$(session_pid "$P1")"; S2="$(session_pid "$P2")"
  Z1="$(shell_of "$P1")"; Z2="$(shell_of "$P2")"
  for p in "$S1" "$S2" "$Z1" "$Z2"; do
    [[ $p =~ ^[0-9]+$ ]] && alive "$p" || fail "a process of a pane is missing (L3=$S1,$S2 shells=$Z1,$Z2)"
  done
  [[ "$(coalition "$S1")" == "$c_first" && "$(coalition "$S2")" == "$c_first" ]] \
    || fail "the sessions are expected in the coalition of the app that opened them ($c_first)"
  if (( on )); then
    (( $(count PANE_JOB_FALLBACK) == 0 )) || fail "launchd did not take a pane's shell: $(grep PANE_JOB_FALLBACK "$APPLOG" | tail -1)"
    (( $(count PANE_JOB_STARTED) >= 2 )) || fail "expected a PANE_JOB_STARTED per pane, have $(count PANE_JOB_STARTED)"
    grep $'\tPANE_JOB_STARTED\t' "$APPLOG" | sed 's/^/    /'
    # what the pane runs is charged to the job's program: the app's own
    (( $(grep $'\tPANE_JOB_STARTED\t' "$APPLOG" | grep -c 'host=[^	]*Marspot\.app/Contents/MacOS/marspot-shell') >= 2 )) \
      || fail "a pane's job was not hosted by the app's main executable"
    [[ "$(ps -o tty= -p "$Z1" | tr -d ' ')" == ttys* ]] || fail "pane $P1's shell has no controlling terminal"
  else
    (( $(count PANE_JOB_STARTED) == 0 )) || fail "a pane's shell became a job with the switch off"
  fi

  quit_app
  for p in "$S1" "$S2" "$Z1" "$Z2"; do
    alive "$p" || fail "pid $p did not survive the app's quit"
  done
  open_app "$on"
  two_panes
  [[ "$(session_pid "$P1")" == "$S1" && "$(shell_of "$P1")" == "$Z1" && "$(session_pid "$P2")" == "$S2" && "$(shell_of "$P2")" == "$Z2" ]] \
    || fail "the second launch did not attach to the sessions of the first"
  shell_ready "$P1"; shell_ready "$P2"
  L2="$(dev_core_pids | head -1)"
  local c_app c_z1 c_z2
  c_app="$(coalition "$L1")"; c_z1="$(coalition "$Z1")"; c_z2="$(coalition "$Z2")"
  [[ "$c_app" != "$c_first" ]] || fail "the second launch is in the first one's coalition; the app was not opened through LaunchServices"
  echo "==> quit and reopened: SHELL_QUIT_CLEANUP, sessions $S1 $S2 kept, attached again with shells $Z1 $Z2"
  echo "==> coalitions: first launch $c_first (sessions $(coalition "$S1") $(coalition "$S2")), running app $c_app, pane shells $c_z1 $c_z2"
  if (( on )); then
    [[ "$c_z1" != "$c_first" && "$c_z2" != "$c_first" && "$c_z1" != "$c_app" && "$c_z2" != "$c_app" && "$c_z1" != "$c_z2" ]] \
      || fail "pane shells share a coalition (first launch $c_first, app $c_app, pane shells $c_z1 $c_z2)"
    echo "ASSERT[on] coalitions: sessions=$c_first app=$c_app pane$P1=$c_z1 pane$P2=$c_z2, pane shells in neither and not together"
  else
    [[ "$c_z1" == "$c_first" && "$c_z2" == "$c_first" ]] || fail "with the switch off the pane shells are expected in the first launch's coalition"
  fi

  send_once "$P1" "sleep 600 & echo \$! > $SB/by1.pid"
  send_once "$P2" "sleep 601 & echo \$! > $SB/by2.pid"
  wait_file "$SB/by1.pid" 10 && wait_file "$SB/by2.pid" 10 || fail "the bystanders never started"
  B1="$(cat "$SB/by1.pid")"; B2="$(cat "$SB/by2.pid")"; rm -f "$SB/by1.pid" "$SB/by2.pid"

  start_gui_in "$P1" 600
  echo "==> LaunchServices record of the program pane $P1 started (pid $GUI):"
  ls_record "$GUI" | sed 's/^/    /'
  listed="$(ls_coalition_pids "$GUI" | tr '\n' ' ')"
  lsid="$(ls_coalition_id "$GUI")"
  # the app knows which pane the program is of, either way
  local named=""
  for (( p = 0; p < 50; p++ )); do
    named="$(grep $'\tPANE_APP\t' "$APPLOG" | grep -E "(^|[^0-9a-z_])pid=$GUI([^0-9]|\$)" | tail -1)"
    [[ -n $named ]] && break
    sleep 0.1
  done
  grep -qE "(^|[^0-9a-z_])sid=$P1([^0-9]|\$)" <<<"$named" \
    || fail "the app did not log the program (pid $GUI) as pane $P1's: ${named:-no PANE_APP line}"
  echo "ASSERT[$on] owner: PANE_APP names pane $P1 for pid $GUI"
  if (( on )); then
    [[ "$(coalition "$GUI")" == "$c_z1" ]] || fail "the program is in coalition $(coalition "$GUI"), not pane $P1's $c_z1"
    for p in "$L1" "$L2" "$S1" "$S2" "$Z2" "$B2"; do
      grep -qw "$p" <<<"$listed" && fail "the program's record lists pid $p, which is not pane $P1's (listed: $listed)"
    done
    echo "ASSERT[on] listing: coalition ${lsid:-none} [$listed] holds none of L1=$L1 L2=$L2 L3=$S1,$S2 other pane=$Z2,$B2"
  else
    for p in "$S1" "$S2" "$Z2" "$B2"; do
      grep -qw "$p" <<<"$listed" || fail "the control did not reproduce: the program's record does not list pid $p (coalition ${lsid:-none}: $listed)"
    done
    echo "CONTROL[off] listing: coalition $lsid [$listed] holds both sessions $S1 $S2 and the other pane's $Z2 $B2"
  fi

  signals_before=$(count L3_SIGNAL)
  dock_force_quit "$S1 $S2 $Z1 $Z2 $B1 $B2" || fail "the force quit of pid $GUI could not be made"
  sleep 3
  alive "$GUI" && fail "the program survived its force quit"
  echo "    after the force quit: L1 $(word "$L1") L2 $(word "$L2") L3 $(word "$S1") $(word "$S2"); pane $P1 shell $(word "$Z1") bystander $(word "$B1"); pane $P2 shell $(word "$Z2") bystander $(word "$B2")"
  grep $'\tL3_SIGNAL\t' "$APPLOG" | tail -n +$((signals_before + 1)) | sed 's/^/    /'
  for p in "$L1" "$L2" "$S1" "$S2"; do
    alive "$p" || fail "pid $p of the app died in the force quit (L1=$L1 L2=$L2 L3=$S1,$S2)"
  done
  if (( on )); then
    for p in "$Z2" "$B2"; do
      alive "$p" || fail "pid $p of pane $P2 died when a program of pane $P1 was force-quit"
    done
    (( $(count L3_SIGNAL) == signals_before )) || fail "a session was signalled"
    echo "ASSERT[on] force quit: L1 L2 both L3 and pane $P2 (shell $Z2, bystander $B2) alive; L3_SIGNAL lines added: 0"
  else
    (( $(count L3_SIGNAL) >= signals_before + 2 )) || fail "the control did not reproduce: the force quit signalled $(( $(count L3_SIGNAL) - signals_before )) sessions, expected both"
    alive "$B2" && fail "the control did not reproduce: the other pane's bystander $B2 survived"
    echo "CONTROL[off] force quit: both sessions signalled, the other pane's bystander $B2 killed"
  fi

  # a program that exits by itself while its pane lives on
  if (( on )) || alive "$Z1"; then
    start_gui_in "$P1" 4
    sleep 6
    alive "$GUI" && fail "the short program did not exit"
    if (( on )); then
      listed="$(ls_coalition_pids "$GUI" | tr '\n' ' ')"
      for p in "$L1" "$L2" "$S1" "$S2" "$Z2" "$B2"; do
        grep -qw "$p" <<<"$listed" && fail "the exited program's record lists pid $p, which is not pane $P1's (listed: $listed)"
      done
      echo "ASSERT[on] exit: the record of pid $GUI ($(ls_record "$GUI" | grep -o 'exited-with-subordinates' || echo gone)) lists [$listed], nothing outside pane $P1"
    else
      ls_record "$GUI" | grep -q 'exited-with-subordinates' \
        || fail "the control did not reproduce: no exited-with-subordinates record of pid $GUI"
      echo "CONTROL[off] exit: the record of pid $GUI stays, exited-with-subordinates"
    fi
  fi
}

echo "════ the switch on ════"
jobs_before="$(launchctl list | grep -c 'com\.marspot\.pane\.')"
run_case 1

echo
echo "════ nothing of the jobs is left ════"
hosts() { ps -axww -o pid=,command= | grep -F -- "--pane-host $MARSPOT_STATE_DIR/" | grep -v grep | awk '{print $1}' | tr '\n' ' '; }
[[ -z "$(jobs_of_this_sandbox)" ]] || fail "jobs on launchd's list while the panes run: $(jobs_of_this_sandbox)"
[[ -z "$(hosts)" ]] || fail "pane hosts still running: $(hosts)"
[[ -z "$(ls "$MARSPOT_STATE_DIR/pane-jobs" 2>/dev/null)" ]] || fail "files left in pane-jobs: $(ls "$MARSPOT_STATE_DIR/pane-jobs")"
send_once "$P2" "kill $B2; exec /usr/bin/true"
for (( i = 0; i < 100; i++ )); do
  alive "$Z2" || alive "$S2" || break
  sleep 0.1
done
alive "$Z2" && fail "pane $P2's shell did not exit"
alive "$S2" && fail "pane $P2's session did not end when its shell exited"
quit_app
dev_kill_shell_core
sleep 1
for p in "$Z1" "$B1"; do
  alive "$p" && fail "pid $p of pane $P1 is left after the sandbox was stopped"
done
jobs_after="$(launchctl list | grep -c 'com\.marspot\.pane\.')"
(( jobs_after <= jobs_before )) || fail "the run left launchd jobs behind ($jobs_before before, $jobs_after after)"
echo "ASSERT[on] jobs: none listed while the panes ran, no host process; pane $P2's session ended with its shell; com.marspot.pane.* before $jobs_before, after $jobs_after"

echo
echo "════ the switch off (control) ════"
run_case 0
dev_kill_shell_core
sleep 2
left="$(lsappinfo list | grep -c 'coalprobe')"
(( left == 0 )) || fail "$left LaunchServices records of the test program are left after the sandbox was stopped"
echo "==> nothing of the run is left in LaunchServices"

echo
echo "PASS: what a pane starts shares a coalition with that pane only"

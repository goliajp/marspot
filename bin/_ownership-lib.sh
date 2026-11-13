# shellcheck shell=bash
#
# What the suites that walk the app through its states share: a bundle
# of the suite's own name opened through LaunchServices with two panes,
# a program a pane can start that registers as a foreground
# application, and the three checks made in every state (see
# test-session-ownership.sh).
#
# Sourced after _dev-sandbox.sh, with OWN_SCRATCH (the scratch
# directory's name), OWN_BUNDLE_ID and OWN_BUNDLE_NAME set.

APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"
SB="$(mktemp -d "/tmp/$OWN_SCRATCH.XXXXXX")"
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
  for p in ${STRANGERS[@]+"${STRANGERS[@]}"} $(cat "$SB/launcher.pid" "$SB/by.pid" 2>/dev/null); do
    [[ $p =~ ^[0-9]+$ ]] && (( p > 1 )) && kill -9 "$p" 2>/dev/null
  done
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
plutil -replace CFBundleIdentifier -string "$OWN_BUNDLE_ID" "$APP/Contents/Info.plist"
plutil -replace CFBundleName -string "$OWN_BUNDLE_NAME" "$APP/Contents/Info.plist"
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
  open -g "$@" "$APP" "${app_env[@]}" 2>"$SB/open.err" || die "LaunchServices did not open the sandbox bundle: $(cat "$SB/open.err")"
  for (( i = 0; i < 300; i++ )); do
    L1="$(dev_shell_pid)"
    [[ -n $L1 ]] && "$SHELL_BIN" --panes >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  die "the app never came up"
}
quit_app() {
  local cleanups answered i
  cleanups=$(count SHELL_QUIT_CLEANUP)
  # the app answers on its command socket before it takes events:
  # a quit sent in that moment is not taken
  for (( i = 0; i < 10; i++ )); do
    answered="$(osascript -l JavaScript -e "ObjC.import('AppKit');
      \$.NSRunningApplication.runningApplicationWithProcessIdentifier($L1).terminate" 2>&1)"
    [[ $answered == true ]] && break
    sleep 0.5
  done
  [[ $answered == true ]] || die "the quit request was not delivered: $answered"
  (( i == 0 )) || echo "    (the quit request was refused $i time(s) first)"
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
  [[ -p $SB/go ]] || { bad "stranger: the launcher's fifo is gone"; return; }
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

# Opens the app with two panes; leaves a launcher in pane 1 that starts
# the program when asked, also while the app has no window, and a
# background process in pane 2.
start_walk() {
  STATE=setup
  dev_kill_shell_core
  rm -rf "$MARSPOT_STATE_DIR"; mkdir -p "$MARSPOT_STATE_DIR/logs"
  mkfifo "$SB/go" && [[ -p $SB/go ]] || die "could not make the launcher's fifo"
  # a second window, so a second pane, on this first launch only
  open_app --env MARSPOT_DEV_EXTRA_WINDOWS=1
  wait_count L3_SPAWNED 1 90 || die "no pane session was ever spawned"
  two_panes
  FIRST="$L1"; C0="$(coalition "$L1")"
  S1="$(session_pid "$P1")"; S2="$(session_pid "$P2")"; Z1="$(shell_of "$P1")"; Z2="$(shell_of "$P2")"
  # it reads a fifo and nothing else, and starts the program a bounded
  # number of times: a loop that read a plain file would start it for ever
  cat > "$SB/launcher.sh" <<EOF
echo \$\$ > $SB/launcher.pid
n=0
while [ -p $SB/go ] && [ \$n -lt 80 ]; do
  read -r secs < $SB/go || { sleep 1; continue; }
  n=\$((n + 1))
  $PROBE gui "\$secs" & echo \$! > $SB/gui.pid
done
EOF
  send_once "$P1" "/bin/sh $SB/launcher.sh &"
  send_once "$P2" "sleep 100000 & echo \$! > $SB/by.pid"
  wait_file "$SB/launcher.pid" 15 && wait_file "$SB/by.pid" 15 || die "the panes' shells never ran what was typed"
  LAUNCHER="$(cat "$SB/launcher.pid")"; BY="$(cat "$SB/by.pid")"
  echo "==> app $L1 in coalition $C0; panes $P1 $P2: sessions $S1 $S2, shells $Z1 $Z2, launcher $LAUNCHER, bystander $BY"
}

stage_shell() { mkdir -p "$MARSPOT_STATE_DIR/binaries/pending"; cp "$SHELL_BIN" "$MARSPOT_STATE_DIR/binaries/pending/marspot-shell"; }

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

# One line for each LaunchServices record that is this sandbox's app or
# a shell of it: pid, alive or dead, name, bundle id or -, bundle path.
app_records() {
  local pid state name id path
  lsappinfo list | awk -v bid="$OWN_BUNDLE_ID" -v app="${APP#/private}" -v sd="${MARSPOT_STATE_DIR#/private}" -v tgt="$ROOT/target/" '
    function val(key,   i, rest) {
      i = index(block, key "=\""); if (!i) return "-"
      rest = substr(block, i + length(key) + 2); return substr(rest, 1, index(rest, "\"") - 1)
    }
    function flush(   exe, path, name) {
      if (block == "") return
      exe = val("executable path"); path = val("bundle path"); sub(/^\/private/, "", exe); sub(/^\/private/, "", path)
      if (val("bundleID") == bid || path == app || (exe ~ /\/marspot-shell$/ && (index(exe, app "/") == 1 || index(exe, sd "/") == 1 || index(exe, tgt) == 1))) {
        name = block; sub(/^[^"]*"/, "", name); sub(/".*/, "", name)
        match(block, /pid = [0-9]+/)
        printf "%s\t%s\t%s\t%s\t%s\n", substr(block, RSTART + 6, RLENGTH - 6), (block ~ /\(exited/ ? "dead" : "alive"), name, val("bundleID"), path
      }
      block = ""
    }
    /^ *[0-9]+\) / { flush() }
    { block = block $0 " " }
    END { flush() }'
}

# The app is one record: the running shell, alive, under the bundle.
check_one_record() {
  local recs n pid state name id path want; want="$(dev_shell_pid)"
  recs="$(app_records)"; n="$(grep -c . <<<"$recs")"
  IFS=$'\t' read -r pid state name id path <<<"$recs"
  if (( n == 1 )) && [[ $pid == "$want" && $state == alive && $id == "$OWN_BUNDLE_ID" && $path == "${APP#/private}" && $name != marspot-shell ]]; then
    ok "one record: \"$name\" ($id), the shell $pid, alive"
  else
    bad "one record: LaunchServices lists $n record(s) for the app where the one of the shell $want under the bundle is expected:"
    sed 's/^/          /' <<<"$recs" | tr '\t' ' '
  fi
}

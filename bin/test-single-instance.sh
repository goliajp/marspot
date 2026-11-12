#!/usr/bin/env bash
#
# One app per state directory, and a quit asked for from outside is a
# quit.
#
# 2026-11-11: the user quit the app, an armed bundle lander reopened it
# and in the same second it was opened a second time.  Two shells ran
# for seven minutes, each with its own core, both restoring the same
# three windows and attached to the same fifteen sessions.  The same
# day a quit sent by the system (what the Dock's Quit, logout and
# `osascript -e 'quit app'` send) was answered `true` and ignored.
#
# What this asserts:
#   1. A second launch against a running app's state dir exits 0 within
#      two seconds and changes nothing: still one shell and one core,
#      the same session processes, no window opened, the command socket
#      still the first shell's.  It logs SHELL_ALREADY_RUNNING with the
#      holder's pid, and the holder logs SHELL_ACTIVATED_BY with its.
#   2. `NSRunningApplication.terminate` by pid quits the app the way
#      Cmd-Q does: SHELL_QUIT_CLEANUP within three seconds, every
#      session still running, and the shell still there for them with
#      no window and no core.
#   3. The same with the window the app booted with already closed and
#      another still open.
#   4. The lander's reopen and a reopen by hand at the same moment leave
#      one app, the shell that was there; the lander lands once, disarms
#      and does not try again.
#
# Windows are counted from the log: nothing here may ask for
# accessibility access on the runner.  That the first app's window
# actually comes to the front is not asserted: the runner sits at its
# login window, and nothing can be in front of that.
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app
# is never touched.  Exits 0 on pass, non-zero on the first failure.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

RUNLOG=/tmp/marspot-test-single-instance.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"
export MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN"

fail() {
  echo "FAIL: $*"
  echo "  --- last 40 lines of $APPLOG:"
  tail -40 "$APPLOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() { dev_kill_shell_core; }
trap cleanup EXIT

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" || ! -x "$SESSION_BIN" ]]; then
  echo "==> building release binaries"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 )
fi

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

# This sandbox's processes of one name, by the state dir in their
# environment: the runner has other sandboxes on it.
ours() { # name
  local p out=""
  for p in $(pgrep -x "$1"); do
    if _dev_is_ours "$p"; then out+="$p "; fi
  done
  echo "${out% }"
}

session_pids() {
  dev_session_pids | sort -n | tr '\n' ' '
}

launch() { # [VAR=value ...]
  env "$@" nohup "$SHELL_BIN" >>"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
  disown
}

# What the Dock's Quit sends, addressed by pid: the sandbox app has no
# bundle id to name it by.
ask_to_quit() { # pid
  osascript -l JavaScript -e "ObjC.import('AppKit');
    \$.NSRunningApplication.runningApplicationWithProcessIdentifier($1).terminate" 2>&1
}

seed_two_windows() {
  python3 - "$MARSPOT_STATE_DIR" <<'PY' || exit 1
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1]); d.mkdir(parents=True, exist_ok=True)
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
def pane(sid=0): return struct.pack('<Q', sid) + s('') + s('')
body  = struct.pack('<II', 0xA5505010, 2)
body += struct.pack('<HH', 0, 2)
body += struct.pack('<HHHH', 2, 1, 0, 2) + pane() + pane()
body += struct.pack('<HHHH', 1, 1, 0, 1) + pane()
(d / 'shell-state.bin').write_bytes(body)
frames  = struct.pack('<IIH', 0xA5505011, 2, 2)
frames += struct.pack('<Idddd', 0, 100.0, 400.0, 900.0, 600.0)
frames += struct.pack('<Idddd', 0, 1050.0, 400.0, 700.0, 500.0)
(d / 'window-state.bin').write_bytes(frames)
PY
}

quit_and_check() { # label, sessions that must survive
  local label="$1" before="$2" shell answered cleanups
  shell="$(dev_shell_pid)"
  [[ -n $shell ]] || fail "[$label] no shell to quit"
  cleanups=$(count SHELL_QUIT_CLEANUP)
  answered="$(ask_to_quit "$shell")"
  echo "==> [$label] terminate() on pid $shell answered: $answered"
  [[ $answered == true ]] || fail "[$label] the quit request was not delivered: $answered"
  wait_count SHELL_QUIT_CLEANUP $((cleanups + 1)) 3 \
    || fail "[$label] no SHELL_QUIT_CLEANUP within 3 s of the quit request"
  # sessions are kept, so the same process stays to answer for them
  local residents
  residents=$(count SHELL_RESIDENT)
  wait_count SHELL_RESIDENT $((residents + 1)) 10 || wait_count SHELL_RESIDENT 1 1 \
    || fail "[$label] the shell did not become a resident"
  sleep 1
  [[ "$(dev_shell_pid)" == "$shell" ]] || fail "[$label] the shell did not stay after its quit cleanup"
  [[ -z "$(ours marspot-core)" ]] || fail "[$label] a core is still running after the quit"
  [[ -z "$(dev_windows_of "$shell")" ]] || fail "[$label] the resident shell has a window"
  [[ "$(session_pids)" == "$before" ]] \
    || fail "[$label] sessions changed over the quit: before [$before] after [$(session_pids)]"
  echo "==> [$label] quit within 3 s, all sessions still running, the shell $shell resident"
}

dev_kill_shell_core
dev_wipe_state
rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired" "$MARSPOT_STATE_DIR/pending-bundle"
mkdir -p "$MARSPOT_STATE_DIR/logs"
rm -f "$APPLOG"
> "$RUNLOG"
seed_two_windows

echo "════ 1. a second launch stands aside ════"
launch
wait_count WINDOW_RESTORED 1 40 || fail "the second window never assembled"
sleep 3
FIRST="$(dev_shell_pid)"
[[ -n $FIRST ]] || fail "the first shell is not running"
SESSIONS="$(session_pids)"
(( $(wc -w <<<"$SESSIONS") == 3 )) || fail "expected 3 sessions, got [$SESSIONS]"
CORES="$(ours marspot-core)"
(( $(wc -w <<<"$CORES") == 1 )) || fail "expected one core, got [$CORES]"
BOOTS=$(count CORE_BOOT)
FRAMES=$(count WINDOW_FIRST_FRAME)
OPENED=$(count WINDOW_OPENED)
PANES_BEFORE="$("$SHELL_BIN" --panes)" || fail "the first shell's command socket does not answer"

"$SHELL_BIN" >>"$RUNLOG" 2>&1 < /dev/null &
SECOND=$!
for (( i = 0; i < 20; i++ )); do
  kill -0 "$SECOND" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$SECOND" 2>/dev/null; then
  dev_record_pid "$SECOND"
  fail "the second launch (pid $SECOND) was still running after 2 s"
fi
wait "$SECOND"; rc=$?
(( rc == 0 )) || fail "the second launch exited $rc, not 0"
grep $'\tSHELL_ALREADY_RUNNING\t' "$APPLOG" | grep -q "holder_pid=$FIRST" \
  || fail "the second launch did not log SHELL_ALREADY_RUNNING holder_pid=$FIRST"
wait_count SHELL_ACTIVATED_BY 1 3 || fail "the first shell never logged SHELL_ACTIVATED_BY"
grep $'\tSHELL_ACTIVATED_BY\t' "$APPLOG" | grep -q "pid=$SECOND" \
  || fail "SHELL_ACTIVATED_BY does not name the second launch (pid $SECOND)"
sleep 2
[[ "$(ours marspot-shell)" == "$FIRST" ]] \
  || fail "expected exactly the first shell ($FIRST), found [$(ours marspot-shell)]"
[[ "$(ours marspot-core)" == "$CORES" ]] \
  || fail "the core changed: before [$CORES] after [$(ours marspot-core)]"
[[ "$(session_pids)" == "$SESSIONS" ]] \
  || fail "sessions changed: before [$SESSIONS] after [$(session_pids)]"
[[ "$(cat "$MARSPOT_STATE_DIR/shell.pid")" == "$FIRST" ]] || fail "shell.pid was rewritten"
(( $(count CORE_BOOT) == BOOTS )) || fail "a core booted for the second launch"
(( $(count WINDOW_FIRST_FRAME) == FRAMES && $(count WINDOW_OPENED) == OPENED )) \
  || fail "a window was opened for the second launch"
[[ "$("$SHELL_BIN" --panes)" == "$PANES_BEFORE" ]] \
  || fail "the command socket no longer answers for the first shell"
echo "==> second launch $SECOND exited 0; shell $FIRST, core $CORES and 3 sessions unchanged"

echo "════ 2. a quit asked for from outside ════"
quit_and_check "both windows open" "$SESSIONS"

echo "════ 3. the same with the boot window closed ════"
launch MARSPOT_DEV_CLOSE_SEQUENCE=1
wait_count DEV_CLOSE_SEQUENCE 1 60 || fail "the boot window was never closed"
sleep 3
[[ -n "$(dev_shell_pid)" ]] || fail "closing one of two windows ended the app"
[[ "$(session_pids)" == "$SESSIONS" ]] || fail "the relaunch did not reattach the same sessions"
quit_and_check "boot window closed" "$SESSIONS"

echo "════ 4. the lander's reopen and a reopen by hand, together ════"
# The lander as install-local.sh writes it, armed with a stage that
# matches binaries/current and pointed at a bundle of its own.
STAGE="$MARSPOT_STATE_DIR/pending-bundle"
FAKE_APP="$MARSPOT_STATE_DIR/Fake.app"
mkdir -p "$STAGE" "$MARSPOT_STATE_DIR/binaries/current" "$FAKE_APP/Contents/MacOS"
for b in marspot-shell marspot-core marspot-session; do
  install -m 0755 "$ROOT/target/release/$b" "$STAGE/$b"
  install -m 0755 "$ROOT/target/release/$b" "$MARSPOT_STATE_DIR/binaries/current/$b"
  printf 'old' > "$FAKE_APP/Contents/MacOS/$b"
done
awk '/^  cat > "\$LANDER_DIR\/land.sh" <<.LANDER.$/{f=1;next} /^LANDER$/{f=0} f' \
  "$ROOT/bin/install-local.sh" > "$STAGE/land.sh"
[[ -s "$STAGE/land.sh" ]] || fail "could not extract land.sh from install-local.sh"
chmod +x "$STAGE/land.sh"
: > "$MARSPOT_STATE_DIR/agent.plist"
# stands in for `open -n`: starts the app and returns at once
cat > "$MARSPOT_STATE_DIR/open-sandbox.sh" <<EOF
#!/bin/sh
nohup "$SHELL_BIN" >>"$RUNLOG" 2>&1 < /dev/null &
echo \$! >> "$MARSPOT_STATE_DIR/dev.pids"
EOF
chmod +x "$MARSPOT_STATE_DIR/open-sandbox.sh"
STOOD_ASIDE=$(count SHELL_ALREADY_RUNNING)
QUITS=$(count SHELL_QUIT_CLEANUP)

MARSPOT_APP="$FAKE_APP" MARSPOT_LANDER_PLIST="$MARSPOT_STATE_DIR/agent.plist" \
  MARSPOT_LANDER_PROCESS="marspot-lander-test-no-such-process" \
  MARSPOT_LANDER_OPEN="$MARSPOT_STATE_DIR/open-sandbox.sh" \
  "$STAGE/land.sh" &
LANDER=$!
# the hand: launches as soon as the lander has written the bundle
for (( i = 0; i < 300; i++ )); do
  grep -q '^=== landed' "$STAGE/land.log" 2>/dev/null && break
  sleep 0.05
done
launch
wait "$LANDER"; rc=$?
(( rc == 0 )) || fail "the lander exited $rc"
grep -q '^relaunched' "$STAGE/land.log" || fail "the lander did not reopen the app: $(tail -3 "$STAGE/land.log")"
# the shell that stayed after the quit is the app again: both launches stand aside
RESIDENT="$(dev_shell_pid)"
wait_count SHELL_ALREADY_RUNNING $((STOOD_ASIDE + 2)) 10 \
  || fail "the two launches did not both stand aside for the resident shell"
sleep 6
n=$(wc -w <<<"$(ours marspot-shell)")
(( n == 1 )) || fail "expected one shell after the double reopen, found $n: [$(ours marspot-shell)]"
n=$(wc -w <<<"$(ours marspot-core)")
(( n == 1 )) || fail "expected one core after the double reopen, found $n"
[[ "$(ours marspot-shell)" == "$RESIDENT" ]] || fail "the app is not the shell that had stayed ($RESIDENT)"
(( $(count SHELL_ALREADY_RUNNING) == STOOD_ASIDE + 2 )) \
  || fail "more than two launches stood aside: the lander tried again"
[[ ! -e "$STAGE/marspot-shell" && ! -e "$MARSPOT_STATE_DIR/agent.plist" ]] \
  || fail "the lander is still armed after landing"
(( $(grep -c '^=== .* land.sh ===' "$STAGE/land.log") == 1 )) || fail "the lander ran more than once"
(( $(count SHELL_QUIT_CLEANUP) == QUITS )) || fail "the surviving app quit"
[[ "$(session_pids)" == "$SESSIONS" ]] || fail "sessions changed over the double reopen"
echo "==> one shell ($(ours marspot-shell)), one core, the lander landed once and disarmed"

cleanup
echo
echo "PASS — a second launch stands aside and tells the first,"
echo "       a quit from outside puts the app away, and a double reopen leaves one app."

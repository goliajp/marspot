#!/usr/bin/env bash
#
# Quit the app, open it again: it opens, every time, however soon.
#
# A quit with sessions kept leaves the shell running with no window,
# and LaunchServices sends that process the event of someone opening
# the app.  When the shell answered the quit by replacing its own image
# there were a few hundred milliseconds in which the event had nowhere
# to go: `open` failed with error -600, or was accepted and lost, and
# once it stayed refused for 25 seconds.  Measured on that build: 23 of
# 25 opens refused when asked 10 ms after the quit, and one in about
# thirty never answered at all.  The shell now stays the same image
# until it has the event in hand.
#
# The app is opened through LaunchServices, as a bundle of its own
# name.  Then, 25 times for each delay between the quit being acted on
# and the open (0, 20, 50, 100, 200, 500, 1000, 3000 ms), and ten times
# more at 0 and 100 ms with the app named as an application (`open -a`):
#   - `open` is not refused, not once
#   - the app answers on its command socket within ten seconds
#   - it is the same process as before the quit, and the only shell
# Nothing is retried: a refused open is the failure.
#
# Sandbox-only: the installed app is never touched.  Exits 0 on pass.

set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
SB="$(mktemp -d /tmp/msp-reopen.XXXXXX)"
APP="$SB/Marspot.app"
BID="com.goliajp.marspot.e2e-reopen"
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
ROUNDS="${REOPEN_ROUNDS:-25}"

cleanup() {
  dev_kill_shell_core
  "$LSREGISTER" -u "$APP" >/dev/null 2>&1
  rm -rf "$SB"
}
trap cleanup EXIT
fail() {
  echo "FAIL: $*"
  echo "  --- last 25 lines of $APPLOG:"
  tail -25 "$APPLOG" 2>/dev/null | cut -c1-260 | sed 's/^/    /'
  exit 1
}

for b in "$SHELL_BIN" "$ROOT/target/release/marspot-core" "$ROOT/target/release/marspot-session"; do
  [[ -x $b ]] || fail "$b is not built"
done
"$ROOT/bin/build-app-bundle.sh" --bin-dir "$ROOT/target/release" --out "$APP" >/dev/null || fail "could not assemble the sandbox bundle"
plutil -replace CFBundleIdentifier -string "$BID" "$APP/Contents/Info.plist"
plutil -replace CFBundleName -string MarspotReopenTest "$APP/Contents/Info.plist"

count() { local n; n=$(grep -c $'\t'"$1"$'\t' "$APPLOG" 2>/dev/null); echo "${n:-0}"; }
ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time*1000'; }
shells() { pgrep -x marspot-shell | while read -r p; do _dev_is_ours "$p" && echo "$p"; done | tr '\n' ' '; }
answers() { local i; for (( i = 0; i < 200; i++ )); do "$SHELL_BIN" --panes >/dev/null 2>&1 && return 0; sleep 0.05; done; return 1; }
app_env=(--env "MARSPOT_STATE_DIR=$MARSPOT_STATE_DIR" --env "MARSPOT_SESSION_BIN=$ROOT/target/release/marspot-session"
  --env "MARSPOT_CORE_BIN=$ROOT/target/release/marspot-core" --env MARSPOT_DEV_SANDBOX=1 --env MARSPOT_MANUAL_UPDATE_ONLY=1)

dev_kill_shell_core
rm -rf "$MARSPOT_STATE_DIR"; mkdir -p "$MARSPOT_STATE_DIR/logs"
open -g "$APP" "${app_env[@]}" || fail "LaunchServices did not open the sandbox bundle"
for (( i = 0; i < 300; i++ )); do L1="$(dev_shell_pid)"; [[ -n $L1 ]] && "$SHELL_BIN" --panes >/dev/null 2>&1 && break; sleep 0.2; done
[[ -n ${L1:-} ]] && answers || fail "the app never came up"
sleep 2
echo "==> app $L1 up; $ROUNDS quits and opens for each delay"

cycles=0
cycle() { # delay ms, how (path|app), label
  local d=$1 how=$2 i q ans t0 t1 shown
  q=$(count SHELL_QUIT_CLEANUP); shown=$(count WINDOW_FIRST_PRESENT)
  ans="$(osascript -l JavaScript -e "ObjC.import('AppKit');
    \$.NSRunningApplication.runningApplicationWithProcessIdentifier($L1).terminate" 2>&1)"
  [[ $ans == true ]] || fail "[$3] the quit request was not taken: $ans"
  for (( i = 0; i < 2000; i++ )); do (( $(count SHELL_QUIT_CLEANUP) > q )) && break; sleep 0.005; done
  (( $(count SHELL_QUIT_CLEANUP) > q )) || fail "[$3] the app did not act on the quit"
  t0=$(ms)
  perl -e "select(undef,undef,undef,$d/1000)"
  t1=$(ms)
  if [[ $how == app ]]; then open -g -a "$APP" 2>"$SB/err"; else open -g "$APP" 2>"$SB/err"; fi \
    || fail "[$3] open was refused $((t1 - t0)) ms after the quit was acted on: $(head -1 "$SB/err")"
  answers || fail "[$3] open was accepted $((t1 - t0)) ms after the quit and the app did not come back (shells: $(shells))"
  [[ "$(shells)" == "$L1 " ]] || fail "[$3] the app that came back is not the one shell that was quit ($L1): $(shells)"
  # the next quit is asked of an app that has its window up, as a
  # person asks it
  for (( i = 0; i < 200; i++ )); do (( $(count WINDOW_FIRST_PRESENT) > shown )) && break; sleep 0.05; done
  (( $(count WINDOW_FIRST_PRESENT) > shown )) || fail "[$3] the app came back and showed no window"
  cycles=$((cycles + 1))
  sleep 0.3
}

for d in 0 20 50 100 200 500 1000 3000; do
  for (( r = 1; r <= ROUNDS; r++ )); do cycle "$d" path "$d ms, round $r"; done
  echo "    $d ms after the quit: $ROUNDS of $ROUNDS opened"
done
for d in 0 100; do
  for (( r = 1; r <= 10; r++ )); do cycle "$d" app "as an application, $d ms, round $r"; done
  echo "    as an application, $d ms after the quit: 10 of 10 opened"
done
(( $(count SHELL_ALREADY_RUNNING) == 0 )) || fail "a second instance was started $(count SHELL_ALREADY_RUNNING) time(s)"

echo
echo "PASS: $cycles quits each followed by an open; none refused, none lost, one shell ($L1) throughout"

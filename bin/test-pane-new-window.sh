#!/usr/bin/env bash
#
# A pane opened in a new window of its own (`pane open --new-window`),
# asked for from inside a pane of a sandboxed app with one window.
#
#   1. The answer names a window; the app shows one more window, and the
#      new pane is alone in it, in a window that is not the caller's.
#   2. The new pane has no parent: it is not a shadow, nothing is listed
#      under the caller's pane, and the caller's window did not grow.
#   3. Closing the pane closes its window; the caller's window and pane
#      stay as they were.
#   4. The open is in the audit log.
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app is
# never touched.  Opens windows, so it runs on the runner:
#   bin/test-e2e-remote.sh test-pane-new-window.sh
# Exits 0 when every check passed.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
source "$ROOT/bin/_sandbox-panes.sh"

SD="$MARSPOT_STATE_DIR"
BIN="$ROOT/target/release"
CLI="$BIN/marspot-shell"
PEER="$BIN/marspot-peer"
LOG="$SD/logs/marspot.log"

die() {
  echo "FAIL: $*"
  echo "  --- window and spawn events in $LOG:"
  grep -E $'\t(WINDOW_[A-Z_]+|PANE_SPAWN[A-Z_]*|PANE_CONTROL_REPLY|core\\.window[a-z_.]*)\t' "$LOG" 2>/dev/null | cut -f7- | tail -40 | sed 's/^/    /'
  exit 1
}
trap dev_kill_shell_core EXIT

for b in marspot-shell marspot-core marspot-session marspot-peer; do
  [[ -x $BIN/$b ]] || die "$BIN/$b is not built (cargo build --release --bin marspot-shell --bin marspot-core --bin marspot-session && cargo build --release -p marspot-peer)"
done

pc() { in_sd "$SD" "$PEER" pane "$@"; }
last_of() { # event tag, field
  grep -a "$(printf '\t%s\t' "$1")" "$LOG" | tail -1 | tr '\t' '\n' | sed -n "s/^$2=//p"
}
windows() { dev_windows_of "$(dev_shell_pid)" | grep -c '^0 '; }
wait_windows() { # count, seconds
  local i
  for (( i = 0; i < $2 * 2; i++ )); do
    [[ $(windows) == "$1" ]] && return 0
    sleep 0.5
  done
  return 1
}
# one pane of the list, by id: prints `shadow parent cell`, or nothing
row() {
  pc list --json 2>/dev/null | python3 -c '
import json,sys
for p in json.load(sys.stdin)["result"]["panes"]:
    if p["id"] == int(sys.argv[1]):
        print(p["shadow"], p["parent"], p["cell"])' "$1"
}
children_of() {
  pc list --json 2>/dev/null | python3 -c '
import json,sys
print(sum(1 for p in json.load(sys.stdin)["result"]["panes"] if p["parent"] == int(sys.argv[1])))' "$1"
}
window_of_cell() { sed -n 's/^.*w(\([0-9]*\),.*$/\1/p' <<<"$1"; }

dev_kill_shell_core
dev_wipe_state
rm -rf "$SD/sessions" "$SD/retired"
mkdir -p "$SD/logs"

env SHELL=/bin/zsh MARSPOT_SESSION_BIN="$BIN/marspot-session" \
  nohup "$CLI" > "$(dev_run_log pane-new-window)" 2>&1 < /dev/null &
dev_record_pid $!
wait_count "$SD" HELLO_ACK 0 90 || die "the core never said hello"
wait_count "$SD" PANE_CONTROL_CAPS_SEEN 0 30 || die "the shell never heard what the core does with pane control"
CAPS=$(last_of PANE_CONTROL_CAPS_SEEN caps)
(( CAPS & 32 )) || die "the core does not announce opening a pane in a new window (caps=$CAPS)"

HOST=$(first_pane "$SD") || die "no pane came up"
pane_ready "$SD" "$HOST"
wait_windows 1 30 || die "one window expected at the start, the app shows $(windows)"
HOST_ROW=$(row "$HOST")
HOST_WIN=$(window_of_cell "$HOST_ROW")
[[ -n $HOST_WIN ]] || die "the caller's pane has no cell: '$HOST_ROW'"
echo "==> caller pane $HOST in window $HOST_WIN"

echo "════ 1. a new window ════"
OUT="$SD/opened.json"
check "pane open --new-window from inside the caller's pane" \
  pc run "$HOST" --timeout 30 "MARSPOT_STATE_DIR='$SD' '$PEER' pane open --new-window --json > '$OUT'"
read -r NEW NEW_WIN_ID < <(python3 -c '
import json,sys
r = json.load(open(sys.argv[1]))["result"]
print(r["id"], r["window"])' "$OUT" 2>/dev/null)
[[ -n ${NEW:-} ]] || die "no pane id in the answer: $(cat "$OUT" 2>/dev/null)"
check "the answer names a window (${NEW_WIN_ID:-none})" test "${NEW_WIN_ID:-None}" != None
check "the app shows two windows" wait_windows 2 20
check "the core built the window around the asked-for pane" test "$(last_of PANE_SPAWN_NEW_WINDOW session)" = "$NEW"
NEW_ROW=""
for (( i = 0; i < 40; i++ )); do
  NEW_ROW=$(row "$NEW")
  [[ $NEW_ROW == *"w("* ]] && break
  sleep 0.25
done
NEW_WIN=$(window_of_cell "$NEW_ROW")
check "the new pane has a cell ($NEW_ROW)" test -n "$NEW_WIN"
check "in a window that is not the caller's" test "$NEW_WIN" != "$HOST_WIN"
check "alone there" test "$(pc list --json | python3 -c '
import json,sys
w = sys.argv[1]
print(sum(1 for p in json.load(sys.stdin)["result"]["panes"] if p["cell"] and p["cell"].startswith("w(" + w + ",")))' "$NEW_WIN")" = 1

echo "════ 2. no parent ════"
check "the new pane is not a shadow and has no parent ($NEW_ROW)" test "${NEW_ROW%% w(*}" = "False None"
check "nothing is listed under the caller's pane" test "$(children_of "$HOST")" = 0
check "the caller's window holds only the caller" test "$(pc list --json | python3 -c '
import json,sys
w = sys.argv[1]
print(sum(1 for p in json.load(sys.stdin)["result"]["panes"] if p["cell"] and p["cell"].startswith("w(" + w + ",")))' "$HOST_WIN")" = 1
check "the new pane's shell runs commands" pc run "$NEW" --timeout 20 true

echo "════ 3. closing it closes the window ════"
empty=$(count "$SD" WINDOW_EMPTY)
check "pane close" pc close "$NEW"
check "the core asked for its window to close" wait_count "$SD" WINDOW_EMPTY "$empty" 10
check "the app shows one window again" wait_windows 1 20
check "the closed pane is not listed" test -z "$(row "$NEW")"
check "the caller's pane is where it was" test "$(row "$HOST")" = "$HOST_ROW"
check "and its shell runs commands" pc run "$HOST" --timeout 20 true

echo "════ 4. audit ════"
check "the open is in the audit log as new_window" bash -c "$(printf '%q' "$PEER") pane audit --json | grep -q 'pane.open.*new_window'"

check "the app is still up" alive "$(dev_shell_pid)"
check "no panic in the log" bash -c "! grep -qa $'\tPANIC' $(printf '%q' "$LOG")"

echo
echo "$((panes_checks - panes_failures - panes_not_applicable))/$panes_checks passed, $panes_not_applicable not applicable"
(( panes_failures == 0 ))

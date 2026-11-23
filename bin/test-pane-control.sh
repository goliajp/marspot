#!/usr/bin/env bash
#
# The pane control socket from the command line, in a sandboxed app:
# `marspot-peer pane …` against one window of three panes.
#
#   1. `pane list` names the three panes, in words and as JSON.
#   2. `pane run` hands back the command's own exit status in a zsh, a
#      bash and a fish pane (fish only where it is installed; said and
#      counted as not applicable otherwise).
#   3. `pane read --wait-text` returns once the text is printed, and on
#      its timeout says so without failing.
#   4. The command socket's `marspot-shell --panes / --send / --read`
#      behave as before.
#   5. Input to a session whose window was closed is answered
#      `not_attached`.
#   6. A wait in progress still comes true across a restart of the core
#      and across the session moving to another build.
#   7. Shadow panes (`pane open` → `run` → `read` → `resize` → `close`,
#      and one moved into the window with `pane place`) when the core
#      announces it opens them (PaneControlCaps bit 1); before that,
#      `pane open` and `pane close` answer `old_session` and nothing is
#      opened or closed.
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app is
# never touched.  Opens windows, so it runs on the runner:
#   bin/test-e2e-remote.sh test-pane-control.sh
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
  echo "  --- pane control events in $LOG:"
  grep -E $'\t(PANE_CONTROL[A-Z_]*|shell\\.control[a-z_.]*|HELLO_ACK|UPDATE_[A-Z]+|L3_EXECV_INVOKE)\t' "$LOG" 2>/dev/null | cut -f7- | tail -30 | sed 's/^/    /'
  exit 1
}
trap dev_kill_shell_core EXIT

for b in marspot-shell marspot-core marspot-session marspot-peer; do
  [[ -x $BIN/$b ]] || die "$BIN/$b is not built (cargo build --release --bin marspot-shell --bin marspot-core --bin marspot-session && cargo build --release -p marspot-peer)"
done

pc() { in_sd "$SD" "$PEER" pane "$@"; }
ids_json() { python3 -c 'import json,sys; print(" ".join(str(p["id"]) for p in json.load(sys.stdin)["result"]["panes"]))'; }
field() { # json key on stdin
  python3 -c 'import json,sys; v=json.load(sys.stdin); r=v.get("result",v); print(r.get(sys.argv[1], ""))' "$1"
}

dev_kill_shell_core
dev_wipe_state
rm -rf "$SD/sessions" "$SD/retired"
mkdir -p "$SD/logs" "$SD/zdot"
printf 'PS1="%%# "\nunsetopt BEEP\n' > "$SD/zdot/.zshrc"

# one window, three panes side by side; sid 0 = each spawns fresh
python3 - "$SD" <<'PY' || die "could not write the layout"
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1])
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
def pane(): return struct.pack('<Q', 0) + s('') + s('')
body = struct.pack('<II', 0xA5505010, 2) + struct.pack('<HH', 0, 1)
body += struct.pack('<HHHH', 3, 1, 0, 3) + pane() + pane() + pane()
(d / 'shell-state.bin').write_bytes(body)
frames = struct.pack('<IIH', 0xA5505011, 2, 1) + struct.pack('<Idddd', 0, 80.0, 200.0, 1500.0, 700.0)
(d / 'window-state.bin').write_bytes(frames)
PY

# an extra window that opens a fresh pane and closes again leaves that
# pane's session running with no window: the `not_attached` case
env SHELL=/bin/zsh ZDOTDIR="$SD/zdot" MARSPOT_SESSION_BIN="$BIN/marspot-session" \
  MARSPOT_DEV_EXTRA_WINDOWS=1 MARSPOT_DEV_CLOSE_EXTRA=1 \
  nohup "$CLI" > "$(dev_run_log pane-control)" 2>&1 < /dev/null &
dev_record_pid $!
wait_count "$SD" HELLO_ACK 0 90 || die "the core never said hello"
wait_count "$SD" PANE_CONTROL_CAPS_SEEN 0 30 || die "the shell never heard what the core does with pane control"
CAPS=$(grep -a $'\tPANE_CONTROL_CAPS_SEEN\t' "$LOG" | tail -1 | grep -o $'\tcaps=[0-9]*' | cut -d= -f2)
echo "==> the core announced pane control caps=$CAPS"

IDS=""
for (( i = 0; i < 100; i++ )); do
  IDS=$(pc list --json 2>/dev/null | ids_json 2>/dev/null)
  (( $(wc -w <<<"$IDS") >= 3 )) && break
  sleep 0.5
done
(( $(wc -w <<<"$IDS") >= 3 )) || die "pane list never showed three panes: '$IDS'"
LAYOUT=$(python3 -c '
import json,sys
ps = json.load(sys.stdin)["result"]["panes"]
print(" ".join(str(p["id"]) for p in sorted((p for p in ps if p["cell"]), key=lambda p: p["cell"])))' < <(pc list --json))
read -r P1 P2 P3 <<<"$LAYOUT"
[[ -n ${P3:-} ]] || die "three panes with cells expected, got '$LAYOUT'"

echo "════ 1. pane list ════"
names_all() { # listing text, ids…
  local text=$1 id
  shift
  grep -q '^ *id ' <<<"$text" || return 1
  for id in "$@"; do grep -qw "$id" <<<"$text" || return 1; done
}
check "the listing has a header and the three panes" names_all "$(pc list)" "$P1" "$P2" "$P3"
all_have_state() {
  pc list --json | python3 -c 'import json,sys; ps=json.load(sys.stdin)["result"]["panes"]; sys.exit(0 if ps and all("state" in p and "detached" in p for p in ps) else 1)'
}
check "every pane in the JSON has a state" all_have_state

# a shell that answers: retried only while the pane is not at its prompt yet
at_shell() { # pane
  local i
  for (( i = 0; i < 30; i++ )); do
    pc run "$1" --timeout 10 true >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}
runs() { # pane, expected status, command…
  local pane=$1 want=$2
  shift 2
  pc run "$pane" --timeout 20 "$@" >/dev/null 2>"$SD/run.err"
  local got=$?
  [[ $got == "$want" ]] || { echo "    $* → $got, wanted $want: $(cat "$SD/run.err")"; return 1; }
}

echo "════ 2. pane run in zsh, bash and fish ════"
at_shell "$P1" || die "pane $P1's zsh never ran a command"
check "zsh: true → 0" runs "$P1" 0 true
check "zsh: false → 1" runs "$P1" 1 false
check "zsh: (exit 7) → 7" runs "$P1" 7 '(exit 7)'
check "zsh: the answer shows what it printed" bash -c "$(printf '%q' "$PEER") pane run $P1 'echo RUN-\$((6*7))Z' | grep -q RUN-42Z"
check "zsh: a command ending in a comment still reports" runs "$P1" 0 'true # trailing comment'

pc send "$P2" 'exec /bin/bash --norc --noprofile' >/dev/null || die "could not start bash in pane $P2"
at_shell "$P2" || die "pane $P2's bash never ran a command"
check "bash: true → 0" runs "$P2" 0 true
check "bash: false → 1" runs "$P2" 1 false
check "bash: (exit 3) → 3" runs "$P2" 3 '(exit 3)'

if FISH=$(command -v fish); then
  pc send "$P3" "exec $FISH --no-config" >/dev/null || die "could not start fish in pane $P3"
  at_shell "$P3" || die "pane $P3's fish never ran a command"
  check "fish: true → 0" runs "$P3" 0 true
  check "fish: false → 1" runs "$P3" 1 false
  check "fish: (exit 5 in a sub-shell) → 5" runs "$P3" 5 'fish -c "exit 5"'
else
  not_applicable "pane run in a fish pane" "fish is not installed on $(hostname -s)"
fi

echo "════ 3. pane read --wait-text ════"
pc send "$P1" 'sleep 1; echo WAIT-$((40+2))X' >/dev/null
check "the wait returns the screen with the text" bash -c "$(printf '%q' "$PEER") pane read $P1 --wait-text WAIT-42X --timeout 20 | grep -q WAIT-42X"
check "a wait that does not come true says timeout" bash -c "$(printf '%q' "$PEER") pane read $P1 --wait-text NEVER-PRINTED --timeout 2 2>&1 >/dev/null | grep -q timeout"
check "pane send answers with a position" bash -c "$(printf '%q' "$PEER") pane send --json $P1 'true' | grep -q '\"pos\":[0-9]'"
check "a pane that does not exist is no_such_pane" bash -c "$(printf '%q' "$PEER") pane send 999999 x 2>&1 | grep -q no_such_pane"

echo "════ 4. marspot-shell --panes / --send / --read ════"
lists_all() { # text, ids…
  local text=$1 id
  shift
  for id in "$@"; do grep -qw "$id" <<<"$text" || return 1; done
}
check "--panes lists the three panes" lists_all "$(in_sd "$SD" "$CLI" --panes)" "$P1" "$P2" "$P3"
type_in_pane "$SD" "$P1" 'echo OLD-$((1+1))Y'
check "--send typed and --read reads it back" wait_shows "$SD" "$P1" "OLD-2Y" 15

echo "════ 5. a session whose window is closed ════"
DETACHED=""
for (( i = 0; i < 60; i++ )); do
  DETACHED=$(pc list --json 2>/dev/null | python3 -c "
import json,sys
ps = json.load(sys.stdin)['result']['panes']
print(' '.join(str(p['id']) for p in ps if p['detached']))" 2>/dev/null)
  [[ -n $DETACHED ]] && (( $(count "$SD" DEV_CLOSE_EXTRA) > 0 )) && break
  sleep 0.5
done
D=${DETACHED%% *}
sleep 2
if [[ -n $D ]]; then
  check "input to session $D answers not_attached" bash -c "$(printf '%q' "$PEER") pane send $D 'echo x' 2>&1 | grep -q not_attached"
  check "and so does a wait on it" bash -c "$(printf '%q' "$PEER") pane read $D --wait-change --timeout 3 2>&1 | grep -q not_attached"
else
  check "a session outlived its closed window (listed as detached)" false
  pc list
fi

echo "════ 6. waits across a core restart and a session update ════"
WAITED="$SD/wait-a.out"
pc read "$P1" --wait-text SURV-A-9X --timeout 150 > "$WAITED" 2>&1 &
waiter=$!
sleep 1
resyncs=$(count "$SD" PANE_CONTROL_RESYNC)
swap_core "$SD" "$BIN/marspot-core"
wait_count "$SD" PANE_CONTROL_RESYNC "$resyncs" 30 || die "the new core's caps never made the shell register its waits again"
type_in_pane "$SD" "$P1" 'echo SURV-A-$((3*3))X'
wait "$waiter"
check "a wait made before the core restarted comes true after it" grep -q SURV-A-9X "$WAITED"

# a session build of another name that runs this one: the session moves to it
WRAP="$SD/work/marspot-session"
mkdir -p "$SD/work"
printf '#!/bin/sh\n# MARSPOT_FP=panectl%s|1|END\nexec %q "$@"\n' "$$" "$BIN/marspot-session" > "$WRAP"
chmod +x "$WRAP"
WAITED="$SD/wait-b.out"
pc read "$P1" --wait-text SURV-B-8X --timeout 150 > "$WAITED" 2>&1 &
waiter=$!
sleep 1
execs=$(count "$SD" L3_EXECV_INVOKE)
swap_core "$SD" "$BIN/marspot-core" "$WRAP"
if wait_count "$SD" L3_EXECV_INVOKE "$execs" 120; then
  sleep 2
  type_in_pane "$SD" "$P1" 'echo SURV-B-$((4*2))X'
  wait "$waiter"
  check "a wait made before the session moved to another build comes true after it" grep -q SURV-B-8X "$WAITED"
else
  kill "$waiter" 2>/dev/null
  check "the session moved to the staged build" false
fi

echo "════ 7. shadow panes ════"
before=$(pc list --json | ids_json)
OPENED=$(pc open --parent "$P1" --json 2>&1)
if (( (CAPS & 2) == 0 )) || [[ $OPENED == *old_session* ]]; then
  check "pane open answers old_session" grep -q old_session <<<"$OPENED"
  # a window pane is closed for real once the core does bit 1
  (( (CAPS & 2) == 0 )) && check "pane close answers old_session" bash -c "$(printf '%q' "$PEER") pane close $P2 2>&1 | grep -q old_session"
  check "pane resize answers old_session" bash -c "$(printf '%q' "$PEER") pane resize $P2 100x30 2>&1 | grep -q old_session"
  check "pane place answers old_session" bash -c "$(printf '%q' "$PEER") pane place $P2 shadow 2>&1 | grep -q old_session"
  check "nothing was opened or closed" test "$(pc list --json | ids_json)" = "$before"
  not_applicable "shadow pane open → run → read → close" "the core or the shell does not do shadow panes yet"
else
  S=$(field id <<<"$OPENED")
  # the list comes from the shell's view of the registry, which takes a moment to see a new session
  check "pane open made a shadow pane" bash -c "[[ -n '$S' ]] && for i in \$(seq 25); do $(printf '%q' "$PEER") pane list --json | python3 -c 'import json,sys; ps=json.load(sys.stdin)[\"result\"][\"panes\"]; sys.exit(0 if any(str(p[\"id\"])==\"$S\" and p[\"shadow\"] and p[\"parent\"]==$P1 for p in ps) else 1)' && exit 0; sleep 0.2; done; exit 1"
  at_shell "$S" || check "the shadow pane's shell answered" false
  check "shadow: run → 4" runs "$S" 4 '(exit 4)'
  pc send "$S" 'echo SHADOW-$((5*5))Q' >/dev/null
  check "shadow: read --wait-text" bash -c "$(printf '%q' "$PEER") pane read $S --wait-text SHADOW-25Q --timeout 20 | grep -q SHADOW-25Q"
  check "a window pane is not resizable" bash -c "$(printf '%q' "$PEER") pane resize $P2 100x30 2>&1 | grep -q not_resizable"
  check "shadow: resize" pc resize "$S" 100x30
  check "shadow: close" pc close "$S"
  check "the shadow pane is gone" bash -c "for i in \$(seq 50); do $(printf '%q' "$PEER") pane list --json | grep -q '\"id\":$S,' || exit 0; sleep 0.2; done; exit 1"
  S2=$(pc open --parent "$P1" --json 2>&1 | field id)
  check "a second shadow pane opened" test -n "$S2"
  check "shadow: place window" pc place "$S2" window
  check "the placed pane is in the window, no shadow" bash -c "for i in \$(seq 25); do $(printf '%q' "$PEER") pane list --json | python3 -c 'import json,sys; ps=json.load(sys.stdin)[\"result\"][\"panes\"]; sys.exit(0 if any(str(p[\"id\"])==\"$S2\" and not p[\"shadow\"] for p in ps) else 1)' && exit 0; sleep 0.2; done; exit 1"
  check "the placed pane closes as a window pane" pc close "$S2"
fi

check "the app is still up" alive "$(dev_shell_pid)"
check "no panic in the log" bash -c "! grep -qa $'\tPANIC' $(printf '%q' "$LOG")"

echo
echo "$((panes_checks - panes_failures - panes_not_applicable))/$panes_checks passed, $panes_not_applicable not applicable"
(( panes_failures == 0 ))

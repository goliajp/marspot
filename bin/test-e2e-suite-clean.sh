#!/usr/bin/env bash
# bin/test-e2e-suite-clean.sh — the decision of bin/_e2e-suite-clean.sh
# over saved inputs: what a runner shows after a clean suite, and after
# each kind of leftover, unreadable or dead source, alone on the host or
# next to another holder of its lock.  Local, a few seconds; reads no
# live log and launches nothing.  The log lines are in the formats the
# Dock and tccd print on macOS 26 and 27 (paths not ours replaced).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d /tmp/marspot-suite-clean.XXXXXX)"
trap 'rm -rf "${T:?}"' EXIT
failed=0

# shellcheck source=bin/_e2e-suite-clean-fixtures.sh
source "$ROOT/bin/_e2e-suite-clean-fixtures.sh"

base clean
expect clean 0 "dock: inserts=1 removal_lines=1 removal_events=1" "dock: removals by reason: 1 _emptyDeleteList()" "window: alone"

base tile-removed
{ echo "$HEADER"; launch 01.000; remove 05.000 "$TILE_REMOVED"; } >"$T/tile-removed/in/dock.log"
expect tile-removed 0 "dock: removals by reason: 1 $TILE_REMOVED"

base insert
launch 06.000 >>"$T/insert/in/dock.log"
expect insert 0 "dock: inserts=2 removal_lines=1 removal_events=1" "dock: tiles at once=1 (most allowed 2, insert index 37..37, 37 before the suite)"

# one removal logged under both reasons is one removal
base logged-twice
{ echo "$HEADER"; launch 01.000; launch 02.000; remove 05.000; remove 05.004 "$TILE_REMOVED"; } >"$T/logged-twice/in/dock.log"
expect logged-twice 0 "dock: inserts=2 removal_lines=2 removal_events=1"

base two-removals
{ echo "$HEADER"; launch 01.000; launch 02.000; remove 05.000; remove 05.104; } >"$T/two-removals/in/dock.log"
expect two-removals 0 "removal_events=2"

base no-tiles
echo "$HEADER" >"$T/no-tiles/in/dock.log"
expect no-tiles 0 "dock: inserts=0 removal_lines=0 removal_events=0"

base dead-dock
echo "$HEADER" >"$T/dead-dock/in/dock.log"
sed -i '' 's/^dock_live=1/dock_live=0/' "$T/dead-dock/before"
expect dead-dock 2 "unread: dock log: no tile line in the suite nor in the 24 h probe"

base dead-tcc
printf '{"count":0,"finished":1}\n' >"$T/dead-tcc/in/tcc.ndjson"
sed -i '' 's/^tcc_live=1/tcc_live=0/' "$T/dead-tcc/before"
expect dead-tcc 2 "unread: tcc log: no request in the suite nor in the 24 h probe"

base persistent
tiles A >"$T/persistent/in/persistent.txt"
expect persistent 1 "left: persistent tile gone: /Applications/B.app/"

# the recent list is full: ours pushes the oldest out and the count stays
base recent-replaced
sed -i '' "s|^recent_entries=.*|recent_entries=$(entries C D E)|" "$T/recent-replaced/before"
# shellcheck disable=SC2086
{ echo "("; tile D; tile E; tile $OURS_TILE; echo ")"; } >"$T/recent-replaced/in/recent.txt"
expect recent-replaced 1 "left: recent tile added: /tmp/msp-e2e-1-1-lander/Marspot.app/ com.goliajp.marspot Marspot" \
  "dock: recent tile gone: /Applications/C.app/" "after:  fg=1 persistent=2 recent=3"

base recent-other
tiles Other >"$T/recent-other/in/recent.txt"
expect recent-other 1 "left: recent tile added: /Applications/Other.app/"

base record
printf '%s\n%s\n' "$DOCK" "$MARSPOT" >"$T/record/in/lsappinfo.txt"
expect record 1 "left: record: BackgroundOnly pid=4242 /tmp/msp-e2e-1-1-lander/Marspot.app"

base known
printf '%s\n%s\n' "$DOCK" "$MARSPOT" >"$T/known/in/lsappinfo.txt"
expect known 0 "clean=0" -- --known "4242:/tmp/msp-e2e-1-1-lander/Marspot.app"

base known-private
printf '%s\n%s\n' "$DOCK" "$MARSPOT_PRIVATE" >"$T/known-private/in/lsappinfo.txt"
expect known-private 0 "clean=0" -- --known "4242:/tmp/msp-e2e-1-1-lander/Marspot.app"

base listed-before
printf '%s\n%s\n' "$DOCK" "$MARSPOT" >"$T/listed-before/in/lsappinfo.txt"
echo "known=4242:/tmp/msp-e2e-1-1-lander/Marspot.app" >>"$T/listed-before/before"
expect listed-before 0 "clean=0"

base foreground
printf '%s\n%s\n' "$DOCK" "${MARSPOT//BackgroundOnly/Foreground}" >"$T/foreground/in/lsappinfo.txt"
expect foreground 1 "left: foreground apps changed: added ['/tmp/msp-e2e-1-1-lander/Marspot.app'], gone []"

# same count, another app: the set is compared, not the count
base foreground-swap
printf '%s\n%s\n' "${DOCK//type=\"Foreground\"/type=\"BackgroundOnly\"}" "${MARSPOT//BackgroundOnly/Foreground}" \
  >"$T/foreground-swap/in/lsappinfo.txt"
expect foreground-swap 1 "left: foreground apps changed: added ['/tmp/msp-e2e-1-1-lander/Marspot.app'], gone ['com.apple.finder']"

base tcc
printf '%s\n%s\n{"count":4,"finished":1}\n' "$TCC_OTHER" "$TCC_OURS" >"$T/tcc/in/tcc.ndjson"
expect tcc 1 "left: tcc: kTCCServiceAppleEvents $BUILD $OTHER /System/Library"

base tcc-accessing
printf '%s\n%s\n{"count":4,"finished":1}\n' "$TCC_OTHER" "$TCC_ACCESSING" >"$T/tcc-accessing/in/tcc.ndjson"
expect tcc-accessing 1 "left: tcc: kTCCServiceListenEvent $OTHER ${LANDER#/private}"

base listen-preflight
printf '%s\n%s\n{"count":5,"finished":1}\n' "$TCC_OTHER" "$TCC_LISTEN" >"$T/listen-preflight/in/tcc.ndjson"
expect listen-preflight 0 "tcc: requests=2 ours=1 listen_event_preflights=1 screen_capture_preflights=0 developer_tool_preflights=0" "info: tcc: kTCCServiceListenEvent $OTHER $BUILD"

base listen-prompt
printf '%s\n%s\n{"count":6,"finished":1}\n' "$TCC_OTHER" "$TCC_LISTEN_PROMPT" >"$T/listen-prompt/in/tcc.ndjson"
expect listen-prompt 1 "left: tcc: kTCCServiceListenEvent $OTHER $BUILD"

base listen-ask
printf '%s\n%s\n{"count":6,"finished":1}\n' "$TCC_OTHER" "$TCC_LISTEN_ASK" >"$T/listen-ask/in/tcc.ndjson"
expect listen-ask 1 "left: tcc: kTCCServiceListenEvent $OTHER $BUILD"

# a second request under the same msgID (pid.counter wraps) is never folded into a preflight
base listen-collide
printf '%s\n%s\n%s\n{"count":8,"finished":1}\n' "$TCC_OTHER" "$TCC_LISTEN" "${TCC_LISTEN_ASK//405.1/403.1}" \
  >"$T/listen-collide/in/tcc.ndjson"
expect listen-collide 1 "left: tcc: kTCCServiceListenEvent $OTHER $BUILD"

# the window server's screen-recording preflight before the shell captures its own window
base capture-preflight
ndjson "$(ctx 175.2117 kTCCServiceScreenCapture)" "$(attribution 175.2117 "$OTHER" "$BUILD")" "$(result 175.2117)" \
  >"$T/capture-preflight/in/tcc.ndjson"
echo '{"count":3,"finished":1}' >>"$T/capture-preflight/in/tcc.ndjson"
expect capture-preflight 0 "screen_capture_preflights=1" "info: tcc: kTCCServiceScreenCapture $OTHER $BUILD"

# Gatekeeper asks whether a bundle of this run is a developer tool when it first runs a new binary
devtool() { # msgID, accessing path
  ndjson "$(ctx "$1" kTCCServiceDeveloperTool)" \
    "AUTHREQ_ATTRIBUTION: msgID=$1, attribution={accessing={TCCDProcess: identifier=marspot_shell, pid=68466, auid=501, euid=501, binary_path=$2}, requesting={TCCDProcess: identifier=com.apple.syspolicyd, pid=274, auid=0, euid=0, binary_path=/usr/libexec/syspolicyd}, }," \
    "$(result "$1")"
}
base devtool-ours
{ devtool 274.1 "$LANDER"; echo '{"count":3,"finished":1}'; } >"$T/devtool-ours/in/tcc.ndjson"
expect devtool-ours 0 "developer_tool_preflights=1"
base devtool-elsewhere
{ devtool 274.2 "$BUILD"; echo '{"count":3,"finished":1}'; } >"$T/devtool-elsewhere/in/tcc.ndjson"
expect devtool-elsewhere 1 "left: tcc: kTCCServiceDeveloperTool"
base devtool-build-tree
{ devtool 274.3 "$BUILD"; echo '{"count":3,"finished":1}'; } >"$T/devtool-build-tree/in/tcc.ndjson"
expect devtool-build-tree 0 "developer_tool_preflights=1" -- /Users/u/work/e2e-marspot/

# the same client reuses its msgID for a second preflight seconds later (seen on a runner)
base listen-reuse
printf '%s\n%s\n%s\n{"count":8,"finished":1}\n' "$TCC_OTHER" "$TCC_LISTEN" "$TCC_LISTEN" >"$T/listen-reuse/in/tcc.ndjson"
expect listen-reuse 0 "listen_event_preflights=2"

# two requests under one msgID, interleaved: the one left without its own lines still fails
base listen-interleave
ndjson "$(ctx 403.1 kTCCServiceListenEvent)" \
  "AUTHREQ_CTX: msgID=403.1, function=<private>, service=kTCCServiceListenEvent, preflight=no, query=1," \
  "$(attribution 403.1 "$OTHER" "$BUILD")" "$(result 403.1)" >"$T/listen-interleave/in/tcc.ndjson"
echo '{"count":4,"finished":1}' >>"$T/listen-interleave/in/tcc.ndjson"
expect listen-interleave 1 "left: tcc: kTCCServiceListenEvent $OTHER $BUILD"

base prompt-no-id
printf '%s\n%s\n{"count":3,"finished":1}\n' "$TCC_OTHER" "$(ndjson "AUTHREQ_PROMPTING: service=kTCCServiceCamera")" \
  >"$T/prompt-no-id/in/tcc.ndjson"
expect prompt-no-id 1 "left: tcc: kTCCServiceCamera"

base unread-log
rm "${T:?}/unread-log/in/dock.log"
expect unread-log 2 "unread: dock log: log show gave no output"

base unread-ls
printf 'nothing\n' >"$T/unread-ls/in/lsappinfo.txt"
expect unread-ls 2 "unread: lsappinfo: no list of the console login (no Dock in it)"

base left-wins
rm "${T:?}/left-wins/in/tcc.ndjson"
tiles A B Other >"$T/left-wins/in/persistent.txt"
expect left-wins 1 "unread: tcc log: log show did not finish"

# another holder: the machine-wide counts are shown, not judged
base shared
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/shared/in/status.txt"
{ opened "$APP_URL" 05.996; launch 06.000; remove 07.000; } >>"$T/shared/in/dock.log"
printf '%s\n%s\n{"count":4,"finished":1}\n' "$TCC_OTHER" "$TCC_OURS" >"$T/shared/in/tcc.ndjson"
expect shared 0 "window: shared-window (holding at check: 5000)" "dock: inserts=2 removal_lines=2" \
  "info: tcc: kTCCServiceAppleEvents $BUILD"

base shared-log
printf '1699999990\theavy\t0\ttook\told\t/\n1699999999\theavy\t0\ttook\tsh -c stopped=; ...\t/w\n1700000100\theavy\t3\ttook\tcargo build\t/\n' >"$T/shared-log/in/lock.log"
rm "${T:?}/shared-log/in/persistent.txt"
expect shared-log 2 "window: shared-window (1 lock take/release in the window)" "unread: defaults: com.apple.dock persistent"

base shared-ours
sed -i '' 's/^others_snap=$/others_snap=5000/' "$T/shared-ours/before"
printf '%s\n%s\n' "$DOCK" "$MARSPOT" >"$T/shared-ours/in/lsappinfo.txt"
printf '%s\n%s\n{"count":4,"finished":1}\n' "$TCC_OTHER" "$TCC_ACCESSING" >"$T/shared-ours/in/tcc.ndjson"
expect shared-ours 1 "window: shared-window (holding at snap: 5000)" \
  "left: leftover: BackgroundOnly pid=4242 /tmp/msp-e2e-1-1-lander/Marspot.app" "left: tcc: kTCCServiceListenEvent"

# a kept tile under this run's prefix is ours even next to another holder
base shared-tile
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/shared-tile/in/status.txt"
tiles A B Other "Foo /private/tmp/msp-e2e-1-2-x/Foo.app" >"$T/shared-tile/in/persistent.txt"
expect shared-tile 1 "window: shared-window (holding at check: 5000)" \
  "left: persistent tile added: /tmp/msp-e2e-1-2-x/Foo.app/" "left: persistent tile added: /Applications/Other.app/"

# no other holder seen but a lock source unread: the machine-wide findings are not judged either way
base status-unread
rm "${T:?}/status-unread/in/status.txt"
printf '%s\n%s\n{"count":4,"finished":1}\n' "$TCC_OTHER" "$TCC_OURS" >"$T/status-unread/in/tcc.ndjson"
expect status-unread 2 "window: unknown (lock status at check unread)" "unread: tcc: kTCCServiceAppleEvents"

base log-unread
rm "${T:?}/log-unread/in/lock.log"
printf '%s\n%s\n{"count":4,"finished":1}\n' "$TCC_OTHER" "$TCC_OURS" >"$T/log-unread/in/tcc.ndjson"
expect log-unread 2 "window: unknown (lock log unread)" "unread: tcc: kTCCServiceAppleEvents"

# our own took line can fall in the snap's second; the log has no pid, so it is told by the command
base own-took
printf '1700000000\theavy\t0\ttook\tsh -c stopped=; ...\t/w\n' >"$T/own-took/in/lock.log"
expect own-took 0 "window: alone"

base other-same-second
printf '1700000000\theavy\t0\ttook\tsh -c stopped=; ...\t/w\n1700000000\theavy\t0\ttook\tcargo build\t/\n' \
  >"$T/other-same-second/in/lock.log"
expect other-same-second 0 "window: shared-window (1 lock take/release in the window)"

base own-mismatch
printf '1700000000\theavy\t0\ttook\tsh -c something else\t/w\n' >"$T/own-mismatch/in/lock.log"
printf '%s\n%s\n{"count":4,"finished":1}\n' "$TCC_OTHER" "$TCC_OURS" >"$T/own-mismatch/in/tcc.ndjson"
expect own-mismatch 2 "our own lock take not found in the lock log" "unread: tcc: kTCCServiceAppleEvents"

base own-too-early
printf '1699999994\theavy\t0\ttook\tsh -c stopped=; ...\t/w\n' >"$T/own-too-early/in/lock.log"
printf '%s\n%s\n{"count":4,"finished":1}\n' "$TCC_OTHER" "$TCC_OURS" >"$T/own-too-early/in/tcc.ndjson"
expect own-too-early 2 "our own lock take not found in the lock log" "unread: tcc: kTCCServiceAppleEvents"

base own-unknown
printf '1700000000\theavy\t0\ttook\tsh -c stopped=; ...\t/w\n' >"$T/own-unknown/in/lock.log"
rm "${T:?}/own-unknown/in/status.txt"
printf '%s\n%s\n{"count":4,"finished":1}\n' "$TCC_OTHER" "$TCC_OURS" >"$T/own-unknown/in/tcc.ndjson"
expect own-unknown 2 "1 lock take in the start second, not told apart from ours" "unread: tcc: kTCCServiceAppleEvents"

# snap: the before state, the probes' answer kept for the run, then reused
d="$T/snap"
mkdir -p "$d/in"
printf '%s\n%s\n' "$DOCK" "$MARSPOT_PRIVATE" >"$d/in/lsappinfo.txt"
cp "$T/clean/in/persistent.txt" "$T/clean/in/recent.txt" "$d/in/"
probed 37 >"$d/in/probe-start.txt"
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$d/in/status.txt"
cp "$T/clean/in/dock.log" "$d/in/probe-dock.log"
printf '%s\n' "$TCC_OTHER" >"$d/in/probe-tcc.ndjson"
E2E_CLEAN_INPUTS="$d/in" "$ROOT/bin/_e2e-suite-clean.sh" snap "$d/before" --lock-pid 4000 --live "$d/live"
rm "${d:?}/in/probe-dock.log" "${d:?}/in/probe-tcc.ndjson"
E2E_CLEAN_INPUTS="$d/in" "$ROOT/bin/_e2e-suite-clean.sh" snap "$d/before2" --lock-pid 4000 --live "$d/live"
E2E_CLEAN_INPUTS="$d/in" "$ROOT/bin/_e2e-suite-clean.sh" snap "$d/before3" --lock-pid 4000
ok=1
for l in fg=1 persistent=2 recent=1 "known=4242:/tmp/msp-e2e-1-1-lander/Marspot.app" others_snap=5000 dock_live=1 tcc_live=1 \
  "self_cmd=sh -c stopped=; ..." probe_dock_pid=726 probe_index=37 "recent_entries=[[\"/Applications/C.app/\", \"com.x.C\", \"C\"]]"; do
  grep -qxF -- "$l" "$d/before" && grep -qxF -- "$l" "$d/before2" || ok=0
done
grep -qx 'dock_live=1' "$d/live" && grep -qx 'tcc_live=1' "$d/live" || ok=0
! grep -qx 'dock_live=1' "$d/before3" || ok=0
grep -q '^start_epoch=[0-9]*$' "$d/before" || ok=0
# the Dock probe not run is said, never left out
rm "${d:?}/in/probe-start.txt"
E2E_CLEAN_INPUTS="$d/in" "$ROOT/bin/_e2e-suite-clean.sh" snap "$d/before4" --lock-pid 4000
grep -qx 'probe_unread=the Dock probe did not run' "$d/before4" && ! grep -q '^probe_index=' "$d/before4" || ok=0
if ((ok)); then echo "  ok    snap (before state, live probes cached and reused, probe unread said)"; else
  echo "  FAIL  snap"; head -50 "$d"/before*; failed=1; fi

((failed)) && exit 1
echo "PASS: test-e2e-suite-clean"

#!/usr/bin/env bash
# bin/test-e2e-suite-clean-dock.sh — the Dock's part of the decision of
# bin/_e2e-suite-clean.sh over saved inputs: the Dock probe's index after
# a suite against before it, and how many tiles the suite had at once
# from the Dock's insert indexes; the probe's reading of the Dock's log
# (bin/_dock-probe.py); and the pacing of the ownership walk's quit
# (bin/_ownership-quit.sh) on the probe.  Local, a few seconds; reads no
# live log and launches nothing.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d /tmp/marspot-suite-clean-dock.XXXXXX)"
trap 'rm -rf "${T:?}"' EXIT
failed=0
# shellcheck source=bin/_e2e-suite-clean-fixtures.sh
source "$ROOT/bin/_e2e-suite-clean-fixtures.sh"
RESTORE="restore: the Dock keeps a tile this suite left; it is not restarted here"
# $1 name, then the probe's index before the suite
probe_before() { sed -i '' "s/^probe_index=.*/probe_index=$2/" "$T/$1/before"; }

echo "the probe after the suite against before it"
base probe-equal
expect probe-equal 0 "dock probe: index before=37 after=37"
base probe-more
probed 38 >"$T/probe-more/in/probe-end.txt"
expect probe-more 1 "left: dock: 1 tile(s) more after the suite than before (the probe's index 37, then 38)" "$RESTORE"
base probe-later
printf 'dock_pid=726\nindex=37\nfirst_index=38\n' >"$T/probe-later/in/probe-end.txt"
expect probe-later 0 "dock probe: index before=37 after=37 (38 a moment earlier)"
base probe-fewer
probed 36 >"$T/probe-fewer/in/probe-end.txt"
expect probe-fewer 2 "unread: dock: the probe's index fell from 37 to 36"
base probe-restarted
probed 37 900 >"$T/probe-restarted/in/probe-end.txt"
expect probe-restarted 2 "unread: dock probe: the Dock restarted during the suite (pid 726, then 900)"
base probe-no-removal
echo "unread=the Dock logged no tile removal within 15 s of the probe's exit" >"$T/probe-no-removal/in/probe-end.txt"
expect probe-no-removal 2 "unread: dock probe after the suite: the Dock logged no tile removal within 15 s"
base probe-end-missing
rm "${T:?}/probe-end-missing/in/probe-end.txt"
expect probe-end-missing 2 "unread: dock probe after the suite: did not run"
base probe-start-unread
sed -i '' '/^probe_/d' "$T/probe-start-unread/before"
echo "probe_unread=the Dock logged no tile for the probe within 15 s" >>"$T/probe-start-unread/before"
expect probe-start-unread 2 "unread: dock probe before the suite: the Dock logged no tile for the probe within 15 s"
# another job on the host: a tile more is ours only when the apps running are the same
base probe-shared
probed 38 >"$T/probe-shared/in/probe-end.txt"
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/probe-shared/in/status.txt"
expect probe-shared 1 "window: shared-window" "left: dock: 1 tile(s) more after the suite than before"
base probe-shared-apps
probed 38 >"$T/probe-shared-apps/in/probe-end.txt"
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/probe-shared-apps/in/status.txt"
sed -i '' 's/^fg_set=.*/fg_set=["com.apple.finder", "com.x.other"]/' "$T/probe-shared-apps/before"
# (the apps running changing is a finding of its own)
expect probe-shared-apps 1 "unread: dock: 1 tile(s) more after the suite than before (the probe's index 37, then 38);" \
  "another job was on the host" "left: foreground apps changed"

# next to another job an equal index passes only when the Dock's lines account for every tile
# the window starts before the probe ahead of the suite and ends after the one behind it:
# both probes' lines are in it, and balance
base shared-equal
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/shared-equal/in/status.txt"
{ echo "$HEADER"; probe_tile 00; opened "$APP_URL" 01.996; launch 02.000; remove 05.000; probe_tile 08; } \
  >"$T/shared-equal/in/dock.log"
expect shared-equal 0 "window: shared-window" "dock probe: index before=37 after=37" \
  "dock: inserts=3 removal_lines=3 removal_events=3" \
  "info: dock: removal lines with no matching insert (taken as the one before logged twice): none"
# passed, with a removal line taken as the one before it logged twice: shown, since a second
# removal there is not told from it
base shared-equal-twice
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/shared-equal-twice/in/status.txt"
{ echo "$HEADER"; probe_tile 00; opened "$APP_URL" 01.996; launch 02.000; remove 05.000; remove 05.004 "$TILE_REMOVED"
  probe_tile 08; } >"$T/shared-equal-twice/in/dock.log"
expect shared-equal-twice 0 "dock: inserts=3 removal_lines=4 removal_events=3" \
  "info: dock: removal lines with no matching insert (taken as the one before logged twice): 2026-11-27 10:00:05.004 $TILE_REMOVED"
base shared-unnamed
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/shared-unnamed/in/status.txt"
{ echo "$HEADER"; opened "$APP_URL" 00.996; launch 01.000; launch 02.000; remove 05.000; remove 06.000; } \
  >"$T/shared-unnamed/in/dock.log"
expect shared-unnamed 2 "unread: dock probe: index 37 before and after the suite next to another job" "(1 insert(s) named for no app)"
# another job's app quit just before the snap: its tile, counted in the probe's index, goes during the
# suite, and a tile the suite left takes its place
base shared-hidden
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/shared-hidden/in/status.txt"
{ echo "$HEADER"; remove 00.500; opened "$APP_URL" 00.996; launch 01.000; } >"$T/shared-hidden/in/dock.log"
expect shared-hidden 2 "(a removal with no insert of the suite's window before it)"
base shared-unbalanced
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/shared-unbalanced/in/status.txt"
{ echo "$HEADER"; opened "$APP_URL" 00.996; launch 01.000; opened "$APP_URL" 01.996; launch 02.000 38; remove 05.000; } \
  >"$T/shared-unbalanced/in/dock.log"
expect shared-unbalanced 2 "(2 insert(s) against 1 removal(s))"
base shared-no-log
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/shared-no-log/in/status.txt"
rm "${T:?}/shared-no-log/in/dock.log"
expect shared-no-log 2 "(log show gave no output)"

echo "tiles at once, from the inserts against the probe's index before the suite"
# the probe's tile still at its index when the suite's first tile comes, after the removal line
# that followed the probe: that one goes one above
base probe-tile-stays
{ echo "$HEADER"; probe_tile 00; opened "$APP_URL" 00.996; launch 01.000 38; opened "$APP_URL" 01.996; launch 02.000 39
  remove 03.000; remove 04.000; } >"$T/probe-tile-stays/in/dock.log"
expect probe-tile-stays 0 "dock: tiles at once=2 (most allowed 2, insert index 38..39, 37 before the suite)"
# the suite's first tile before the removal line after the probe: A at 38 beside the probe's tile,
# which then goes (A moves to 37), B at 38, C at 39.  Three held, two counted: not read
base probe-went-late
{ echo "$HEADER"; opened "$DOCKPROBE_URL" 00.000; launch 00.004 37; opened "$APP_URL" 00.300; launch 00.304 38
  remove 00.600; opened "$APP_URL" 01.000; launch 01.004 38; opened "$APP_URL" 02.000; launch 02.004 39
  remove 03.000; remove 04.000; remove 05.000; } >"$T/probe-went-late/in/dock.log"
expect probe-went-late 2 "dock: tiles at once=2 (most allowed 2, insert index 38..39, 37 before the suite)" \
  "unread: dock: tiles at once: the suite's tile at 2026-11-27 10:00:00.304 came before the Dock logged the probe's tile gone, so the count can be one low"
# quit and opened again faster than the Dock drops the last tile (mini2, 2026-11-27 22:16:04):
# a tile per opening, index 25 up to 41, all of them kept
base quick-reopens
probe_before quick-reopens 25
probed 42 >"$T/quick-reopens/in/probe-end.txt"
{ echo "$HEADER"
  for i in $(seq 10 26); do opened "$APP_URL" "$i.000"; launch "$i.004" $((i + 15)); done
  remove 27.000; remove 28.000; } >"$T/quick-reopens/in/dock.log"
expect quick-reopens 1 "left: dock: 17 tiles at once (insert index 25..41, 25 before the suite), more than 2" \
  "left: dock: 17 tile(s) more after the suite than before (the probe's index 25, then 42)" "$RESTORE"

# test-reopen-after-quit.sh on mini2 (2026-11-27 22:29 and 22:58, both stopped by hand: 201 and
# 177 of a full run's 221 openings): the Dock removes the tile a quit left 0.52-0.56 s after the
# quit.  Opened 0-200 ms after the quit, the app's new tile goes in at 57 beside it and the old
# one goes after; opened 500-3000 ms after, the old one went first and the new one goes in at 56.
# The next quit comes 2 s or more after that removal, so no tile is kept: two at once at most
base paced-reopens
probe_before paced-reopens 56
probed 56 >"$T/paced-reopens/in/probe-end.txt"
{ echo "$HEADER"; probe_tile 00 56; opened "$APP_URL" 10.000; launch 10.004 56
  for i in 13 16 19; do opened "$APP_URL" "$i.000"; launch "$i.004" 57; remove "$i.500"; done
  for i in 22 25 28; do remove "$i.000"; opened "$APP_URL" "$i.500"; launch "$i.504" 56; done
  remove 31.000; probe_tile 40 56; } >"$T/paced-reopens/in/dock.log"
expect paced-reopens 0 "dock: tiles at once=2 (most allowed 2, insert index 56..57, 56 before the suite)"

# every insert but the probe's counts, whatever program it is for: a helper of the run
# registering as a regular app is allowed only by the number the suite declares
base helper
probe_before helper 56
probed 56 >"$T/helper/in/probe-end.txt"
{ echo "$HEADER"; opened "$APP_URL" 01.000; launch 01.004 56; opened "$PROBE_URL" 02.000; launch 02.004 57
  opened "$APP_URL" 03.000; launch 03.004 58; remove 04.000; remove 05.000; } >"$T/helper/in/dock.log"
expect helper 1 "left: dock: 3 tiles at once (insert index 56..58, 56 before the suite), more than 2" "$RESTORE"
cp -R "$T/helper" "$T/helper-declared"
expect helper-declared 0 "dock: tiles at once=3 (most allowed 3, insert index 56..58, 56 before the suite)" -- --dock-max 3

echo "the most tiles at once a suite declares"
# n tiles of the app at once, from the probe's index 37
at_once_log() {
  local i
  echo "$HEADER"
  for ((i = 0; i < $1; i++)); do opened "$APP_URL" "1$i.000"; launch "1$i.004" $((37 + i)); done
  for ((i = 0; i < $1; i++)); do remove "2$i.000"; done
}
base declared-4-holds-4
at_once_log 4 >"$T/declared-4-holds-4/in/dock.log"
expect declared-4-holds-4 0 "dock: tiles at once=4 (most allowed 4, insert index 37..40, 37 before the suite)" -- --dock-max 4
base declared-4-holds-5
at_once_log 5 >"$T/declared-4-holds-5/in/dock.log"
expect declared-4-holds-5 1 "left: dock: 5 tiles at once (insert index 37..41, 37 before the suite), more than 4" "$RESTORE" \
  -- --dock-max 4
base undeclared-holds-3
at_once_log 3 >"$T/undeclared-holds-3/in/dock.log"
expect undeclared-holds-3 1 "left: dock: 3 tiles at once (insert index 37..39, 37 before the suite), more than 2"

# the probe's own inserts, named by loginwindow, are not the suite's
base probe-lines
{ echo "$HEADER"; opened "$DOCKPROBE_URL" 00.100; launch 00.104 37; remove 00.900
  opened "$APP_URL" 01.000; launch 01.004 37; remove 05.000
  opened "$DOCKPROBE_URL" 09.000; launch 09.004 39; remove 09.900; } >"$T/probe-lines/in/dock.log"
expect probe-lines 0 "dock: tiles at once=1 (most allowed 2, insert index 37..37, 37 before the suite)"

# step B on mini2 (Dock pid 84089), as `log show` printed it: the probe after
# test-reopen-after-quit.sh, another project's headless browser named 1 ms nearer its insert
# than the probe.  The probe's index is 10, as before the suite
stepb() { cat <<'LOG'
Timestamp               Ty Process[PID:TID]
2026-11-28 01:45:32.214 Df loginwindow[179:9d5] [com.apple.loginwindow.logging:Standard] -[Application _updateInformationInternal] | Got App URL: file:///private/tmp/msp-e2e-01155914491-dockprobe/marspot-dockprobe
2026-11-28 01:45:32.215 Df loginwindow[179:9d5] [com.apple.loginwindow.logging:Standard] -[Application _updateInformationInternal] | Got App URL: file:///Users/u/Library/Caches/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-mac-arm64/chrome-headless-shell
2026-11-28 01:45:32.223 Df loginwindow[179:9d5] [com.apple.loginwindow.logging:Standard] -[Application _updateInformationInternal] | Got App URL: file:///Users/u/Library/Caches/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-mac-arm64/chrome-headless-shell
2026-11-28 01:45:32.225 Df loginwindow[179:9d5] [com.apple.loginwindow.logging:Standard] -[Application _updateInformationInternal] | Got App URL: file:///private/tmp/msp-e2e-01155914491-dockprobe/marspot-dockprobe
2026-11-28 01:45:32.227 Df Dock[84089:1ae541] [com.apple.dock:dock-tiles] Inserting tile: <private> at index: 10 for reason: -[DockBar _handleLaunchNotification:data:]
2026-11-28 01:45:32.228 Df loginwindow[179:9d5] [com.apple.loginwindow.logging:Standard] -[Application _updateInformationInternal] | Got App URL: file:///Users/u/Library/Caches/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-mac-arm64/chrome-headless-shell
2026-11-28 01:45:32.240 Df loginwindow[179:9d5] [com.apple.loginwindow.logging:Standard] -[Application _updateInformationInternal] | Got App URL: file:///private/tmp/msp-e2e-01155914491-dockprobe/marspot-dockprobe
LOG
}
STEPB_EXE=/tmp/msp-e2e-01155914491-dockprobe/marspot-dockprobe
STEPB_REMOVED='2026-11-28 01:45:47.867 Df Dock[84089:1ae541] [com.apple.dock:dock-tiles] Removing tiles identical to: <private> for reason: _emptyDeleteList()'
# the step B lines as the suite's log sees them: the probe's insert is not the suite's, and with
# the index equal before and after, the suite is clean
base step-b
sed -i '' 's/^probe_dock_pid=.*/probe_dock_pid=84089/' "$T/step-b/before"
probe_before step-b 10
probed 10 84089 >"$T/step-b/in/probe-end.txt"
{ stepb; echo "$STEPB_REMOVED"; } >"$T/step-b/in/dock.log"
expect step-b 0 "dock: tiles at once=0 (most allowed 2)" "dock probe: index before=10 after=10"
# an app's tile between the probe's names (both inserts next to a probe name): one of the two is
# the probe's, the other counts as the suite's, whichever it is
base probe-names-around
probe_before probe-names-around 10
probed 10 >"$T/probe-names-around/in/probe-end.txt"
{ echo "$HEADER"; opened "$DOCKPROBE_URL" 02.000; opened /Applications/Marspot.app 02.050; launch 02.100 10
  opened "$DOCKPROBE_URL" 02.340; launch 02.350 11; remove 03.000; } >"$T/probe-names-around/in/dock.log"
expect probe-names-around 0 "dock: tiles at once=1 (most allowed 2, insert index 11..11, 10 before the suite)"
# two inserts in one window with two programs named, next to another job: not passed
base two-in-window
probe_before two-in-window 10
probed 10 >"$T/two-in-window/in/probe-end.txt"
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/two-in-window/in/status.txt"
{ echo "$HEADER"; opened "$DOCKPROBE_URL" 02.000; opened "$APP_URL" 02.001; launch 02.004 10; launch 02.010 11
  remove 03.000; remove 04.000; } >"$T/two-in-window/in/dock.log"
expect two-in-window 2 "(2 insert(s) named for no app)"

# a tile gone during the suite lowers the base: stricter, never laxer
base lower-insert
{ echo "$HEADER"; launch 01.000 35; launch 02.000 36; launch 03.000 37; } >"$T/lower-insert/in/dock.log"
expect lower-insert 1 "left: dock: 3 tiles at once (insert index 35..37, 37 before the suite), more than 2"

# inserts nobody is named for count against the suite alone, and are not passed next to another job
base unnamed-tiles
probe_before unnamed-tiles 10
probed 10 >"$T/unnamed-tiles/in/probe-end.txt"
{ echo "$HEADER"; launch 01.000 10; launch 02.000 11; launch 03.000 12; } >"$T/unnamed-tiles/in/dock.log"
expect unnamed-tiles 1 "left: dock: 3 tiles at once (insert index 10..12, 10 before the suite), more than 2" "$RESTORE"
base unnamed-shared
probe_before unnamed-shared 10
probed 10 >"$T/unnamed-shared/in/probe-end.txt"
{ echo "$HEADER"; launch 01.000 10; launch 02.000 11; launch 03.000 12; } >"$T/unnamed-shared/in/dock.log"
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/unnamed-shared/in/status.txt"
expect unnamed-shared 2 "unread: dock: 3 tiles at once (insert index 10..12, 10 before the suite), more than 2, with another job"
! grep -q '^restore:' "$T/unnamed-shared/result" || { echo "  FAIL  unnamed-shared: asks for a restore it cannot know is needed"; failed=1; }

# a tile line of another Dock process: its indexes count from another start
base other-dock
{ echo "$HEADER"; launch 01.000; echo "2026-11-27 10:00:02.000 Df Dock[900:1] [com.apple.dock:dock-tiles] Inserting tile: <private> at index: 3 for reason: x"; } \
  >"$T/other-dock/in/dock.log"
expect other-dock 2 "unread: dock: tiles at once: a Dock other than 726 logged tiles in the suite (900)"

echo "the declaration in a suite's file"
# shellcheck source=bin/_e2e-suites.sh
source "$ROOT/bin/_e2e-suites.sh"
declared() { # name, expected exit, expected output, then the file's lines
  local name=$1 want=$2 out_want=$3 got=0 out
  shift 3
  printf '#!/usr/bin/env bash\n' >"$T/suite-$name.sh"
  [[ $# -eq 0 ]] || printf '%s\n' "$@" >>"$T/suite-$name.sh"
  echo 'echo run' >>"$T/suite-$name.sh"
  out="$(dock_tiles_max "$T/suite-$name.sh" 2>&1)" || got=$?
  if [[ $got == "$want" && $out == *"$out_want"* ]]; then echo "  ok    $name"; else
    echo "  FAIL  $name: exit $got (wanted $want), printed: $out (wanted: $out_want)"; failed=1; fi
}
declared absent 0 2 "# a suite"
declared four 0 4 "# e2e-dock-tiles-max: 4  app 2 + probe 1 + instance 1" "#"
declared no-reason 2 "malformed" "# e2e-dock-tiles-max: 4"
declared not-a-number 2 "malformed" "# e2e-dock-tiles-max: four  app"
declared zero 2 "malformed" "# e2e-dock-tiles-max: 0  none"
declared no-colon 2 "malformed" "# e2e-dock-tiles-max 3  app"
declared twice 2 "2 lines name e2e-dock-tiles-max" "# e2e-dock-tiles-max: 3  a" "# e2e-dock-tiles-max: 4  b"
# what the suites in this tree declare
for s in test-session-ownership.sh:4 test-one-app-record.sh:3 test-coalition-reclaim.sh:3 test-reseat-bound.sh:3 \
  test-reopen-after-quit.sh:2 test-multi-window.sh:2 test-shell-core.sh:2; do
  n="$(dock_tiles_max "$ROOT/bin/${s%%:*}")" || n="refused"
  if [[ $n == "${s##*:}" ]] && { [[ ${s%%:*} == test-shell-core.sh ]] || grep -q '^# e2e-dock-tiles-max: ' "$ROOT/bin/${s%%:*}"; }; then
    echo "  ok    ${s%%:*} declares $n"
  else echo "  FAIL  ${s%%:*}: $n, wanted ${s##*:} declared"; failed=1; fi
done

echo "the probe's reading of the Dock's log"
EXE=/tmp/msp-e2e-1-dockprobe/marspot-dockprobe
py() { python3 "$ROOT/bin/_dock-probe.py" "$@"; }
reads() { # name, expected exit, expected output, then the command
  local name=$1 want=$2 out_want=$3 got=0 out
  shift 3
  out="$("$@" 2>&1)" || got=$?
  if [[ $got == "$want" && $out == "$out_want" ]]; then echo "  ok    $name"; else
    echo "  FAIL  $name: exit $got (wanted $want), printed: $out (wanted: $out_want)"; failed=1; fi
}
S="$T/stream"
{ echo 'Filtering the log data using "x"'
  opened /Applications/Other.app 01.000; launch 01.004 39
  opened "$DOCKPROBE_URL" 02.000; opened /Applications/Other.app 02.001; launch 02.004 40
  opened "$DOCKPROBE_URL" 03.000; launch 03.004 41
  remove 03.500; } >"$S"
reads ready 0 "" py ready 0 "$S"
# the banner read from the stream's errors when its output has none yet
: >"$S.out"
echo 'Filtering the log data using "x"' >"$S.err"
reads ready-on-errors 0 "" py ready 0 "$S.out" "$S.err"
# the probe named for two inserts: which is its own cannot be told
reads insert-named-twice 5 "" py insert "$S" "$EXE" 726 0
reads removed-after 0 "" py removed "$S" 8 726 0
reads removed-none 1 "" py removed "$S" 9 726 0
{ echo 'Filtering'; opened "$DOCKPROBE_URL" 02.000; launch 02.300 40; } >"$S.late"
reads insert-too-late 1 "" py insert "$S.late" "$EXE" 726 0
# the name logged 5 ms after the insert (mini2, 2026-11-27 22:35:59.996); nothing logged after
# it, so it stands once it held a second
{ echo 'Filtering'; launch 02.000 40; opened "$DOCKPROBE_URL" 02.005; } >"$S.after"
reads insert-named-after 0 $'index=40\nline=2' py insert "$S.after" "$EXE" 726 3
{ echo 'Filtering'; opened "$DOCKPROBE_URL" 02.000; launch 02.004 40; } >"$S.other"
reads insert-other-dock 3 "" py insert "$S.other" "$EXE" 900 0
# another app named nearer the insert than the probe, on either side: it took no tile in the
# lines, the probe is built to take one, and the Dock inserted one
{ echo 'Filtering'; opened /Applications/Other.app 02.000; launch 02.100 40; opened "$DOCKPROBE_URL" 02.110; } >"$S.near"
reads insert-other-before 0 $'index=40\nline=3' py insert "$S.near" "$EXE" 726 3
{ echo 'Filtering'; opened "$DOCKPROBE_URL" 02.000; launch 02.100 40; opened /Applications/Other.app 02.110; } >"$S.near2"
reads insert-other-after 0 $'index=40\nline=3' py insert "$S.near2" "$EXE" 726 3
# the same, with the other app known to take a tile: whose cannot be told, so it is not taken
{ echo 'Filtering'; opened /Applications/Other.app 01.000; launch 01.004 39; remove 01.500
  opened "$DOCKPROBE_URL" 02.000; launch 02.100 40; opened /Applications/Other.app 02.110; } >"$S.near3"
reads insert-other-taker 1 "" py insert "$S.near3" "$EXE" 726 0
# step B: the probe's index is 10, as before the suite
{ echo 'Filtering'; stepb | tail -n +2; echo "$STEPB_REMOVED"; } >"$S.stepb"
reads insert-step-b 0 $'index=10\nline=6' py insert "$S.stepb" "$STEPB_EXE" 84089 0
# two inserts in one window with two programs named: neither is guessed
{ echo 'Filtering'; opened "$DOCKPROBE_URL" 02.000; opened /Applications/Other.app 02.001; launch 02.004 40
  launch 02.010 41; remove 03.000; } >"$S.two"
reads insert-two-in-window 5 "" py insert "$S.two" "$EXE" 726 0
# the probe named before and after an app's insert, its own 0.25 s later: the first is not taken
# as the probe's (it would hide the app's tile), the two are unread
{ echo 'Filtering'; opened "$DOCKPROBE_URL" 02.000; opened /Applications/Marspot.app 02.050; launch 02.100 10
  opened "$DOCKPROBE_URL" 02.340; launch 02.350 11; remove 03.000; } >"$S.around"
reads insert-names-around 5 "" py insert "$S.around" "$EXE" 726 0
# the probe's insert seen but not stood by the time given: its own answer, not "no tile"
{ echo 'Filtering'; opened "$DOCKPROBE_URL" 02.000; launch 02.004 40; } >"$S.unsettled"
reads insert-unsettled 6 "" py insert "$S.unsettled" "$EXE" 726 0.5
# a second insert logged just after the probe's makes it unattributable: the answer waits for
# the window to pass
{ echo 'Filtering'; opened "$DOCKPROBE_URL" 02.000; launch 02.004 40; } >"$S.grow"
( sleep 0.4; { opened /Applications/Other.app 02.005; launch 02.010 41; remove 03.000; } >>"$S.grow" ) &
reads insert-grows 5 "" py insert "$S.grow" "$EXE" 726 2
wait
# a line with no time is answered at once, not after the wait
{ echo 'Filtering'; echo "garbled Got App URL: file://$DOCKPROBE_URL/"; opened "$DOCKPROBE_URL" 02.000; launch 02.004 40; } >"$S.untimed"
t0=$SECONDS
reads insert-untimed 4 "untimed=garbled Got App URL: file://$DOCKPROBE_URL/" py insert "$S.untimed" "$EXE" 726 20
(( SECONDS - t0 < 5 )) || { echo "  FAIL  insert-untimed: answered after $((SECONDS - t0)) s, not at once"; failed=1; }
touch "$S.empty"
reads not-attached 1 "" py ready 0 "$S.empty"

echo "the ownership walk's quit, paced on the probe"
pace() { # name, expected exit, expected last line, then the probe's answers
  local name=$1 want=$2 last=$3 got=0 out
  shift 3
  printf '%s\n' "$@" >"$T/answers.$name"
  : >"$T/asked.$name"
  out="$(cd "$T" && ROOT="$ROOT" SB="$T" bash -c '
    die() { echo "DIE: $*"; exit 1; }
    source "$ROOT/bin/_ownership-quit.sh"
    # called in a command substitution: the count is kept in a file
    dock_probe() { local n a; n=$(( $(wc -l <"'"$T/asked.$name"'") + 1 )); echo >>"'"$T/asked.$name"'"
      a="$(sed -n "${n}p" "'"$T/answers.$name"'")"; [[ -n $a ]] || a="$(tail -1 "'"$T/answers.$name"'")"
      [[ $a == unread=* ]] && { echo "$a"; return 2; }; printf "dock_pid=%s\nindex=%s\n" "${a%%:*}" "${a#*:}"; }
    DOCK_PROBE_EXE=x DOCK_SETTLE_S=1 DOCK_BASE=726:56
    dock_settled && echo settled' 2>&1)" || got=$?
  if [[ $got == "$want" && $(tail -1 <<<"$out") == "$last"* ]]; then echo "  ok    $name"; else
    echo "  FAIL  $name: exit $got (wanted $want), printed: $out"; failed=1; fi
}
pace settled 0 settled 726:57 726:56
pace kept-tile 1 "DIE: the Dock still holds a tile 1s after the quit: the probe at index 57, 56 before the app was opened" 726:57
# below the baseline: another tile went, and waiting cannot tell; dies at the first reading
pace below-base 1 "DIE: unreadable: the Dock probe after a quit at index 55, below 56 before the app was opened" 726:55 726:56
pace restarted 1 "DIE: the Dock restarted during the walk (pid 726, now 900)" 900:56
pace unread 1 "DIE: the Dock probe after a quit: the Dock logged no tile for the probe within 15 s" \
  "unread=the Dock logged no tile for the probe within 15 s"

((failed)) && exit 1
echo "PASS: test-e2e-suite-clean-dock"

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
base shared-equal
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/shared-equal/in/status.txt"
expect shared-equal 0 "window: shared-window" "dock probe: index before=37 after=37"
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
# the probe's tile still at its index when the suite's first tile comes: that one goes one above
base probe-tile-stays
{ echo "$HEADER"; opened "$APP_URL" 00.996; launch 01.000 38; opened "$APP_URL" 01.996; launch 02.000 39
  remove 03.000; remove 04.000; } >"$T/probe-tile-stays/in/dock.log"
expect probe-tile-stays 0 "dock: tiles at once=2 (most allowed 2, insert index 38..39, 37 before the suite)"
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

# each opening after the Dock dropped the last tile (22:58, 177 openings): two at once at most
base paced-reopens
probe_before paced-reopens 56
probed 56 >"$T/paced-reopens/in/probe-end.txt"
{ echo "$HEADER"
  for i in $(seq 10 3 40); do opened "$APP_URL" "$i.000"; launch "$i.004" 57; remove "$((i + 1)).000"; done
  opened "$APP_URL" 45.000; launch 45.004 56; } >"$T/paced-reopens/in/dock.log"
expect paced-reopens 0 "dock: tiles at once=2 (most allowed 2, insert index 56..57, 56 before the suite)"

# the suite's helper registering as a regular app is its subject: one more allowed
base helper
probe_before helper 56
probed 56 >"$T/helper/in/probe-end.txt"
{ echo "$HEADER"; opened "$APP_URL" 01.000; launch 01.004 56; opened "$PROBE_URL" 02.000; launch 02.004 57
  opened "$APP_URL" 03.000; launch 03.004 58; remove 04.000; remove 05.000; } >"$T/helper/in/dock.log"
expect helper 0 "dock: tiles at once=3 (most allowed 3, insert index 56..58, 56 before the suite)"

# the probe's own inserts, named by loginwindow, are not the suite's
base probe-lines
{ echo "$HEADER"; opened "$DOCKPROBE_URL" 00.100; launch 00.104 37; remove 00.900
  opened "$APP_URL" 01.000; launch 01.004 37; remove 05.000
  opened "$DOCKPROBE_URL" 09.000; launch 09.004 39; remove 09.900; } >"$T/probe-lines/in/dock.log"
expect probe-lines 0 "dock: tiles at once=1 (most allowed 2, insert index 37..37, 37 before the suite)"

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
reads insert-named-last 0 $'index=41\nline=8' py insert "$S" "$EXE" 726 0
reads removed-after 0 "" py removed "$S" 8 726 0
reads removed-none 1 "" py removed "$S" 9 726 0
{ echo 'Filtering'; opened "$DOCKPROBE_URL" 02.000; launch 02.300 40; } >"$S.late"
reads insert-too-late 1 "" py insert "$S.late" "$EXE" 726 0
# the name logged 5 ms after the insert (mini2, 2026-11-27 22:35:59.996)
{ echo 'Filtering'; launch 02.000 40; opened "$DOCKPROBE_URL" 02.005; } >"$S.after"
reads insert-named-after 0 $'index=40\nline=2' py insert "$S.after" "$EXE" 726 0
{ echo 'Filtering'; opened "$DOCKPROBE_URL" 02.000; launch 02.004 40; } >"$S.other"
reads insert-other-dock 3 "" py insert "$S.other" "$EXE" 900 0
# the nearest name on either side wins: another app named 100 ms before, the probe 10 ms after
{ echo 'Filtering'; opened /Applications/Other.app 02.000; launch 02.100 40; opened "$DOCKPROBE_URL" 02.110; } >"$S.near"
reads insert-nearest-after 0 $'index=40\nline=3' py insert "$S.near" "$EXE" 726 0
{ echo 'Filtering'; opened "$DOCKPROBE_URL" 02.000; launch 02.100 40; opened /Applications/Other.app 02.110; } >"$S.near2"
reads insert-nearest-other 1 "" py insert "$S.near2" "$EXE" 726 0
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

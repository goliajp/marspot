#!/usr/bin/env bash
# bin/test-e2e-suite-clean-dock.sh — the Dock's part of the decision of
# bin/_e2e-suite-clean.sh over saved inputs: how many tiles a suite had
# at once, from the Dock's insert indexes, and the tiles the Dock lists
# after it that it did not before.  Local, a second; reads no live log
# and launches nothing.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d /tmp/marspot-suite-clean-dock.XXXXXX)"
trap 'rm -rf "${T:?}"' EXIT
failed=0
# shellcheck source=bin/_e2e-suite-clean-fixtures.sh
source "$ROOT/bin/_e2e-suite-clean-fixtures.sh"

# quit and opened again faster than the Dock drops the last tile (mini2, 2026-11-27 22:16:04):
# a tile per opening, index 25 up to 41, the removal lines far fewer and the tiles kept
base quick-reopens
{ echo "$HEADER"
  for i in $(seq 10 26); do opened "$APP_URL" "$i.000"; launch "$i.004" $((i + 15)); done
  remove 27.000; remove 28.000; } >"$T/quick-reopens/in/dock.log"
expect quick-reopens 1 "left: dock: 17 tiles at once (insert index 25..41), more than 2" "restore: dock"

# each opening after the Dock dropped the last tile (22:58, 177 openings): two at once at most
base paced-reopens
{ echo "$HEADER"
  for i in $(seq 10 3 40); do opened "$APP_URL" "$i.000"; launch "$i.004" 57; remove "$((i + 1)).000"; done
  opened "$APP_URL" 45.000; launch 45.004 56; } >"$T/paced-reopens/in/dock.log"
expect paced-reopens 0 "dock: tiles at once=2 (most allowed 2, insert index 56..57)"

# the suite's probe registering as a regular app is its subject: one more allowed
base probe
{ echo "$HEADER"; opened "$APP_URL" 01.000; launch 01.004 56; opened "$PROBE_URL" 02.000; launch 02.004 57
  opened "$APP_URL" 03.000; launch 03.004 58; remove 04.000; remove 05.000; } >"$T/probe/in/dock.log"
expect probe 0 "dock: tiles at once=3 (most allowed 3, insert index 56..58)"

# inserts nobody is named for count against the suite when it is alone, and are shown when not
base unnamed-tiles
{ echo "$HEADER"; launch 01.000 10; launch 02.000 11; launch 03.000 12; } >"$T/unnamed-tiles/in/dock.log"
expect unnamed-tiles 1 "left: dock: 3 tiles at once (insert index 10..12), more than 2" "restore: dock"
base unnamed-shared
{ echo "$HEADER"; launch 01.000 10; launch 02.000 11; launch 03.000 12; } >"$T/unnamed-shared/in/dock.log"
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/unnamed-shared/in/status.txt"
expect unnamed-shared 0 "info: dock: 3 tiles at once (insert index 10..12), more than 2"
! grep -q '^restore:' "$T/unnamed-shared/result" || { echo "  FAIL  unnamed-shared: restores the Dock next to another holder"; failed=1; }

# a tile the Dock kept after its app went: in no app list, only in the Dock's own
base stray-tile
dock_tiles Finder A B C MarspotOwnershipTest >"$T/stray-tile/in/dock-tiles.txt"
expect stray-tile 1 "left: dock tile added: MarspotOwnershipTest AXApplicationDockItem" "dock tiles: before=4 after=5" "restore: dock"
base stray-shared
dock_tiles Finder A B C MarspotOwnershipTest Other >"$T/stray-shared/in/dock-tiles.txt"
printf '%s\n%s\n' "$ME" "$SOMEONE" >"$T/stray-shared/in/status.txt"
expect stray-shared 1 "left: dock tile added: MarspotOwnershipTest" "info: dock tile added: Other"
base stray-other
dock_tiles Finder A B C Other >"$T/stray-other/in/dock-tiles.txt"
expect stray-other 1 "left: dock tile added: Other AXApplicationDockItem"
base tile-gone
dock_tiles Finder A B >"$T/tile-gone/in/dock-tiles.txt"
expect tile-gone 0 "dock: tile gone: C AXApplicationDockItem"

# the Dock's list needs accessibility: without it the tiles are not read, never guessed
base untrusted
echo untrusted >"$T/untrusted/in/dock-tiles.txt"
expect untrusted 2 "unread: dock tiles: this login is not trusted for accessibility"
base tiles-unread
rm "${T:?}/tiles-unread/in/dock-tiles.txt"
expect tiles-unread 2 "unread: dock tiles: the Dock's own list could not be read"
base tiles-not-before
sed -i '' '/^dock_tiles=/d' "$T/tiles-not-before/before"
expect tiles-not-before 2 "unread: dock tiles: not read before the suite"

((failed)) && exit 1
echo "PASS: test-e2e-suite-clean-dock"

#!/usr/bin/env bash
#
# Quit the app the moment its window is back after an update in place
# or after opening it again: the quit is taken, every time.
#
# An image started by an exec can lose its Apple event registration to
# the old image's exit, and until the shell has found that out and
# replaced its image again, a quit from the Dock never reaches it.  The
# shell looks before a window it has to start a core for is on screen,
# so a quit asked at the first frame finds an image that is registered.
#
# bin/test-quit-after-update.sh with QAU_EARLY=1, three times: as it
# comes (QAU_EARLY_ROUNDS, default 10), with every exec losing its
# registration before the first look (the app replaces its image before
# its first frame), and before the last look (by then the quit has made
# the app a resident, which has to replace itself as a resident to be
# opened again).  Exits 0 when all three pass.

set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ROUNDS="${QAU_EARLY_ROUNDS:-10}"

for drop in "" first last; do
  echo "──── ${drop:-no seam}"
  QAU_EARLY=1 QAU_DROP="$drop" QAU_ROUNDS="$ROUNDS" "$ROOT/bin/test-quit-after-update.sh" || exit 1
done

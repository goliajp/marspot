#!/usr/bin/env bash
# Screenshot the live marspot window so the AI session can `Read` and analyze it.
# Requires a running marspot instance — start one with `bin/run.sh` first.
#
# Usage:
#   bin/screencap.sh                    -> writes build/screencap.png
#   bin/screencap.sh path/to/out.png    -> writes to given path
#
# Exit codes:
#   0  success
#   1  no marspot window found
#   2  screencapture failed / output unreadable
#   3  output looks blank (occlusion, no first frame, missing permission)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/build/screencap.png}"
mkdir -p "$(dirname "$OUT")"

# Locate the marspot window via CGWindowList. We pick the first onscreen window
# whose owner name is "marspot" (case-insensitive).
read -r WID GX GY GW GH < <(swift - <<'SWIFT'
import Cocoa
let opts: CGWindowListOption = [.optionOnScreenOnly, .excludeDesktopElements]
let info = CGWindowListCopyWindowInfo(opts, kCGNullWindowID) as? [[String: Any]] ?? []
for w in info {
  let n = (w[kCGWindowOwnerName as String] as? String) ?? ""
  if n.lowercased() == "marspot" {
    let wid = w[kCGWindowNumber as String] ?? 0
    let b = w[kCGWindowBounds as String] as? [String: Any] ?? [:]
    print("\(wid) \(b["X"] ?? 0) \(b["Y"] ?? 0) \(b["Width"] ?? 0) \(b["Height"] ?? 0)")
    exit(0)
  }
}
exit(1)
SWIFT
) || { echo "screencap: no marspot window found — start marspot with bin/run.sh first" >&2; exit 1; }

# -l <wid>: capture by window id (independent of stacking order)
# -o      : drop the window's drop-shadow
# -x      : silent, no shutter sound
screencapture -l "$WID" -o -x "$OUT" || { echo "screencap: screencapture failed" >&2; exit 2; }
[[ -s "$OUT" ]] || { echo "screencap: output missing/empty" >&2; exit 2; }

# Sanity-check: was the content area actually composited? If only window
# chrome got captured (occluded, no first frame, missing Screen Recording
# permission), the content area collapses to ~1 solid color with a tiny
# halo of antialiasing. We use unique-color count: a real terminal frame
# has thousands of distinct RGBA values from font antialiasing; a blank
# capture has only a few dozen.
python3 -W ignore::DeprecationWarning - "$OUT" <<'PY' || exit 3
import sys
from PIL import Image
im = Image.open(sys.argv[1]).convert('RGBA')
content = im.crop((0, 40, im.width, im.height))  # skip title bar
unique = len(set(content.getdata()))
if unique < 200:
    print(f"screencap: content looks blank "
          f"(only {unique} unique colors in content area) — window likely "
          f"occluded or no first frame yet", file=sys.stderr)
    sys.exit(1)
PY

echo "screencap: wrote $OUT"

#!/usr/bin/env bash
# bin/font-snapshot.sh — render the dev-panel Font v5 showcase to PNG.
#
# Useful when you want to inspect the chrome rendering without
# opening the dev panel in the live marspot.  The bitmap goes
# through `MetalRenderer::render_canvas_to_bitmap` so what you see
# is identical (modulo macOS GPU jitter) to what the live dev panel
# paints.
#
# Output: bench/font-rendering/snapshots/font_v5_showcase.png

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "==> rendering Font v5 showcase → PNG"
MARSPOT_FONT_SNAPSHOT=1 cargo nextest run -p marspot --lib \
  --no-capture \
  font_v5_showcase_snapshot \
  2>&1 | grep -E "^\[font v5|PASS|FAIL|Summary"

PNG="$ROOT/bench/font-rendering/snapshots/font_v5_showcase.png"
if [ -f "$PNG" ]; then
  echo "==> opening $PNG"
  open "$PNG"
else
  echo "FAIL: snapshot file missing — did the test skip?"
  exit 1
fi

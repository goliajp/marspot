#!/usr/bin/env bash
# bin/font-bench.sh — font v5 Phase 9 perf characterization.
#
# Runs the cold/warm shape + atlas perf tests; their stderr lines tag
# `[font v5 Phase 9]` so you can grep:
#
#   bin/font-bench.sh | grep 'font v5'
#
# Acceptance bounds (docs/font-rendering-design.md §16 Phase 9):
#   - cold raster < 500 µs / glyph
#   - warm cache lookup < 100 ns / glyph
#   - shape per frame < 1 ms
#
# Tests currently assert LOOSE multiples of those (5 ms / 50 µs)
# because Metal first-context cost + macOS GPU jitter make tighter
# bounds flaky on the dev box.  Treat the printed medians as the
# tracked metric; the asserts catch only gross regression.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "==> running font v5 Phase 9 perf tests"
cargo nextest run -p marspot --lib \
  --no-capture \
  shape_warm_cache_beats_cold_shape \
  raster_warm_cache_beats_cold_raster \
  2>&1 | grep -E "^\[font v5|PASS|FAIL|Summary"

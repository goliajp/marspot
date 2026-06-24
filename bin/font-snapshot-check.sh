#!/usr/bin/env bash
# bin/font-snapshot-check.sh — Phase 9 SSIM regression gate.
#
# Renders each font-v5 snapshot fresh, computes SSIM vs the committed
# PNG baseline under `bench/font-rendering/snapshots/`, and fails when
# any snapshot's SSIM drops below the project-wide 0.98 threshold
# (per docs/font-rendering-design.md §9).
#
# Workflow:
#   bin/font-snapshot.sh            # update / lock baselines (write mode)
#   bin/font-snapshot-check.sh      # CI / pre-merge gate (check mode)
#
# Exits 0 when every snapshot passes;  non-zero on first SSIM drop or
# missing baseline.  Default test runs without env stay fast — the gate
# only fires under `MARSPOT_FONT_SNAPSHOT=check`.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "==> Phase 9 SSIM gate (threshold ≥ 0.98)"
MARSPOT_FONT_SNAPSHOT=check cargo nextest run -p marspot --lib \
  --no-capture \
  font_v5_showcase_snapshot \
  font_v5_mono_grid_snapshot \
  font_v5_box_drawing_snapshot \
  font_v5_subpx_fingerprint_snapshot \
  font_v5_chrome_small_sizes_snapshot \
  2>&1 | grep -E "^\[ssim\]|^\[font v5|PASS|FAIL|Summary|error"

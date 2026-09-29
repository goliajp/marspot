#!/usr/bin/env bash
# Regenerate Unicode property tables.  Re-run when a new Unicode
# version ships.
#
# Outputs:
#   crates/marspot-term/src/emoji_presentation.rs
#   crates/marspot-term/src/unicode_data.rs
#
# Sources (UCD, latest):
#   auxiliary/GraphemeBreakProperty.txt
#   emoji/emoji-data.txt
#   DerivedCoreProperties.txt

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT_UNICODE="$ROOT/crates/marspot-term/src/unicode_data.rs"
OUT_EMOJI="$ROOT/crates/marspot-term/src/emoji_presentation.rs"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

GBP="$TMPDIR/GraphemeBreakProperty.txt"
EMD="$TMPDIR/emoji-data.txt"
DCP="$TMPDIR/DerivedCoreProperties.txt"

echo "==> fetching UCD source files"
curl -fsSL https://www.unicode.org/Public/UCD/latest/ucd/auxiliary/GraphemeBreakProperty.txt -o "$GBP"
curl -fsSL https://www.unicode.org/Public/UCD/latest/ucd/emoji/emoji-data.txt           -o "$EMD"
curl -fsSL https://www.unicode.org/Public/UCD/latest/ucd/DerivedCoreProperties.txt      -o "$DCP"

echo "==> generating Rust modules"
python3 "$ROOT/bin/_regen_unicode_tables.py" \
    --gbp "$GBP" \
    --emd "$EMD" \
    --dcp "$DCP" \
    --out-emoji "$OUT_EMOJI" \
    --out-unicode "$OUT_UNICODE"

echo "==> done"
echo "    $OUT_EMOJI"
echo "    $OUT_UNICODE"

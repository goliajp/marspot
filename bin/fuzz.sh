#!/usr/bin/env bash
# bin/fuzz.sh — run libfuzzer targets.
#
# Default: 60 s smoke run of every target under fuzz/fuzz_targets.
# Long runs (overnight, days) parametrise via env:
#   DURATION_S=3600 bin/fuzz.sh           # one hour each
#   DURATION_S=0    bin/fuzz.sh           # run forever (Ctrl-C)
#   TARGET=parse_vt bin/fuzz.sh           # one target only
#
# Corpus and crash artefacts persist under fuzz/corpus/<target>/ and
# fuzz/artifacts/<target>/ — both gitignored. Bring a crash back into
# version control by extracting the minimised input separately.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DURATION_S="${DURATION_S:-60}"
ONLY="${TARGET:-}"

# macOS ships bash 3.2 → no mapfile/readarray. Plain pipe + read loop.
if [[ -n "$ONLY" ]]; then
  TARGETS_LIST="$ONLY"
else
  TARGETS_LIST="$(find fuzz/fuzz_targets -maxdepth 1 -name '*.rs' -exec basename -s .rs {} \;)"
fi

if [[ -z "$TARGETS_LIST" ]]; then
  echo "no fuzz targets found" >&2; exit 2
fi

# cargo-fuzz needs nightly (-Z sanitizer flags); it also defaults to
# x86_64-apple-darwin on Apple Silicon (cargo-fuzz <=0.13.x), so pin
# the host triple explicitly to avoid an unwanted Rosetta build.
#
# The pin covers what cargo-fuzz *builds*, not what it *is*: a
# cargo-fuzz installed under Rosetta is itself an x86_64 binary, and
# running it launches a translated process.  macOS attributes that to
# the terminal's bundle — every child of a pane is `responsible_path=
# .../Marspot.app/...` — which is how a clean, arm64-only Marspot
# ends up showing "Intel app support will end soon".  Keep the driver
# native; `lipo -archs "$(command -v cargo-fuzz)"` must say arm64.
HOST_TRIPLE="$(rustc -vV | awk '/host:/ {print $2}')"

while IFS= read -r t; do
  [[ -z "$t" ]] && continue
  echo "==> fuzz $t (${DURATION_S}s, $HOST_TRIPLE)"
  if (( DURATION_S > 0 )); then
    cargo +nightly fuzz run "$t" --target "$HOST_TRIPLE" -- -max_total_time="$DURATION_S"
  else
    cargo +nightly fuzz run "$t" --target "$HOST_TRIPLE"
  fi
done <<< "$TARGETS_LIST"

echo "==> fuzz pass"

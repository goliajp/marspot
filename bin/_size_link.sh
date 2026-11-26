#!/usr/bin/env bash
# bin/_size_link.sh — the linker for bin/_size_tables.sh build.
#
# Passes every link through to cc, and gives the link of each binary
# named in MARSPOT_SIZE_MAP_BINS its own `-Wl,-map,<dir>/<binary>.map`.
# Set as the target linker of a plain `cargo build --release`, so the
# maps come from the same feature resolution as the shipped build: a
# per-package `cargo rustc` resolves features for that package alone.
set -euo pipefail

out=""
prev=""
for a in "$@"; do
  [[ $prev == -o ]] && out=$a
  prev=$a
done
# rustc links a bin into deps/<crate_name>-<16 hex>
name="$(basename "$out")"
name="$(sed -E 's/-[0-9a-f]{16}$//; s/_/-/g' <<<"$name")"
for b in ${MARSPOT_SIZE_MAP_BINS:-}; do
  if [[ $b == "$name" ]]; then
    exec cc "$@" "-Wl,-map,${MARSPOT_SIZE_MAP_DIR:?}/$b.map"
  fi
done
exec cc "$@"

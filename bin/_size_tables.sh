#!/usr/bin/env bash
# bin/_size_tables.sh build|tables — the module tables behind a size budget.
#
#   build    the shipped `cargo build --release` of bin/bench.sh, then
#            the same build in its own target dir with bin/_size_link.sh
#            as the linker, so each of the four budgeted binaries is
#            linked with a map (the shipped build is not relinked)
#   tables   sum each map by crate and module into
#            bench/results/size-tables/<binary>.tsv (bin/_size_budget.py
#            raw); refused when a binary differs in size from the
#            shipped one, where that exists
#
# Run through `bin/bench-remote.sh --size-tables`: the tables come back in
# bench/remote-runs/<ts>/size-tables/, and `bin/_size_budget.py set` turns
# one into a committed budget.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"
TD="$(_mars_target_dir)/size-map"
MAPS="$TD/maps"
OUT="$ROOT/bench/results/size-tables"
BINS="$(python3 "$ROOT/bin/_size_budget.py" binaries)"

case "${1:-}" in
  build)
    mkdir -p "$MAPS"
    echo "==> building release (shipped)"
    ( cd "$ROOT" && cargo build --release 2>&1 | tail -3 )
    echo "==> building release with a link map for: $BINS"
    # the linker is in every unit's fingerprint, hence the own target dir;
    # a per-package build would resolve marspot-term's features differently
    ( cd "$ROOT" && CARGO_TARGET_DIR="$TD" MARSPOT_SIZE_MAP_DIR="$MAPS" MARSPOT_SIZE_MAP_BINS="$BINS" \
        CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="$ROOT/bin/_size_link.sh" \
        cargo build --release 2>&1 | tail -3 )
    for bin in $BINS; do
      [[ -s "$MAPS/$bin.map" ]] || { echo "no link map for $bin" >&2; exit 1; }
    done
    ;;
  tables)
    rm -rf "${OUT:?}" && mkdir -p "$OUT"
    for bin in $BINS; do
      size="$(stat -f%z "$TD/release/$bin")"
      shipped="$(marspot_bin "$bin")"
      note=""
      if [[ -f $shipped ]]; then
        if [[ "$(stat -f%z "$shipped")" != "$size" ]]; then
          echo "$bin: the mapped build is $size bytes, the shipped $shipped $(stat -f%z "$shipped"): refusing" >&2
          exit 1
        fi
        note=" (= shipped)"
      fi
      python3 "$ROOT/bin/_size_budget.py" raw "$MAPS/$bin.map" "$TD/release/$bin" "$bin" "$OUT/$bin.tsv" "$ROOT"
      printf "    %-16s %s bytes%s -> %s\n" "$bin" "$size" "$note" "$OUT/$bin.tsv"
    done
    ;;
  *)
    sed -n '2,15p' "$0"; exit 2 ;;
esac

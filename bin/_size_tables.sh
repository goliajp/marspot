#!/usr/bin/env bash
# bin/_size_tables.sh build|tables — the module tables behind a size budget.
#
#   build    link marspot-shell, marspot-core, marspot-session and
#            marspot-peer (release) with `-Wl,-map,<file>` in their own
#            target dir, so the shipped build next to it is not relinked
#   tables   sum each map by crate and module into
#            bench/results/size-tables/<binary>.tsv (bin/_size_budget.py raw)
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

# binary package
TARGETS="marspot-shell marspot
marspot-core marspot
marspot-session marspot-session
marspot-peer marspot-peer"

case "${1:-}" in
  build)
    mkdir -p "$MAPS"
    while read -r bin pkg; do
      echo "==> linking $bin with a map"
      ( cd "$ROOT" && CARGO_TARGET_DIR="$TD" cargo rustc --release -p "$pkg" --bin "$bin" \
          -- -C "link-arg=-Wl,-map,$MAPS/$bin.map" 2>&1 | tail -2 )
    done <<<"$TARGETS"
    ;;
  tables)
    rm -rf "${OUT:?}" && mkdir -p "$OUT"
    while read -r bin _; do
      python3 "$ROOT/bin/_size_budget.py" raw "$MAPS/$bin.map" "$TD/release/$bin" "$bin" "$OUT/$bin.tsv" "$ROOT"
      printf "    %-16s %s bytes -> %s\n" "$bin" "$(stat -f%z "$TD/release/$bin")" "$OUT/$bin.tsv"
    done <<<"$TARGETS"
    ;;
  *)
    sed -n '2,12p' "$0"; exit 2 ;;
esac

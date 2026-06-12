#!/usr/bin/env bash
#
# Build a release tarball the silent-update pipeline can consume.
# Output layout (consumed by `src/updater.rs::extract_binary` via
# `find_named_file`):
#
#   marspot-aarch64-apple-darwin.tar.gz
#   ├── marspot-shelld
#   ├── marspot-shell
#   └── marspot-core
#
# Optional: `--marspot` also includes the legacy single-binary
# `marspot` for backward compat with v1 single-binary tarballs.
#
# Usage:
#   bin/build-release-tarball.sh                 # build all three
#   bin/build-release-tarball.sh --output /tmp/foo.tar.gz
#   bin/build-release-tarball.sh --strip         # `strip` the
#                                                 # binaries first

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="$ROOT/target/release"
OUTPUT="$ROOT/target/release/marspot-aarch64-apple-darwin.tar.gz"
STRIP_BINS=0
INCLUDE_MARSPOT=0

while (( $# )); do
  case "$1" in
    --output)   OUTPUT="$2"; shift 2 ;;
    --strip)    STRIP_BINS=1; shift ;;
    --marspot)  INCLUDE_MARSPOT=1; shift ;;
    -h|--help)
      sed -n '3,18p' "$0"
      exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

bins=(marspot-shelld marspot-shell marspot-core)
(( INCLUDE_MARSPOT )) && bins+=(marspot)

echo "==> building release binaries"
(
  cd "$ROOT"
  cargo build --release "${bins[@]/#/--bin=}" 2>&1 | tail -4
)

STAGE=$(mktemp -d "/tmp/marspot-release.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
for b in "${bins[@]}"; do
  src="$TARGET/$b"
  [[ -x "$src" ]] || { echo "ERROR: $src missing after build"; exit 1; }
  install -m 0755 "$src" "$STAGE/$b"
  if (( STRIP_BINS )); then
    /usr/bin/strip "$STAGE/$b" 2>/dev/null || true
  fi
  # Strip provenance/quarantine xattrs so the binary launches
  # without a Gatekeeper stall — the updater does the same, but
  # belt-and-braces for tarballs that go through other channels.
  xattr -d com.apple.quarantine "$STAGE/$b" 2>/dev/null || true
  xattr -d com.apple.provenance  "$STAGE/$b" 2>/dev/null || true
done

mkdir -p "$(dirname "$OUTPUT")"
tar -czf "$OUTPUT" -C "$STAGE" "${bins[@]}"
echo "==> wrote $OUTPUT ($(stat -f '%z' "$OUTPUT") B)"
shasum -a 256 "$OUTPUT"
echo "Contents:"
tar -tzf "$OUTPUT" | sed 's/^/  /'

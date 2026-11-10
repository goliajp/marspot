#!/usr/bin/env bash
# bin/publish-release.sh — make this version the one installed copies
# update themselves to.
#
#   bin/publish-release.sh                 # publish Cargo.toml's version
#   bin/publish-release.sh --rehearse DIR  # build everything, upload nothing
#
# An installed marspot polls the repository's latest release, and when
# its version is newer than its own it downloads the update tarball,
# checks the detached signature against the key compiled into it, and
# swaps itself.  So publishing is the act of updating every copy out
# there within minutes, and it is done on purpose, by hand, from the
# machine that holds the three things a release needs: the Developer ID
# certificate, the notary profile (see bin/notarize.sh), and the update
# signing key (keys/marspot-update.sec).  Day-to-day builds go to this
# machine alone with bin/install-local.sh and never pass through here.
#
# What goes up:
#   Marspot-<version>.dmg                        what a person installs
#   marspot-aarch64-apple-darwin.tar.gz + .sig   what a copy updates from
#
# The version is the product version in Cargo.toml.  It must be newer
# than the last release: a copy compares it with its own and ignores
# anything that is not.
#
# `--rehearse DIR` leaves the artefacts in DIR with a feed.json in the
# shape of the releases API whose URLs point at those files, which is
# what MARSPOT_UPDATE_FEED takes: bin/test-release-update.sh uses it to
# watch an older copy update itself without anything being published.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

REHEARSE=""
case "${1:-}" in
  --rehearse) REHEARSE="${2:?--rehearse needs a directory}" ;;
  "") ;;
  *) echo "usage: $0 [--rehearse DIR]" >&2; exit 2 ;;
esac

VERSION="$(sed -n '/^\[package\]/,/^\[/p' Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
TAG="v$VERSION"
TAR="marspot-aarch64-apple-darwin.tar.gz"

if [[ -z "$REHEARSE" ]]; then
  [[ -z "$(git status --porcelain)" ]] || { echo "the working tree has uncommitted changes" >&2; exit 1; }
  git fetch -q origin
  [[ "$(git rev-parse HEAD)" == "$(git rev-parse '@{u}')" ]] || { echo "HEAD is not what is pushed" >&2; exit 1; }
  if gh release view "$TAG" >/dev/null 2>&1; then
    echo "$TAG is already released; raise the version in Cargo.toml" >&2; exit 1
  fi
  latest="$(gh release view --json tagName -q .tagName 2>/dev/null || true)"
  if [[ -n "$latest" ]]; then
    newest="$(printf '%s\n%s\n' "${latest#v}" "$VERSION" | sort -V | tail -1)"
    [[ "$newest" == "$VERSION" && "${latest#v}" != "$VERSION" ]] \
      || { echo "$VERSION is not newer than the last release, $latest: no copy would take it" >&2; exit 1; }
  fi
fi

OUT="${REHEARSE:-$ROOT/build/dist}"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

bin/build-dmg.sh "$OUT"
bin/build-release-tarball.sh --strip --codesign --sign --output "$OUT/$TAR"
DMG="$OUT/Marspot-$VERSION.dmg"
test -f "$DMG" && test -f "$OUT/$TAR" && test -f "$OUT/$TAR.sig"

if [[ -n "$REHEARSE" ]]; then
  cat > "$OUT/feed.json" <<JSON
{
  "tag_name": "$TAG",
  "name": "Marspot $VERSION",
  "assets": [
    {"name": "$TAR", "browser_download_url": "file://$OUT/$TAR"},
    {"name": "$TAR.sig", "browser_download_url": "file://$OUT/$TAR.sig"}
  ]
}
JSON
  echo "==> rehearsal: nothing uploaded.  $OUT/feed.json"
  exit 0
fi

gh release create "$TAG" --target "$(git rev-parse HEAD)" --title "Marspot $VERSION" --latest \
  --notes "Signed and notarised. \`Marspot-$VERSION.dmg\` to install by hand; the tarball and its detached signature are what an installed copy updates itself from." \
  "$DMG" "$OUT/$TAR" "$OUT/$TAR.sig"
echo "==> published $TAG"
gh release view "$TAG" --json url -q .url

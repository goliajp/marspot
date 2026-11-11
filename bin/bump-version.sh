#!/usr/bin/env bash
# bin/bump-version.sh — move the product version.
#
#   bin/bump-version.sh minor      # 0.4.0 -> 0.5.0: something a person can see or use is new
#   bin/bump-version.sh patch      # 0.5.0 -> 0.5.1: fixes only
#   bin/bump-version.sh 1.0.0      # an exact version; must be higher than the current one
#
# The product version is the one in Cargo.toml: what the title bar and
# the About panel show, what a disk image is named, and what the
# updater compares.  It moves here and nowhere else.  The three layer
# numbers in version-vector.toml are build numbers and move with every
# change; they are not this.
#
# A build that leaves this machine must carry a version no earlier
# outside build carried: bin/build-dmg.sh refuses to write over a disk
# image of the same version.  So: bump, then build.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

current="$(sed -n '/^\[package\]/,/^\[/p' Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
IFS=. read -r major minor patch <<<"$current"
case "${1:-}" in
  minor) next="$major.$((minor + 1)).0" ;;
  patch) next="$major.$minor.$((patch + 1))" ;;
  [0-9]*.[0-9]*.[0-9]*) next="$1" ;;
  *) echo "usage: $0 minor|patch|X.Y.Z   (now $current)" >&2; exit 2 ;;
esac

highest="$(printf '%s\n%s\n' "$current" "$next" | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)"
if [[ "$next" == "$current" || "$highest" != "$next" ]]; then
  echo "$next is not higher than $current" >&2
  exit 1
fi
if [[ -n "$(git status --porcelain -- Cargo.toml Cargo.lock)" ]]; then
  echo "Cargo.toml or Cargo.lock has uncommitted changes" >&2
  exit 1
fi

# the [package] version, and this package's entry in the lock file
awk -v v="$next" '
  /^\[package\]/ { in_pkg = 1 }
  /^\[/ && !/^\[package\]/ { in_pkg = 0 }
  in_pkg && /^version = "/ && !done { print "version = \"" v "\""; done = 1; next }
  { print }' Cargo.toml > Cargo.toml.new && mv Cargo.toml.new Cargo.toml
awk -v v="$next" '
  /^name = "marspot"$/ { hit = 1; print; next }
  hit && /^version = "/ { print "version = \"" v "\""; hit = 0; next }
  { hit = 0; print }' Cargo.lock > Cargo.lock.new && mv Cargo.lock.new Cargo.lock

git add Cargo.toml Cargo.lock
git commit -q -m "infra: version $next"
echo "$current -> $next"

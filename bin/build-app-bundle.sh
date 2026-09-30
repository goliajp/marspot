#!/usr/bin/env bash
# bin/build-app-bundle.sh — assemble Marspot.app from built binaries.
#
# One builder, two callers: the local install path and the release
# workflow.  They used to disagree, which is how the installed app came
# to report version 0.2.0 for months — the plist was written once, on
# first install, and never touched again.
#
# Assembly only.  Signing and notarisation are the caller's business,
# because a local install wants whatever identity is in the keychain
# and a release wants a Developer ID plus a notarisation round trip.
#
# Usage:
#   bin/build-app-bundle.sh --bin-dir target/release --out /tmp/Marspot.app
#   bin/build-app-bundle.sh --out "$HOME/.local/Marspot.app"   # bin-dir defaults
#
# Options:
#   --bin-dir DIR   where marspot-shell / marspot-core / marspot-session are
#   --out PATH      the .app to create or refresh
#   --version V     override the product version (default: read from Cargo.toml)
#   --scaffold-only write the plist and icon but do NOT place binaries
#
# `--scaffold-only` exists for the local install path, which decides
# whether a silent update is needed by comparing the bundle's binaries
# against the freshly built ones.  Placing the new binaries before that
# comparison would make every install look like a no-op.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

BIN_DIR="$ROOT/target/release"
OUT=""
VERSION=""
SCAFFOLD_ONLY=0

while (( $# )); do
  case "$1" in
    --bin-dir) BIN_DIR="$2"; shift 2 ;;
    --out)     OUT="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --scaffold-only) SCAFFOLD_ONLY=1; shift ;;
    -h|--help) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ -n "$OUT" ]] || { echo "--out is required" >&2; exit 2; }

# The product version has one source.  Reading it here rather than
# taking it on faith is what keeps the bundle from drifting from the
# binary inside it.
if [[ -z "$VERSION" ]]; then
  VERSION="$(sed -n '/^\[package\]/,/^\[/p' "$ROOT/Cargo.toml" \
             | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
fi
[[ -n "$VERSION" ]] || { echo "could not read the product version from Cargo.toml" >&2; exit 1; }

BINARIES=(marspot-shell marspot-core marspot-session)
if (( SCAFFOLD_ONLY == 0 )); then
  for b in "${BINARIES[@]}"; do
    [[ -f "$BIN_DIR/$b" ]] || { echo "missing $BIN_DIR/$b — build first" >&2; exit 1; }
  done
fi

MACOS="$OUT/Contents/MacOS"
RES="$OUT/Contents/Resources"
PLIST="$OUT/Contents/Info.plist"
mkdir -p "$MACOS" "$RES"

# Written in full every time, not patched.  A plist that is created
# once and then only ever amended is how a field goes stale without
# anyone noticing: this one claimed 0.2.0 while the app was on 0.4.0.
cat > "$PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Marspot</string>
  <key>CFBundleDisplayName</key><string>Marspot</string>
  <key>CFBundleIdentifier</key><string>com.goliajp.marspot</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>marspot-shell</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleIconFile</key><string>AppIcon</string>
</dict>
</plist>
PLIST

if [[ -f "$ROOT/assets/AppIcon.icns" ]]; then
  cp "$ROOT/assets/AppIcon.icns" "$RES/AppIcon.icns"
fi

# `install` rather than `cp`: an in-place overwrite of a binary that is
# about to be exec'd keeps the old inode, and macOS kills the process
# for an invalid signature.  A rename gives it a new one.
if (( SCAFFOLD_ONLY == 0 )); then
  for b in "${BINARIES[@]}"; do
    install -m 0755 "$BIN_DIR/$b" "$MACOS/$b"
  done
fi

# macOS caches icons by bundle identifier; touching the root asks
# Finder and the Dock to look again.
touch "$OUT" 2>/dev/null || true

echo "==> $OUT"
if (( SCAFFOLD_ONLY == 1 )); then
  echo "    version $VERSION, scaffold only (binaries left to the caller)"
else
  echo "    version $VERSION, $(ls "$MACOS" | tr '\n' ' ')"
fi

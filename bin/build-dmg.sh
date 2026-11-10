#!/usr/bin/env bash
# bin/build-dmg.sh — a signed, notarised Marspot disk image, built here.
#
#   bin/build-dmg.sh [out-dir]        # default: build/dist
#
# For handing marspot to a Mac that has none of this machine's setup:
# the image opens without a Gatekeeper warning, and the app is dragged
# into Applications.  The release workflow does the same on a tag; this
# is that path run by hand, with the keychain's Developer ID and the
# notary profile bin/notarize.sh describes.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/build/dist}"
PROFILE="${NOTARY_PROFILE:-marspot}"
IDENTITY="${CODESIGN_IDENTITY:-Developer ID Application: GOLIA K.K. (KF79DRC524)}"

echo "==> building release"
( cd "$ROOT" && cargo build --release --workspace --bins 2>&1 | tail -2 )

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
"$ROOT/bin/build-app-bundle.sh" --out "$STAGE/Marspot.app"
"$ROOT/bin/notarize.sh" "$STAGE/Marspot.app"

VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$STAGE/Marspot.app/Contents/Info.plist")"
DMG="$OUT/Marspot-$VERSION.dmg"
mkdir -p "$OUT"
ln -s /Applications "$STAGE/Applications"

echo "==> disk image"
hdiutil create -volname "Marspot $VERSION" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
codesign --force --timestamp --sign "$IDENTITY" "$DMG"
xcrun notarytool submit "$DMG" --keychain-profile "$PROFILE" --wait
xcrun stapler staple "$DMG"

echo "==> checking what Gatekeeper will say"
spctl --assess --type open --context context:primary-signature -v "$DMG"
spctl --assess --type execute -v "$STAGE/Marspot.app"
echo "==> $DMG"

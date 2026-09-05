#!/usr/bin/env bash
# bin/notarize.sh — sign with Hardened Runtime, notarise, staple.
#
# Why this exists: a freshly linked binary's FIRST execution pays a
# Gatekeeper scan (a notarisation round trip + XProtect, serialised
# through one syspolicyd) — ~0.3 s idle, ~1.3 s under load, and
# reportedly tens of seconds on a busy test tier.  Terminal.app (an
# Apple platform binary) and iTerm.app (notarised) log ZERO scans and
# cost 0.00 s.  Everything else pays every time.  Notarisation is the
# only difference left standing after provenance, Hardened Runtime,
# cs.* entitlements, install location, DeveloperTool grants and spawn
# disclaim were each ruled out — see docs/rfc-007-clean-exec-chain.md.
#
# Credentials never appear here.  Store them once:
#
#   xcrun notarytool store-credentials marspot \
#     --apple-id <your-apple-id> --team-id KF79DRC524 \
#     --password <app-specific-password>
#
# The app-specific password is generated at appleid.apple.com →
# Sign-In and Security → App-Specific Passwords.  notarytool does NOT
# accept a primary Apple ID password.
set -euo pipefail

APP="${1:-$HOME/.local/Marspot.app}"
PROFILE="${NOTARY_PROFILE:-marspot}"
IDENTITY="${CODESIGN_IDENTITY:-Developer ID Application: GOLIA K.K. (KF79DRC524)}"

[[ -d "$APP" ]] || { echo "no such app bundle: $APP" >&2; exit 1; }

if ! xcrun notarytool history --keychain-profile "$PROFILE" >/dev/null 2>&1; then
  cat >&2 <<MSG
==> no usable notary profile "$PROFILE".  Store it once with:

    xcrun notarytool store-credentials $PROFILE \\
      --apple-id <apple-id> --team-id KF79DRC524 \\
      --password <app-specific-password>

MSG
  exit 1
fi

echo "==> signing $APP (Hardened Runtime + timestamp)"
# Inner executables first, bundle last: a signature over a bundle
# covers what is inside it, so re-signing a nested binary afterwards
# invalidates the outer one.
while IFS= read -r bin; do
  echo "    $(basename "$bin")"
  codesign -f -s "$IDENTITY" --options runtime --timestamp "$bin"
done < <(find "$APP/Contents/MacOS" -type f -perm -u+x)
codesign -f -s "$IDENTITY" --options runtime --timestamp "$APP"
codesign -v --deep --strict "$APP"

ZIP="$(mktemp -d)/$(basename "$APP" .app).zip"
echo "==> submitting for notarisation (this waits for Apple)"
ditto -c -k --keepParent "$APP" "$ZIP"
xcrun notarytool submit "$ZIP" --keychain-profile "$PROFILE" --wait

echo "==> stapling"
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"

echo "==> done"
codesign -dv "$APP" 2>&1 | grep -E "flags|TeamIdentifier" | sed 's/^/    /'
echo "    stapled: yes"
echo
echo "Verify the tax is actually gone — the timing alone moves 10x with"
echo "queue depth and has misled this before.  Count performScan too:"
echo "    log show --last 1m --predicate 'process == \"syspolicyd\"' \\"
echo "      --style compact | grep -c performScan"

#!/usr/bin/env bash
# bin/install-remote.sh — install marspot on another Mac, signed.
#
#   bin/install-remote.sh [host]        # default: mini
#
# Why this exists rather than running install-local.sh over ssh: an ssh
# session cannot reach the login keychain, so `codesign` there fails
# with errSecInternalComponent even though `find-identity` lists the
# certificate.  install-local.sh used to warn and carry on, which
# installed adhoc binaries — those stall 30-60 s in amfid on first run
# and fail the code requirement behind the Developer Tools grant, so
# the remote marspot silently loses its Gatekeeper exemption.
#
# So the signing happens HERE, where the keychain is unlocked, and a
# fully signed bundle is shipped.  The private key never leaves this
# machine and the remote host needs no certificate of its own.
set -euo pipefail

HOST="${1:-mini}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="$ROOT/target/release"
STAGE="/tmp/marspot-remote-$HOST.app"
REMOTE_APP="\$HOME/.local/Marspot.app"

SIGN_ID="${MARSPOT_SIGN_ID:-}"
[[ -n "$SIGN_ID" ]] || SIGN_ID=$(security find-identity -v -p codesigning 2>/dev/null \
  | awk -F'"' '/Developer ID Application/{print $2; exit}')
if [[ -z "$SIGN_ID" ]]; then
  echo "no 'Developer ID Application' certificate here — nothing to sign with" >&2
  exit 1
fi

echo "==> building release"
( cd "$ROOT" && cargo build --release 2>&1 | tail -2 )

echo "==> assembling + signing bundle for $HOST"
rm -rf "$STAGE"
mkdir -p "$STAGE/Contents/MacOS"
cat > "$STAGE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>marspot-shell</string>
<key>CFBundleIdentifier</key><string>com.marspot.dev</string>
<key>CFBundleName</key><string>Marspot</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
for b in marspot-shell marspot-core marspot-session; do
  install -m 0755 "$TARGET/$b" "$STAGE/Contents/MacOS/$b"
  xattr -c "$STAGE/Contents/MacOS/$b" 2>/dev/null || true
  codesign -f -s "$SIGN_ID" --timestamp=none "$STAGE/Contents/MacOS/$b" >/dev/null
done
# The bundle last: its signature covers everything inside it, so
# re-signing a nested binary afterwards would invalidate it.
codesign -f -s "$SIGN_ID" --timestamp=none "$STAGE" >/dev/null
codesign -v --deep --strict "$STAGE"
# Capture, then test.  `... | grep -q` looks equivalent and is not:
# grep exits at the first match, codesign takes SIGPIPE writing into a
# closed pipe, and under `set -o pipefail` the whole pipeline reads as
# a failure — so a perfectly signed binary gets rejected.
for b in marspot-shell marspot-core marspot-session; do
  info=$(codesign -dv "$STAGE/Contents/MacOS/$b" 2>&1 || true)
  case "$info" in
    *TeamIdentifier=*) ;;
    *) echo "$b lost its TeamIdentifier — refusing to ship it" >&2; exit 1 ;;
  esac
done
echo "    signed: $(codesign -dv "$STAGE" 2>&1 | grep -o 'TeamIdentifier=[A-Z0-9]*')"
echo "    version: $(MARSPOT_NO_REDIRECT=1 "$STAGE/Contents/MacOS/marspot-shell" --version 2>&1 | head -1)"

# Replacing a bundle binary under a LIVE process gets that process
# killed by AMFI once the on-disk CDHash stops matching, so the remote
# app has to be down first.  Ask rather than decide: it may have live
# sessions in it.
if ssh "$HOST" 'pgrep -x marspot-shell >/dev/null'; then
  if [[ -z "${MARSPOT_REMOTE_FORCE:-}" ]]; then
    echo "" >&2
    echo "==> marspot is RUNNING on $HOST." >&2
    echo "    Its bundle cannot be replaced underneath it (AMFI kills the" >&2
    echo "    process when the on-disk CDHash changes).  Quit it there," >&2
    echo "    or re-run with MARSPOT_REMOTE_FORCE=1 to stop it from here." >&2
    exit 1
  fi
  echo "==> stopping marspot on $HOST (MARSPOT_REMOTE_FORCE=1)"
  ssh "$HOST" 'pkill -x marspot-shell 2>/dev/null; sleep 2' || true
fi

echo "==> shipping to $HOST:~/.local/Marspot.app"
ssh "$HOST" "mkdir -p \$HOME/.local"
rsync -a --delete "$STAGE/" "$HOST:.local/Marspot.app/"
ssh "$HOST" "codesign -v --deep --strict $REMOTE_APP && echo '    remote signature verifies'"
ssh "$HOST" "/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister -f $REMOTE_APP" >/dev/null 2>&1 || true

echo "==> launching on $HOST"
ssh "$HOST" "open -n $REMOTE_APP" || true
sleep 5
ssh "$HOST" 'pgrep -x marspot-shell >/dev/null && echo "    running (pid $(pgrep -x marspot-shell | head -1))" || echo "    did NOT start — check the host" >&2'
rm -rf "$STAGE"
echo "==> done"
echo "    If this host has never been granted Developer Tools, add it once:"
echo "      System Settings → Privacy & Security → Developer Tools → + → Marspot.app"

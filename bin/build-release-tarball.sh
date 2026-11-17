#!/usr/bin/env bash
#
# Build a release tarball the silent-update pipeline can consume.
# Output layout (consumed by `marspot-term`'s `updater::extract_binary` via
# `find_named_file`):
#
#   marspot-aarch64-apple-darwin.tar.gz
#   ├── marspot-shell
#   ├── marspot-core
#   └── marspot-session   (per-pane L3 engine — staged + promoted by
#                          core so silent updates carry the session
#                          binary in lockstep with core)
#
# Optional: `--marspot` also includes the legacy single-binary
# `marspot` for backward compat with v1 single-binary tarballs.
#
# Usage:
#   bin/build-release-tarball.sh                 # build all three
#   bin/build-release-tarball.sh --output /tmp/foo.tar.gz
#   bin/build-release-tarball.sh --strip         # `strip` the
#                                                 # binaries first
#   bin/build-release-tarball.sh --sign [key]    # also write
#                                                 # <output>.sig (ECDSA
#                                                 # P-256 / SHA-256);
#                                                 # key defaults to
#                                                 # keys/marspot-update.sec
#   bin/build-release-tarball.sh --codesign       # Developer ID +
#                                                 # Hardened Runtime on
#                                                 # each binary, then
#                                                 # notarised: what an
#                                                 # update puts in place
#                                                 # is the same kind of
#                                                 # binary the disk
#                                                 # image carries

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="$ROOT/target/release"
OUTPUT="$ROOT/target/release/marspot-aarch64-apple-darwin.tar.gz"
STRIP_BINS=0
INCLUDE_MARSPOT=0
SIGN=0
SIGN_KEY="$ROOT/keys/marspot-update.sec"
CODESIGN=0
IDENTITY="${CODESIGN_IDENTITY:-Developer ID Application: GOLIA K.K. (KF79DRC524)}"
NOTARY_PROFILE="${NOTARY_PROFILE:-marspot}"

while (( $# )); do
  case "$1" in
    --output)   OUTPUT="$2"; shift 2 ;;
    --strip)    STRIP_BINS=1; shift ;;
    --marspot)  INCLUDE_MARSPOT=1; shift ;;
    --codesign) CODESIGN=1; shift ;;
    --sign)
      SIGN=1; shift
      # Optional key-path argument (anything not starting with -).
      if (( $# )) && [[ "$1" != -* ]]; then SIGN_KEY="$1"; shift; fi
      ;;
    -h|--help)
      sed -n '3,24p' "$0"
      exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

# `marspot-shelld` used to lead this list.  RFC-003 retired L4 and
# deleted the bin target, but this line kept naming it — so `cargo build
# --bin=marspot-shelld` failed and took every caller down with it.  That
# silently disabled all three `--real` suites (real / negative /
# adversarial update), each of which builds a tarball before it can test
# anything, from 2026-06-17 until 2026-07-29.
bins=(marspot-shell marspot-core marspot-session)
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

# An updated binary runs from the state directory, outside the bundle.
# Left with the linker's ad-hoc signature it is a different program to
# the system than the one the disk image installed: its first run is
# scanned, and what was granted to the app is not granted to it.  So it
# gets the bundle's identity, and Apple is told about it.  A bare
# executable cannot carry a stapled ticket; the notary service keeps
# one, and the system asks for it.
if (( CODESIGN )); then
  for b in "${bins[@]}"; do
    codesign -f -s "$IDENTITY" --options runtime --timestamp \
      --entitlements "$ROOT/assets/Marspot.entitlements" "$STAGE/$b"
  done
  ( cd "$STAGE" && ditto -c -k . "$STAGE.zip" )
  echo "==> notarising the update binaries (this waits for Apple)"
  verdict="$(xcrun notarytool submit "$STAGE.zip" --keychain-profile "$NOTARY_PROFILE" --wait 2>&1)"
  rm -f "$STAGE.zip"
  [[ "$verdict" == *"status: Accepted"* ]] || { echo "$verdict" | tail -8 >&2; echo "ERROR: not notarised" >&2; exit 1; }
  for b in "${bins[@]}"; do codesign -v --strict "$STAGE/$b"; done
fi

mkdir -p "$(dirname "$OUTPUT")"
tar -czf "$OUTPUT" -C "$STAGE" "${bins[@]}"
echo "==> wrote $OUTPUT ($(stat -f '%z' "$OUTPUT") B)"
shasum -a 256 "$OUTPUT"
echo "Contents:"
tar -tzf "$OUTPUT" | sed 's/^/  /'

if (( SIGN )); then
  if [[ ! -f "$SIGN_KEY" ]]; then
    echo "ERROR: signing key not found at $SIGN_KEY" >&2
    echo "       (generate: openssl ecparam -genkey -name prime256v1 -noout -out $SIGN_KEY)" >&2
    exit 1
  fi
  /usr/bin/openssl dgst -sha256 -sign "$SIGN_KEY" -out "$OUTPUT.sig" "$OUTPUT"
  # Self-check against the checked-in public key — a tarball signed
  # with a key the shipped updater can't verify is worse than no sig.
  if ! /usr/bin/openssl dgst -sha256 -verify "$ROOT/keys/marspot-update.pub" \
       -signature "$OUTPUT.sig" "$OUTPUT" >/dev/null 2>&1; then
    echo "ERROR: $OUTPUT.sig does not verify against keys/marspot-update.pub" >&2
    echo "       — wrong signing key?  Updater would reject this release." >&2
    exit 1
  fi
  echo "==> wrote $OUTPUT.sig (verified against keys/marspot-update.pub)"
fi

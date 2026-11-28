#!/bin/sh
# The stand-in's installer, for a test that must not fetch the real
# one: run as marspot runs codex's own (CODEX_HOME set, nothing to ask),
# it lays out the release the app-server package has selected the way
# codex's installer lays out the latest, selects it, and puts the
# command in ~/.local/bin.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
home="${CODEX_HOME:?}"
rel="$(basename "$(readlink "$home/packages/app-server-daemon/current")")"
rel="${rel%%-*}"
[ -n "$rel" ] || { echo "no selected app-server release under $home" >&2; exit 1; }
pkg="$home/packages/standalone"
dest="$pkg/releases/$rel-aarch64-apple-darwin/bin"
mkdir -p "$dest"
cc -O1 -DPYTHON="\"$(command -v python3)\"" -DSTANDIN="\"$here/standin.py\"" -o "$dest/codex" "$here/launcher.c"
ln -sfn "$pkg/releases/$rel-aarch64-apple-darwin" "$pkg/next"
mv -h "$pkg/next" "$pkg/current"
mkdir -p "$HOME/.local/bin"
ln -sfn "$pkg/current/bin/codex" "$HOME/.local/bin/codex"
echo "installed codex $rel (stand-in)"

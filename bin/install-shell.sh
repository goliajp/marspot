#!/usr/bin/env bash
# bin/install-shell.sh — install the marspot-shell + marspot-core pair
# into ~/.local/Marspot.app so Spotlight / Dock / `open` see the
# supervisor as the app's primary entry point.
#
# Drops:
#   ~/.local/Marspot.app/Contents/MacOS/marspot-shell
#   ~/.local/Marspot.app/Contents/MacOS/marspot-core
#
# Rewrites Info.plist so `CFBundleExecutable` points at
# `marspot-shell`; the existing single-binary `marspot` is left in
# place for compatibility with anyone launching it directly.
#
# Usage:
#   bin/install-shell.sh              # install both binaries
#   bin/install-shell.sh --status     # show what's installed
#   bin/install-shell.sh --uninstall  # remove shell+core; revert
#                                     # CFBundleExecutable to marspot
#
# Idempotent.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$HOME/.local/Marspot.app"
MACOS="$APP/Contents/MacOS"
PLIST="$APP/Contents/Info.plist"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"

cmd_status() {
  echo "App bundle:    $APP"
  if [[ -d "$APP" ]]; then
    echo "  Info.plist exec: $(/usr/libexec/PlistBuddy -c 'Print CFBundleExecutable' "$PLIST" 2>/dev/null || echo missing)"
    for b in marspot marspot-shell marspot-core marspot-shelld marspot-bootstrap; do
      if [[ -x "$MACOS/$b" ]]; then
        sz=$(stat -f '%z' "$MACOS/$b")
        echo "  $b: $sz B"
      else
        echo "  $b: (absent)"
      fi
    done
  else
    echo "  (not installed)"
  fi
}

cmd_install() {
  if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" ]]; then
    echo "==> building release …"
    ( cd "$ROOT" && cargo build --release --bin marspot-shell --bin marspot-core 2>&1 | tail -3 )
  fi
  if [[ ! -d "$APP" ]]; then
    echo "ERROR: $APP doesn't exist.  Install marspot first (./bin/run.sh runs from target/; bin/install.sh wires the bundle)."
    exit 1
  fi
  install -m 0755 "$SHELL_BIN" "$MACOS/marspot-shell"
  install -m 0755 "$CORE_BIN"  "$MACOS/marspot-core"
  # Repoint the bundle's primary executable so Finder / Dock / open
  # all start the supervisor.  We back the old value up into a
  # custom key so --uninstall can restore it.
  prev=$(/usr/libexec/PlistBuddy -c 'Print CFBundleExecutable' "$PLIST" 2>/dev/null || echo marspot)
  if [[ "$prev" != "marspot-shell" ]]; then
    /usr/libexec/PlistBuddy -c "Add :MarspotPreviousExecutable string $prev" "$PLIST" 2>/dev/null \
      || /usr/libexec/PlistBuddy -c "Set :MarspotPreviousExecutable $prev" "$PLIST"
    /usr/libexec/PlistBuddy -c "Set :CFBundleExecutable marspot-shell" "$PLIST"
  fi
  # Nudge LaunchServices so Finder picks up the new executable
  # without requiring a logout / reboot.
  /System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister \
    -f "$APP" >/dev/null 2>&1 || true
  echo "==> installed:"
  cmd_status
  echo "Launch with: open '$APP'"
}

cmd_uninstall() {
  if [[ ! -d "$APP" ]]; then
    echo "no app at $APP — nothing to do"
    return 0
  fi
  rm -f "$MACOS/marspot-shell" "$MACOS/marspot-core"
  # Restore the previous CFBundleExecutable if we backed one up.
  if /usr/libexec/PlistBuddy -c 'Print :MarspotPreviousExecutable' "$PLIST" >/dev/null 2>&1; then
    prev=$(/usr/libexec/PlistBuddy -c 'Print :MarspotPreviousExecutable' "$PLIST")
    /usr/libexec/PlistBuddy -c "Set :CFBundleExecutable $prev" "$PLIST"
    /usr/libexec/PlistBuddy -c 'Delete :MarspotPreviousExecutable' "$PLIST"
  fi
  echo "==> uninstalled.  Bundle executable: $(/usr/libexec/PlistBuddy -c 'Print CFBundleExecutable' "$PLIST")"
}

case "${1:-}" in
  --status)    cmd_status ;;
  --uninstall) cmd_uninstall ;;
  "")          cmd_install ;;
  *)
    echo "usage: $0 [--status|--uninstall]" >&2
    exit 1
    ;;
esac

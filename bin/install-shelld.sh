#!/usr/bin/env bash
# bin/install-shelld.sh — install marspot-shelld as a LaunchAgent.
#
# Writes ~/Library/LaunchAgents/com.marspot.shelld.plist that points
# at the binary inside ~/.local/Marspot.app and bootstraps it via
# launchctl so it starts now and on every login.
#
# Usage:
#   bin/install-shelld.sh                 # install + start
#   bin/install-shelld.sh --uninstall     # stop + remove plist
#   bin/install-shelld.sh --status        # print runtime status
#
# Idempotent: re-running is a no-op apart from refreshing the plist
# content (so a binary path change propagates next time).

set -euo pipefail

LABEL="com.marspot.shelld"
PLIST="$HOME/Library/LaunchAgents/${LABEL}.plist"
BIN="$HOME/.local/Marspot.app/Contents/MacOS/marspot-shelld"
LOG_DIR="$HOME/Library/Logs/marspot"
LOG_OUT="$LOG_DIR/shelld.log"
LOG_ERR="$LOG_DIR/shelld.err"

case "${1:-}" in
  --uninstall)
    if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
      launchctl bootout "gui/$(id -u)/$LABEL" 2>&1 || true
    fi
    rm -f "$PLIST"
    echo "uninstalled."
    exit 0
    ;;
  --status)
    echo "plist: $PLIST"
    [[ -f "$PLIST" ]] && echo "  exists" || echo "  MISSING"
    echo "launchctl:"
    launchctl print "gui/$(id -u)/$LABEL" 2>&1 | head -20 || true
    echo "socket:"
    ls -la "$HOME/Library/Caches/marspot/shelld.sock" 2>&1 || true
    exit 0
    ;;
  ""|--install)
    ;;
  *)
    echo "unknown arg: $1" >&2; exit 2 ;;
esac

if [[ ! -x "$BIN" ]]; then
  echo "error: binary not found at $BIN" >&2
  echo "       build with: cargo build --release --bin marspot-shelld" >&2
  echo "       and install into ~/.local/Marspot.app/Contents/MacOS/" >&2
  exit 1
fi

mkdir -p "$LOG_DIR"
mkdir -p "$(dirname "$PLIST")"

cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>${BIN}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>${LOG_OUT}</string>
  <key>StandardErrorPath</key>
  <string>${LOG_ERR}</string>
  <key>ThrottleInterval</key>
  <integer>5</integer>
  <key>ProcessType</key>
  <string>Interactive</string>
</dict>
</plist>
EOF

# Bootout first (idempotent) so a content change to the plist
# actually takes effect.  Then bootstrap.
if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
  launchctl bootout "gui/$(id -u)/$LABEL" 2>&1 || true
  # Give launchd a moment to fully tear the previous instance down
  # before we start a new one.
  sleep 0.5
fi
launchctl bootstrap "gui/$(id -u)" "$PLIST"

# Verify
sleep 1
if launchctl print "gui/$(id -u)/$LABEL" 2>&1 | grep -q "state = running"; then
  echo "shelld running, socket at $HOME/Library/Caches/marspot/shelld.sock"
else
  echo "warning: shelld doesn't show running state — check $LOG_ERR" >&2
  launchctl print "gui/$(id -u)/$LABEL" 2>&1 | head -30
  exit 1
fi

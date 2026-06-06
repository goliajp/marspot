#!/usr/bin/env bash
# bin/install-bench-launchagent.sh — install a per-user LaunchAgent on
# the bench host so cross-terminal measurement can be ssh-triggered
# without Screen Sharing each time.
#
# Architecture: ~/.marspot-bench-trigger/ is a watched directory; touching
# refresh-others.req inside it fires receiver.sh, which lives in the
# user's GUI Aqua session and can therefore dispatch AppleEvents to
# iTerm/Warp/Terminal and launch Ghostty. Result lands at
# ~/.marspot-bench-trigger/result.json with a done sentinel.
#
# Why a LaunchAgent and not ssh + asuser: macOS 26.5 TCC + SIP scope
# AppleEvent Automation to (osascript, target-app) pairs evaluated
# against the responsible process. ssh-spawned osascript has launchd
# (or a non-aqua bash) as its responsible process and TCC prompts
# cannot be answered in that context — they hang. A LaunchAgent
# launched into gui/<uid> at login is treated as a user-aqua process
# whose AE dispatch inherits user-granted TCC scope.
#
# Run this ONCE on the bench host (in a Screen Sharing session — the
# install is non-interactive but the *first* measurement run will
# raise TCC consent dialogs that only a console user can answer).

set -euo pipefail

TRIG_DIR="$HOME/.marspot-bench-trigger"
PLIST_DIR="$HOME/Library/LaunchAgents"
PLIST="$PLIST_DIR/com.marspot.bench-trigger.plist"
LABEL="com.marspot.bench-trigger"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

mkdir -p "$TRIG_DIR" "$PLIST_DIR"

# ---- receiver.sh -----------------------------------------------------
# Watches for refresh-others.req. When present, removes the trigger
# (so the WatchPaths event doesn't loop), runs _remote-measure-
# others-mini.sh inside the LaunchAgent's user aqua session (no
# SSH_CONNECTION, so the script runs in console mode — all four
# terminals get driven, not just Ghostty), writes result.json and a
# done sentinel.
cat > "$TRIG_DIR/receiver.sh" <<'RCV'
#!/bin/bash
# Launched by com.marspot.bench-trigger LaunchAgent on changes to
# ~/.marspot-bench-trigger/. Runs in the user's GUI Aqua session so
# osascript dispatches and Ghostty NSApp init both work.
set -uo pipefail
TRIG_DIR="$HOME/.marspot-bench-trigger"
LOCK_D="$TRIG_DIR/.lock.d"
REQ="$TRIG_DIR/refresh-others.req"
LOG="$TRIG_DIR/receiver.log"

# Append-only log, all output (stdout + stderr).
exec >> "$LOG" 2>&1
echo "==> fired $(/bin/date -u +%FT%TZ)"

# Only act on the refresh-others trigger; ignore other dir activity
# (receiver writes its own outputs into TRIG_DIR which would otherwise
# cause re-fires).
[[ -f "$REQ" ]] || { echo "  no req — skip"; exit 0; }

# mkdir-lock — macOS bash 3.2 has no flock(1).
if ! mkdir "$LOCK_D" 2>/dev/null; then
  if [[ -f "$LOCK_D/pid" ]] && kill -0 "$(cat "$LOCK_D/pid" 2>/dev/null)" 2>/dev/null; then
    echo "  already running pid $(cat "$LOCK_D/pid")"
    exit 0
  fi
  rm -rf "$LOCK_D" && mkdir "$LOCK_D"
fi
echo "$$" > "$LOCK_D/pid"
trap 'rm -rf "$LOCK_D"' EXIT INT TERM

# Consume the trigger atomically so a second touch arriving during
# the run isn't lost (re-fires after our exit will see no req file).
rm -f "$REQ"
# Clear stale outputs so the requester can poll for 'done' un-
# ambiguously.
rm -f "$TRIG_DIR/result.json" "$TRIG_DIR/err.log" "$TRIG_DIR/done"

# Critical: LaunchAgent inherits no SSH_CONNECTION; this also makes
# _remote-measure-others-mini.sh fall into console mode where iTerm,
# Warp, Terminal AND Ghostty all get driven.
unset SSH_CONNECTION || true

cd "$HOME/bench-marspot"
if bash bin/_remote-measure-others-mini.sh > "$TRIG_DIR/result.json" 2> "$TRIG_DIR/err.log"; then
  echo "  measure OK"
else
  rc=$?
  echo "  measure FAILED rc=$rc"
fi
/usr/bin/touch "$TRIG_DIR/done"
echo "==> done $(/bin/date -u +%FT%TZ)"
RCV
chmod +x "$TRIG_DIR/receiver.sh"

# ---- LaunchAgent plist ----------------------------------------------
# WatchPaths on the whole dir fires on any change in TRIG_DIR. The
# receiver filters down to the refresh-others.req trigger and ignores
# its own output writes (which would otherwise re-fire). launchd
# auto-throttles so the loop converges instead of busy-firing.
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>$TRIG_DIR/receiver.sh</string>
  </array>
  <key>WatchPaths</key>
  <array>
    <string>$TRIG_DIR</string>
  </array>
  <key>RunAtLoad</key>
  <false/>
  <key>StandardOutPath</key>
  <string>$TRIG_DIR/launchd.out</string>
  <key>StandardErrorPath</key>
  <string>$TRIG_DIR/launchd.err</string>
</dict>
</plist>
EOF

# ---- bootstrap into the user's GUI domain ---------------------------
uid="$(id -u)"
# Unload any prior instance idempotently. bootout reports an error
# when nothing is loaded — that's fine.
launchctl bootout "gui/$uid" "$PLIST" 2>/dev/null || true
launchctl bootstrap "gui/$uid" "$PLIST"
launchctl enable "gui/$uid/$LABEL"

echo "==> installed: $LABEL → gui/$uid"
echo "    receiver:  $TRIG_DIR/receiver.sh"
echo "    plist:     $PLIST"
echo "    trigger:   touch $TRIG_DIR/refresh-others.req"
echo "    result:    $TRIG_DIR/result.json (after $TRIG_DIR/done appears)"

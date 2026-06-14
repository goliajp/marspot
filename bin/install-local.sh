#!/usr/bin/env bash
#
# install-local.sh — install / update the marspot you actually USE.
#
# This is the production path: the app lives in ~/.local/Marspot.app as
# real binary copies (no symlinks into target/), runs from the default
# state dir (~/Library/Caches/marspot), and its shelld is a LaunchAgent.
# Dev work (bin/run.sh, bin/test-*.sh) runs in a separate MARSPOT_STATE_DIR
# sandbox with its own shelld, so building / testing / killing processes
# never disturbs this instance.
#
# Run it after you've made changes and want them in your live terminal:
#
#   bin/install-local.sh            # build, install, silent-update the
#                                   #   running app (window + sessions survive)
#   bin/install-local.sh --with-shelld   # also update the daemon
#                                        #   (KILLS all sessions — asks first)
#   bin/install-local.sh --status   # what's installed + running
#   bin/install-local.sh --no-build # install the existing target/release
#
# How the silent update lands: changed shell/core binaries are staged
# into the app's binaries/pending/ slots and the running supervisor is
# SIGUSR1'd — it promotes + execs (shell) / respawns (core) in place,
# 30 s probation with auto-rollback.  Same machinery a real GitHub
# release would drive; running it on every local change keeps that path
# exercised.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="$ROOT/target/release"
APP="$HOME/.local/Marspot.app"
MACOS="$APP/Contents/MacOS"
PLIST="$APP/Contents/Info.plist"
# Production state dir — the default; never set MARSPOT_STATE_DIR here.
TREE="$HOME/Library/Caches/marspot/binaries"
SUP_LOG="$HOME/Library/Logs/Marspot/supervisor.log"
PROD_PID_FILE="$HOME/Library/Caches/marspot/shell.pid"

# Is the installed GUI shell actually running?  Uses its pid file
# (written on startup), NOT a `pgrep marspot-shell` — that substring
# also matches `marspot-shelld` and would report a phantom shell.
prod_shell_running() {
  local p
  p=$(cat "$PROD_PID_FILE" 2>/dev/null) || return 1
  [[ -n "$p" ]] && kill -0 "$p" 2>/dev/null
}

BUILD=1
WITH_SHELLD=0
MODE=install
for arg in "$@"; do
  case "$arg" in
    --no-build)    BUILD=0 ;;
    --with-shelld) WITH_SHELLD=1 ;;
    --status)      MODE=status ;;
    -h|--help)     sed -n '2,27p' "$0"; exit 0 ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done

bundle_bin() { echo "$MACOS/$1"; }

cmd_status() {
  echo "App bundle:    $APP"
  if [[ -d "$APP" ]]; then
    echo "  CFBundleExecutable: $(/usr/libexec/PlistBuddy -c 'Print CFBundleExecutable' "$PLIST" 2>/dev/null || echo '?')"
    for b in marspot-shell marspot-core marspot-shelld; do
      local p; p="$(bundle_bin "$b")"
      if [[ -f "$p" && ! -L "$p" ]]; then
        echo "  $b: $(stat -f '%z' "$p") B"
      elif [[ -L "$p" ]]; then
        echo "  $b: SYMLINK → $(readlink "$p")  (should be a real copy)"
      else
        echo "  $b: (absent)"
      fi
    done
  else
    echo "  (not installed)"
  fi
  echo "Running:"
  "$MACOS/marspot-shell" --status 2>/dev/null | sed -n '3,6p' | sed 's/^/  /' || echo "  (shell --status unavailable)"
}

if [[ "$MODE" == status ]]; then
  cmd_status
  exit 0
fi

# ── 1. Build ──────────────────────────────────────────────────────
if (( BUILD )); then
  echo "==> building release (shell + core + shelld + session)"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-shelld --bin marspot-session 2>&1 | tail -3 )
fi
# marspot-session is the per-pane L3 engine: core spawns it as its
# sibling, so it must ship in the bundle (and ride updates) or L3 silently
# falls back to the in-process grid.  Omitting it here was a real bug.
for b in marspot-shell marspot-core marspot-shelld marspot-session; do
  [[ -x "$TARGET/$b" ]] || { echo "ERROR: $TARGET/$b missing after build" >&2; exit 1; }
done

# ── 2. Scaffold the bundle (first run) ────────────────────────────
mkdir -p "$MACOS"
if [[ ! -f "$PLIST" ]]; then
  echo "==> creating Info.plist"
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
  <key>CFBundleShortVersionString</key><string>0.2.0</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
fi
# No symlinks: a stale `marspot` symlink into target/ would make the
# installed app track dev builds.  Drop it; the bundle runs the shell.
if [[ -L "$MACOS/marspot" ]]; then
  echo "==> removing legacy target/ symlink ($MACOS/marspot)"
  rm -f "$MACOS/marspot"
fi
/usr/libexec/PlistBuddy -c 'Set :CFBundleExecutable marspot-shell' "$PLIST" 2>/dev/null \
  || /usr/libexec/PlistBuddy -c 'Add :CFBundleExecutable string marspot-shell' "$PLIST"

# ── 3. Decide the silent update BEFORE overwriting the bundle ─────
# Order matters: step 4 overwrites the bundle binaries, so any
# "did this change?" comparison MUST happen first — otherwise the
# bundle would compare equal to the build we just copied over it and
# nothing would ever look changed.
running_equiv() {
  # What the running supervisor came from: current/ slot if a prior
  # update populated it, else the (still-old) bundle binary.
  local bin="$1"
  if [[ -f "$TREE/current/$bin" ]]; then echo "$TREE/current/$bin"; else echo "$MACOS/$bin"; fi
}
changed() {
  local bin="$1" ref
  ref="$(running_equiv "$bin")"
  [[ ! -f "$ref" ]] || ! cmp -s "$TARGET/$bin" "$ref"
}
stage() {
  local bin="$1"
  mkdir -p "$TREE/pending"
  cp "$TARGET/$bin" "$TREE/pending/$bin"
  xattr -d com.apple.quarantine "$TREE/pending/$bin" 2>/dev/null || true
  xattr -d com.apple.provenance "$TREE/pending/$bin" 2>/dev/null || true
  echo "    $bin: staged → pending/"
}

RUNNING=0; prod_shell_running && RUNNING=1
SHELL_CHANGED=0; changed marspot-shell  && SHELL_CHANGED=1
CORE_CHANGED=0;  changed marspot-core   && CORE_CHANGED=1
SHELLD_CHANGED=0; changed marspot-shelld && SHELLD_CHANGED=1
SESSION_CHANGED=0; changed marspot-session && SESSION_CHANGED=1

STAGED=0
if (( RUNNING )); then
  echo "==> staging changed binaries into the running app"
  (( SHELL_CHANGED )) && { stage marspot-shell; STAGED=1; } || echo "    marspot-shell: unchanged"
  (( CORE_CHANGED ))  && { stage marspot-core;  STAGED=1; } || echo "    marspot-core: unchanged"
  # Session rides with the core: the freshly-spawned core boot-promotes
  # pending/marspot-session → current/ (updater::promote_pending_session),
  # so a changed session must be staged whenever we restart the core.
  (( SESSION_CHANGED )) && { stage marspot-session; STAGED=1; } || echo "    marspot-session: unchanged"
fi

# ── 4. Install the bundle binaries (cold-launch fallback) ─────────
echo "==> installing bundle binaries"
install -m 0755 "$TARGET/marspot-shell"   "$MACOS/marspot-shell"
install -m 0755 "$TARGET/marspot-core"    "$MACOS/marspot-core"
install -m 0755 "$TARGET/marspot-shelld"  "$MACOS/marspot-shelld"
install -m 0755 "$TARGET/marspot-session" "$MACOS/marspot-session"
for b in marspot-shell marspot-core marspot-shelld marspot-session; do
  xattr -d com.apple.quarantine "$MACOS/$b" 2>/dev/null || true
  xattr -d com.apple.provenance "$MACOS/$b" 2>/dev/null || true
done
/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister \
  -f "$APP" >/dev/null 2>&1 || true

# ── 5. shelld LaunchAgent (production daemon, default socket) ──────
if ! pgrep -f "$MACOS/marspot-shelld" >/dev/null 2>&1; then
  echo "==> shelld not running — installing LaunchAgent"
  "$ROOT/bin/install-shelld.sh" >/dev/null
fi

# ── 6. Apply the silent update ────────────────────────────────────
if (( ! RUNNING )); then
  # `resolve_runnable` prefers binaries/current/ over the bundle, so a
  # stale current/ from a prior update would shadow the fresh bundle we
  # just installed and the cold-launched app would run the OLD code.
  # Refresh current/ to the new build (all four) so the launch runs this
  # build regardless of resolve order.
  if [[ -d "$TREE/current" ]]; then
    echo "==> refreshing binaries/current/ to match new bundle (was shadowing)"
    for b in marspot-shell marspot-core marspot-shelld marspot-session; do
      cp "$TARGET/$b" "$TREE/current/$b"
      xattr -d com.apple.quarantine "$TREE/current/$b" 2>/dev/null || true
      xattr -d com.apple.provenance "$TREE/current/$b" 2>/dev/null || true
    done
  fi
  echo "==> no running app — launching"
  open "$APP"
  echo "==> done.  Marspot started from $APP"
  exit 0
fi

if (( STAGED )); then
  echo "==> triggering silent update (window + sessions survive)"
  DEADLINE=$(( $(date +%s) + 60 )); NEXT=0
  while :; do
    left=0
    [[ -f "$TREE/pending/marspot-shell"   ]] && left=1
    [[ -f "$TREE/pending/marspot-core"    ]] && left=1
    [[ -f "$TREE/pending/marspot-session" ]] && left=1
    (( left == 0 )) && break
    now=$(date +%s)
    (( now >= DEADLINE )) && { echo "WARN: pending/ not consumed in 60s — see marspot-shell --status" >&2; exit 1; }
    if (( now >= NEXT )); then "$MACOS/marspot-shell" --trigger >/dev/null 2>&1 || true; NEXT=$(( now + 3 )); fi
    sleep 0.5
  done
  echo "    applied.  $(tail -1 "$SUP_LOG" 2>/dev/null)"
else
  echo "==> running app already matches this build"
fi

# ── 7. shelld update (opt-in; kills sessions) ─────────────────────
if (( SHELLD_CHANGED )); then
  if (( WITH_SHELLD )); then
    echo "==> updating shelld (this restarts the daemon — sessions will die)"
    mkdir -p "$TREE/pending"
    cp "$TARGET/marspot-shelld" "$TREE/pending/marspot-shelld"
    "$ROOT/bin/install-shelld.sh" --apply-pending
  else
    echo "==> note: marspot-shelld differs but was NOT updated (would kill sessions)."
    echo "    run 'bin/install-local.sh --with-shelld' when you can drop sessions."
  fi
fi

echo "==> done."

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
#                                   #   running app — L2 (core) + L3 (session)
#                                   #   swap silently; L1 (shell) skipped to
#                                   #   avoid the NSWindow flash from execv.
#   bin/install-local.sh --with-shell    # also apply a hot L1 shell update
#                                        #   (window flashes closed→open ~100ms)
#   bin/install-local.sh --with-shelld   # also update the daemon
#                                        #   (in-place execv, sessions survive)
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
SUP_LOG="$HOME/Library/Logs/Marspot/marspot.log"
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
WITH_SHELL=0
MODE=install
for arg in "$@"; do
  case "$arg" in
    --no-build)    BUILD=0 ;;
    --with-shelld) WITH_SHELLD=1 ;;
    --with-shell)  WITH_SHELL=1 ;;
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
# Extract the git sha embedded by build.rs into a marspot binary. The
# rodata holds the sha and the build timestamp adjacently — pull just
# the sha + optional -dirty marker. Empty output = unable to find one
# (caller falls back to byte-compare).
binary_git_sha() {
  local bin="$1"
  [[ -f "$bin" ]] || { echo ""; return; }
  # The marspot + marspot-term crates embed a literal `MARSPOT_FP=<sha>|
  # <ts>|END` static into the binary's rodata so this extractor is
  # robust to linker ordering — no regex guessing where the sha lives
  # next to. Returns just the sha portion (with optional `-dirty`).
  strings "$bin" 2>/dev/null \
    | grep -oE 'MARSPOT_FP=[0-9a-f]{8}(-dirty)?\|' \
    | head -1 \
    | sed -E 's/^MARSPOT_FP=//;s/\|$//'
}

# L2 (marspot-core) is the canonical version carrier. L1 (marspot-shell)
# is a thin stable wrapper; L3 (marspot-session) is a per-pane child of
# L2. They all ride on the same git commit, so we decide "is this
# install-local a no-op or a real update?" by looking at L2's fingerprint
# alone:
#
#   bundle L2 sha == target L2 sha, no `-dirty` suffix on either side
#   ⇒ same commit, nothing to do. Skip stage for ALL four binaries so
#     no SIGUSR1 fires, no shell self-update execv runs, no window
#     flashes.
#
# Any other state ⇒ fall back to the historical per-binary `cmp -s`
# byte compare, which keeps dirty-workspace dev iteration honest (every
# touched recompile re-stages and re-execs).
#
# The cache is computed once on first `changed()` call and reused for
# all four binaries within one install-local run.
_L2_NOOP=""  # "yes" / "no" once decided
_l2_decision() {
  if [[ -z "$_L2_NOOP" ]]; then
    local ref tgt ref_sha tgt_sha
    ref="$(running_equiv marspot-core)"
    tgt="$TARGET/marspot-core"
    if [[ -f "$ref" && -f "$tgt" ]]; then
      ref_sha="$(binary_git_sha "$ref")"
      tgt_sha="$(binary_git_sha "$tgt")"
      if [[ -n "$ref_sha" && -n "$tgt_sha" \
            && "$ref_sha" == "$tgt_sha" \
            && "$ref_sha" != *-dirty ]]; then
        _L2_NOOP=yes
      else
        _L2_NOOP=no
      fi
    else
      _L2_NOOP=no
    fi
  fi
  [[ "$_L2_NOOP" == yes ]]
}

changed() {
  local bin="$1" ref
  ref="$(running_equiv "$bin")"
  [[ -f "$ref" ]] || return 0
  if _l2_decision; then
    return 1  # L2 says same clean commit — whole tree is no-op
  fi
  ! cmp -s "$TARGET/$bin" "$ref"
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

# Print the version vector from version-vector.toml so the operator
# can correlate this install with which layers actually moved. L2
# (core) is the headline marspot version.
print_version_vector() {
  echo "==> version vector (this build):"
  # L1=shell, L2=core, L3=session, L4=shelld is the canonical ordering
  # everywhere in the codebase. L2 (core) is the headline marspot
  # version — what the title bar shows.
  local sh co se shd
  while IFS='=' read -r key value; do
    key="$(echo "$key" | tr -d ' ')"
    # Strip trailing inline comment (`# L1` etc) and surrounding
    # quotes / whitespace from the value before storing it.
    value="$(echo "$value" | sed -E 's/[[:space:]]*#.*$//' | tr -d ' \"')"
    [[ -z "$key" || "$key" == \#* ]] && continue
    case "$key" in
      shell)   sh="$value"  ;;
      core)    co="$value"  ;;
      session) se="$value"  ;;
      shelld)  shd="$value" ;;
    esac
  done < "$ROOT/version-vector.toml"
  printf "      L1 shell   %s\n" "${sh:-?}"
  printf "      L2 core    %s   ← marspot version (title bar)\n" "${co:-?}"
  printf "      L3 session %s\n" "${se:-?}"
  printf "      L4 shelld  %s\n" "${shd:-?}"
}
print_version_vector

STAGED=0
if (( RUNNING )); then
  echo "==> staging changed binaries into the running app"
  # L1 (marspot-shell) is the stable outer shell. Updating it requires
  # `apply_pending_update` → `try_apply_shell_self_update` → libc::execv,
  # which tears down + recreates the NSWindow (the ~100 ms visible
  # flash documented in docs/silent-update.md). Almost every dev iter
  # touches only L2 / L3 code, so default install-local skips L1
  # staging entirely: the bundle binary still gets refreshed below for
  # the next cold launch, but the running shell is not asked to swap
  # itself, so no flash. Pass --with-shell when an actual L1 code
  # change needs to land hot.
  if (( SHELL_CHANGED )); then
    if (( WITH_SHELL )); then
      stage marspot-shell; STAGED=1
    else
      echo "    marspot-shell: differs but NOT staged (default: avoid L1 execv flash);"
      echo "                   pass --with-shell to apply hot. Bundle will refresh on next cold launch."
    fi
  else
    echo "    marspot-shell: unchanged"
  fi
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
  xattr -c "$MACOS/$b" 2>/dev/null || true   # clear ALL provenance/quarantine
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
      # MUST be `install` (write-temp + atomic rename = fresh inode), not
      # `cp` (in-place overwrite = same inode).  macOS caches a binary's
      # code-signature cdhash per vnode; overwriting in place leaves the
      # kernel with the OLD core's cached signature, so when the shell
      # spawns the new core at the same path AMFI/taskgated rejects it
      # ("Invalid Signature") and kills it — a fresh inode forces a
      # re-validate.  `xattr -c` clears ALL provenance/quarantine xattrs
      # (a stray com.apple.provenance can also stall a spawned child).
      install -m 0755 "$TARGET/$b" "$TREE/current/$b"
      xattr -c "$TREE/current/$b" 2>/dev/null || true
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

# ── 7. shelld update (opt-in; in-place execv preserves sessions) ───
# Default now: --apply-pending-execv. The shelld supervisor receives
# SIGUSR1, promotes pending → current internally, and execv's over its
# own image while preserving the listen fd + every PTY master fd, so
# the running zsh children at the other end of every session keep
# living. GUI clients see a sub-second read pause; the ShelldClient
# supervisor loop reconnects + re-attaches each session via bytelog
# replay (no "exited" blink, no GUI quit). Falls back to the legacy
# bootout/bootstrap path (--apply-pending) only when the running
# shelld is older than the SIGUSR1 handler — install-shelld.sh checks.
if (( SHELLD_CHANGED )); then
  if (( WITH_SHELLD )); then
    echo "==> updating shelld via execv (sessions preserved)"
    mkdir -p "$TREE/pending"
    cp "$TARGET/marspot-shelld" "$TREE/pending/marspot-shelld"
    if ! "$ROOT/bin/install-shelld.sh" --apply-pending-execv; then
      echo "==> execv path declined — falling back to --apply-pending (KILLS sessions)"
      "$ROOT/bin/install-shelld.sh" --apply-pending
    fi
  else
    echo "==> note: marspot-shelld differs but was NOT updated"
    echo "    run 'bin/install-local.sh --with-shelld' to apply via execv (session-preserving)"
  fi
fi

echo "==> done."

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
#                                   #   running app — L1 / L2 / L3 all
#                                   #   swap if changed. L1 swap is the
#                                   #   ~100 ms NSWindow flash (sessions
#                                   #   survive via L3 reattach); known
#                                   #   cost, by design.
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
# RFC-004 D.1: the root moved to Application Support; the old Caches
# path survives as a symlink after the binaries' one-time migration.
# Root selection: the root whose shell.pid points at a LIVE process
# wins (that's where the running app actually lives — 2026-07-17
# incident: an empty Application Support shell dir out-ranked the
# real Caches root, shell.pid wasn't found, and the script "launched"
# a duplicate app).  With no live shell anywhere, prefer whichever
# root exists, new first.
NEW_ROOT="$HOME/Library/Application Support/marspot"
OLD_ROOT="$HOME/Library/Caches/marspot"
root_shell_alive() {
  local p
  p=$(cat "$1/shell.pid" 2>/dev/null) || return 1
  [[ -n "$p" ]] && kill -0 "$p" 2>/dev/null
}
if root_shell_alive "$NEW_ROOT"; then
  STATE_ROOT="$NEW_ROOT"
elif root_shell_alive "$OLD_ROOT"; then
  STATE_ROOT="$OLD_ROOT"
elif [[ -e "$NEW_ROOT" ]]; then
  STATE_ROOT="$NEW_ROOT"
else
  STATE_ROOT="$OLD_ROOT"
fi
TREE="$STATE_ROOT/binaries"
SUP_LOG="$HOME/Library/Logs/Marspot/marspot.log"
PROD_PID_FILE="$STATE_ROOT/shell.pid"

# Is the installed GUI shell actually running?  Uses its pid file
# (written on startup), NOT a `pgrep marspot-shell` — that substring
# also matches `marspot-shelld` and would report a phantom shell.
prod_shell_running() {
  local p
  p=$(cat "$PROD_PID_FILE" 2>/dev/null) || return 1
  [[ -n "$p" ]] && kill -0 "$p" 2>/dev/null
}

# Structured log helper.  Mirrors install-shelld.sh's sup_log so install
# steps land on the same marspot.log timeline as core / shell / shelld
# events.  Best-effort: silently no-ops when no binary is reachable.
# Added 2026-06-15 after install-local's pgrep-based shelld liveness
# check returned a false negative, triggered launchctl bootout, and
# took down 9 active claudecode sessions — without a single line in
# marspot.log explaining why.  We won't be invisible like that again.
sup_log() {
  local tag="$1"; shift
  local detail="$*"
  for bin in "$MACOS/marspot-core" "$TREE/current/marspot-core" "$TARGET/marspot-core"; do
    [[ -x "$bin" ]] || continue
    "$bin" --log-event "$tag" "$detail" >/dev/null 2>&1 && return 0 || true
  done
}

BUILD=1
MODE=install
for arg in "$@"; do
  case "$arg" in
    --no-build)    BUILD=0 ;;
    # --with-shelld retired 2026-06-25: see section 7 comment + RFC-003.
    # Accept + ignore so existing user scripts don't error.
    --with-shelld) ;;
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
    for b in marspot-shell marspot-core; do
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
  echo "==> building release (shell + core + session)"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 )
fi
# marspot-session is the per-pane L3 engine: core spawns it as its
# sibling, so it must ship in the bundle (and ride updates) or L3 silently
# falls back to the in-process grid.  Omitting it here was a real bug.
for b in marspot-shell marspot-core marspot-session; do
  [[ -x "$TARGET/$b" ]] || { echo "ERROR: $TARGET/$b missing after build" >&2; exit 1; }
done

# ── 1b. Developer-ID code signing ─────────────────────────────────
# Without this, freshly-written binaries (new inode after cargo + cp)
# trigger macOS amfid full verification on first launch — measured
# 38 s for adhoc-signed marspot-core, blocking the silent-update
# swap.  Signing with a Developer ID Application cert routes amfid
# to its fast path (sub-second).  `--timestamp=none` skips the
# Apple timestamp-server round-trip we don't need for local install.
#
# Override the identity via MARSPOT_SIGN_ID env if you have a
# different cert.  If signing fails (no cert available), install
# still works — just with the 30-60 s amfid penalty on next core
# swap; warn loudly but don't abort.
#
# Runs regardless of --no-build.  Signing is about the binaries being
# INSTALLED, not about who compiled them: this block used to sit
# behind `if (( BUILD ))`, so `--no-build` (the path used when the
# binaries were cross-built on the bench host) installed adhoc,
# linker-signed binaries.  AMFI killed the shell the instant it
# execv'd into one — the whole app went down with sixteen live
# sessions behind it.
SIGN_ID="${MARSPOT_SIGN_ID:-}"
if [[ -z "$SIGN_ID" ]]; then
  SIGN_ID=$(security find-identity -v -p codesigning 2>/dev/null \
            | awk -F'"' '/Developer ID Application/{print $2; exit}')
fi
# Signing is not advisory.  An adhoc binary costs the first-run amfid
# stall AND fails the csreq behind the Developer Tools grant, so the
# whole app loses its Gatekeeper exemption — every binary the user
# compiles then pays a scan.  Both used to be a WARN and an install
# that carried on regardless; a silent downgrade to adhoc is exactly
# the outcome nobody would have chosen if asked.  Refuse instead, and
# say what to do.  `MARSPOT_ALLOW_ADHOC=1` makes it a deliberate act.
adhoc_bail() {
  echo "" >&2
  echo "==> REFUSING to install adhoc-signed binaries." >&2
  echo "    $1" >&2
  echo "" >&2
  echo "    An adhoc binary stalls 30-60 s in amfid on first run, and does" >&2
  echo "    not satisfy the code requirement behind the Developer Tools" >&2
  echo "    grant — so everything you compile inside marspot pays a" >&2
  echo "    Gatekeeper scan on ITS first run too." >&2
  echo "" >&2
  if [[ -n "${SSH_CONNECTION:-}" ]]; then
    echo "    You are over ssh, where codesign cannot reach the login" >&2
    echo "    keychain (errSecInternalComponent, even though" >&2
    echo "    find-identity lists the cert).  Two ways out:" >&2
    echo "" >&2
    echo "      • from your workstation:  bin/install-remote.sh <host>" >&2
    echo "        (signs locally, ships a signed bundle; no private key" >&2
    echo "         and no keychain password ever leaves this machine)" >&2
    echo "      • or on that host, once:" >&2
    echo "          security unlock-keychain ~/Library/Keychains/login.keychain-db" >&2
  else
    echo "    Install a 'Developer ID Application' certificate, or set" >&2
    echo "    MARSPOT_SIGN_ID to the identity you want used." >&2
  fi
  echo "" >&2
  echo "    MARSPOT_ALLOW_ADHOC=1 proceeds anyway, knowing the above." >&2
  exit 1
}
if [[ -n "$SIGN_ID" ]]; then
  echo "==> signing release binaries"
  echo "    identity: $SIGN_ID"
  for b in marspot-shell marspot-core marspot-session; do
    # No pipe around codesign: with a pipe the exit status belongs to
    # whatever is downstream, and only `pipefail` made this work at all.
    if ! out=$(codesign --force --sign "$SIGN_ID" --timestamp=none "$TARGET/$b" 2>&1); then
      echo "$out" | sed "s/^/    /" >&2
      [[ -n "${MARSPOT_ALLOW_ADHOC:-}" ]] \
        || adhoc_bail "codesign failed for $b."
      echo "    WARN: $b stays adhoc (MARSPOT_ALLOW_ADHOC=1)" >&2
    else
      echo "$out" | sed "s/^/    /"
    fi
  done
  # Verify rather than trust: codesign can exit 0 and still leave a
  # binary the loader will not accept.
  # Capture, then test.  `... | grep -q` under `set -o pipefail` reports
  # failure for a correctly signed binary: grep exits at the first match
  # and codesign dies of SIGPIPE writing into the closed pipe.
  for b in marspot-shell marspot-core marspot-session; do
    info=$(codesign -dv "$TARGET/$b" 2>&1 || true)
    case "$info" in
      *TeamIdentifier=*) ;;
      *) [[ -n "${MARSPOT_ALLOW_ADHOC:-}" ]] \
           || adhoc_bail "$b has no TeamIdentifier after signing." ;;
    esac
  done
else
  [[ -n "${MARSPOT_ALLOW_ADHOC:-}" ]] \
    || adhoc_bail "No 'Developer ID Application' certificate found."
  echo "    WARN: no cert; binaries stay adhoc (MARSPOT_ALLOW_ADHOC=1)" >&2
fi

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
  <key>CFBundleIconFile</key><string>AppIcon</string>
</dict>
</plist>
PLIST
fi
# Make sure CFBundleIconFile is wired (idempotent — for old bundles
# created before the icon landed).
/usr/libexec/PlistBuddy -c 'Set :CFBundleIconFile AppIcon' "$PLIST" 2>/dev/null \
  || /usr/libexec/PlistBuddy -c 'Add :CFBundleIconFile string AppIcon' "$PLIST"
# Install the .icns into Resources/.  iconutil's output isn't in the
# repo (built from assets/Marspot.iconset/ at gen time);  fall back to
# the iconset's 512px PNG if .icns isn't available.
mkdir -p "$MACOS/../Resources"
if [[ -f "$ROOT/assets/AppIcon.icns" ]]; then
  cp "$ROOT/assets/AppIcon.icns" "$MACOS/../Resources/AppIcon.icns"
fi
# macOS aggressively caches icons by bundle identifier.  Touching the
# bundle root tells Finder/Dock to re-read the icon on next launch.
touch "$MACOS/.." 2>/dev/null || true
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

# Extract the per-layer version (e.g. `0.5.4`) from a binary's
# `MARSPOT_LAYER_VERS=shell:X|core:Y|session:Z|END` rodata marker.
# Returns empty when the marker isn't present (older binary, predates
# the per-layer-version install-local skip path).
# $1 = binary path, $2 = layer name (shell / core / session).
binary_layer_ver() {
  local bin="$1" layer="$2"
  [[ -f "$bin" && -n "$layer" ]] || { echo ""; return; }
  strings "$bin" 2>/dev/null \
    | grep -oE 'MARSPOT_LAYER_VERS=[^[:cntrl:]]*\|END' \
    | head -1 \
    | grep -oE "${layer}:[0-9A-Za-z._-]+" \
    | head -1 \
    | sed -E "s/^${layer}://"
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
  # Per-layer version skip: if the binary embeds a
  # MARSPOT_LAYER_VERS marker AND that layer's version is unchanged
  # between running and target, the developer's intent (as declared
  # in version-vector.toml) is "this layer didn't change."  Skip
  # stage so a pure-L2 install doesn't restage L1 just because the
  # rebuild bumped the git sha + build timestamp embedded in every
  # binary's rodata.
  # Gated on `-dirty` absence: a dirty workspace is mid-iteration,
  # the version vector may not yet reflect uncommitted source — fall
  # through to byte-compare so dev's "edit + install + see change"
  # cycle keeps working.
  local layer=""
  case "$bin" in
    marspot-shell)   layer="shell" ;;
    marspot-core)    layer="core" ;;
    marspot-session) layer="session" ;;
  esac
  if [[ -n "$layer" ]]; then
    local tgt_sha
    tgt_sha="$(binary_git_sha "$TARGET/$bin")"
    if [[ -n "$tgt_sha" && "$tgt_sha" != *-dirty ]]; then
      local ref_ver tgt_ver
      ref_ver="$(binary_layer_ver "$ref" "$layer")"
      tgt_ver="$(binary_layer_ver "$TARGET/$bin" "$layer")"
      if [[ -n "$ref_ver" && -n "$tgt_ver" && "$ref_ver" == "$tgt_ver" ]]; then
        return 1
      fi
    fi
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
SESSION_CHANGED=0; changed marspot-session && SESSION_CHANGED=1

# Print the version vector from version-vector.toml so the operator
# can correlate this install with which layers actually moved. L2
# (core) is the headline marspot version.
print_version_vector() {
  echo "==> version vector (this build):"
  # L1=shell, L2=core, L3=session is the canonical ordering everywhere
  # in the codebase since RFC-003 retired L4 shelld.  L2 (core) is the
  # headline marspot version — what the title bar shows.
  local sh co se
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
    esac
  done < "$ROOT/version-vector.toml"
  printf "      L1 shell   %s\n" "${sh:-?}"
  printf "      L2 core    %s   ← marspot version (title bar)\n" "${co:-?}"
  printf "      L3 session %s\n" "${se:-?}"
}
print_version_vector

STAGED=0
if (( RUNNING )); then
  echo "==> staging changed binaries into the running app"
  # L1 (marspot-shell) update path: `apply_pending_update` →
  # `try_apply_shell_self_update` → libc::execv, which tears down +
  # recreates the NSWindow (~100 ms visible flash, see docs/silent-
  # update.md). Sessions survive via L3 reattach + bytelog replay —
  # the flash is the *only* user-visible cost, and it's the agreed
  # design (a long-lived "skip L1" path silently strands new shell
  # binaries on disk and is worse than the flash).
  (( SHELL_CHANGED ))   && { stage marspot-shell; STAGED=1; } || echo "    marspot-shell: unchanged"
  (( CORE_CHANGED ))  && { stage marspot-core;  STAGED=1; } || echo "    marspot-core: unchanged"
  # Session rides with the core: the freshly-spawned core boot-promotes
  # pending/marspot-session → current/ (updater::promote_pending_session),
  # so a changed session must be staged whenever we restart the core.
  (( SESSION_CHANGED )) && { stage marspot-session; STAGED=1; } || echo "    marspot-session: unchanged"
fi

# ── 4. Install the bundle binaries (cold-launch fallback) ─────────
#
# Bundle binaries are the cold-launch fallback path.  Overwriting them
# while their process is RUNNING is dangerous:
#
#   - `install` does atomic rename → new inode, so the old mmap'd inode
#     in the running process technically lives until close.  In
#     practice macOS AMFI / taskgated re-checks the bundle CDHash on
#     various spot events (page faults, fork, etc), and if the disk
#     image's CDHash no longer matches the in-kernel record, the
#     running process gets killed.
#   - shelld 35237 died at exactly the moment of bundle overwrite on
#     2026-06-16 (see SHELLD_STOP @ 11:11:42.917).  LaunchAgent
#     respawned it 47 ms later but session table was empty — the 9
#     L3 children's ATTACH(id=1..9) hit a brand-new shelld that
#     didn't know those ids → no StateSnapshot → all 9 panes blank.
#
# So: skip bundle overwrite for any binary whose process is running
# against THIS bundle path.  Cold launch will pick it up next time
# the running instance exits cleanly.
echo "==> installing bundle binaries"
# macOS pgrep -f has a known blind spot for LaunchAgent-spawned processes
# — the production shelld (launched by com.marspot.shelld.plist) doesn't
# clearly lists it with that exact ARGV[0].  So mirror line 330's pattern:
# `ps auxww | grep -F "$MACOS/$b"` is the only check that consistently
# matches both manually-launched and LaunchAgent-launched bundle bins.
for b in marspot-shell marspot-core marspot-session; do
  running=0
  if ps auxww 2>/dev/null | grep -F "$MACOS/$b" | grep -vq grep; then
    running=1
  fi
  if (( running )); then
    echo "    $b: skipped bundle overwrite ($MACOS/$b is in use; cold launch picks up new bin on next exit)"
    sup_log "INSTALL_BUNDLE_SKIP" "$b in use; skipped bundle overwrite to avoid AMFI kill"
    # L1 is special since the redirect was turned off: the process
    # that runs is the BUNDLE binary, so a new L1 only takes effect
    # once this one has exited and a later install can write it.
    # Until then the app keeps its Gatekeeper exemption but runs the
    # older shell.
    if [[ "$b" == "marspot-shell" ]] && (( SHELL_CHANGED )); then
      echo "       ↳ L1 changed: arming a one-shot lander for the next full quit."
      ARM_LANDER=1
    fi
  else
    install -m 0755 "$TARGET/$b" "$MACOS/$b"
    xattr -c "$MACOS/$b" 2>/dev/null || true   # clear ALL provenance/quarantine
  fi
done
/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister \
  -f "$APP" >/dev/null 2>&1 || true

# ── 4b. One-shot bundle lander ────────────────────────────────────
# A bundle binary cannot be overwritten while its own process runs —
# AMFI kills the process when the on-disk CDHash stops matching (nine
# panes went blank that way on 2026-06-16), and L1 no longer redirects,
# so the process that runs IS the bundle binary.  A changed L1
# therefore cannot land until the app fully quits.
#
# This used to be a hand-made script outside the repo with a
# RunAtLoad LaunchAgent, and it had no idea what "newest" meant: it
# kept its staged copy after landing and re-landed it at every login,
# so on 2026-09-07 a reboot silently reverted the bundle to binaries
# from the previous morning.  An update mechanism that can move the
# app BACKWARDS is worse than one that does nothing.
#
# So the rule here is: the stage is valid only while it is byte-identical
# to binaries/current/, which every install refreshes to the newest
# build.  Anything else means a newer install has happened since, and
# the lander deletes itself instead of landing.  It is one-shot in
# every exit path.
LANDER_DIR="$STATE_ROOT/pending-bundle"
LANDER_PLIST="$HOME/Library/LaunchAgents/com.marspot.land-bundle.plist"
if (( ${ARM_LANDER:-0} )); then
  echo "==> arming one-shot bundle lander (lands on next full quit)"
  mkdir -p "$LANDER_DIR"
  for b in marspot-shell marspot-core marspot-session; do
    install -m 0755 "$TARGET/$b" "$LANDER_DIR/$b"
  done
  cat > "$LANDER_DIR/land.sh" <<'LANDER'
#!/bin/sh
# Generated by bin/install-local.sh — do not edit here; edit the
# generator.  One-shot: every exit path below clears the stage and
# removes the LaunchAgent, so this can run at most once per install.
# Overridable so the guard below can be exercised against a sandbox
# instead of the user's real app — the failure this replaces was one
# nobody could test.
APP="${MARSPOT_APP:-$HOME/.local/Marspot.app}"; MACOS="$APP/Contents/MacOS"
STATE="${MARSPOT_STATE_DIR:-$HOME/Library/Application Support/marspot}"
STAGE="$STATE/pending-bundle"; CURRENT="$STATE/binaries/current"
PLIST="${MARSPOT_LANDER_PLIST:-$HOME/Library/LaunchAgents/com.marspot.land-bundle.plist}"
WAIT_FOR="${MARSPOT_LANDER_PROCESS:-marspot-shell}"
LOG="$STAGE/land.log"
exec >>"$LOG" 2>&1
disarm() { rm -f "$PLIST" "$STAGE"/marspot-* ; }
echo "=== $(date) land.sh ==="
# Never move the app backwards.  binaries/current/ is refreshed by
# every install; a stage that no longer matches it was superseded.
for b in marspot-shell marspot-core marspot-session; do
  if ! cmp -s "$STAGE/$b" "$CURRENT/$b"; then
    echo "stage is stale ($b differs from binaries/current) — discarding, not landing"
    disarm; exit 0
  fi
done
i=0
while [ $i -lt 900 ]; do
  pgrep -x "$WAIT_FOR" >/dev/null 2>&1 || break
  sleep 2; i=$((i+1))
done
if pgrep -x "$WAIT_FOR" >/dev/null 2>&1; then
  echo "still running after $((i*2))s — leaving the stage armed for next login"
  exit 0
fi
for b in marspot-shell marspot-core marspot-session; do
  /usr/bin/install -m 0755 "$STAGE/$b" "$MACOS/$b" && echo "installed $b"
  /usr/bin/xattr -c "$MACOS/$b" 2>/dev/null
done
# Changing any bundle content invalidates the outer signature, and the
# Developer Tools TCC grant is matched against it — so re-sign the
# binaries AND the bundle, innermost first.
ID=$(security find-identity -v -p codesigning 2>/dev/null | grep -i "Developer ID Application" | head -1 | sed 's/.*"\(.*\)".*/\1/')
if [ -n "$ID" ]; then
  for b in marspot-shell marspot-core marspot-session; do
    /usr/bin/codesign -f -s "$ID" --timestamp=none "$MACOS/$b" && echo "signed $b"
  done
  /usr/bin/codesign -f -s "$ID" --timestamp=none "$APP" && echo "signed bundle"
  /usr/bin/codesign -v --deep --strict "$APP" && echo "bundle signature verifies"
fi
/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister -f "$APP" >/dev/null 2>&1
echo "=== landed: $(MARSPOT_NO_REDIRECT=1 "$MACOS/marspot-shell" --version 2>&1 | head -1) ==="
disarm
/usr/bin/open -n "$APP" && echo "relaunched"
LANDER
  chmod +x "$LANDER_DIR/land.sh"
  cat > "$LANDER_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.marspot.land-bundle</string>
<key>ProgramArguments</key><array><string>$LANDER_DIR/land.sh</string></array>
<key>RunAtLoad</key><true/>
</dict></plist>
PLIST
  launchctl bootout "gui/$(id -u)/com.marspot.land-bundle" >/dev/null 2>&1 || true
  launchctl bootstrap "gui/$(id -u)" "$LANDER_PLIST" >/dev/null 2>&1 || true
elif [[ -d "$LANDER_DIR" ]]; then
  # This install wrote the bundle itself, so anything staged earlier is
  # by definition older than what is now on disk.
  echo "==> disarming a previously-armed lander (this install landed directly)"
  launchctl bootout "gui/$(id -u)/com.marspot.land-bundle" >/dev/null 2>&1 || true
  rm -f "$LANDER_PLIST" "$LANDER_DIR"/marspot-*
fi

# ── 5. shelld LaunchAgent — retired (RFC-003) ─────────────────────
# F2+2b — L4 marspot-shelld was retired by RFC-003 (L3 owns its own
# PTY + UDS listener + registry; nothing dials shelld anymore).  Up
# to 2026-06-18 a stale `com.marspot.shelld.plist` was still being
# launchd-managed (KeepAlive: true), running a zombie 8.7 MB daemon
# and getting in the way of `launchctl bootout` paths every install.
# Tear-down is now manual + one-time:
#     launchctl unload ~/Library/LaunchAgents/com.marspot.shelld.plist
#     rm ~/Library/LaunchAgents/com.marspot.shelld.plist
# (a backup of the old plist sits in retired-launchagent-backup/ if
# we ever need to resurrect it).  The bootstrap path that previously
# called `install-shelld.sh` is gone — the script was already absent
# from the tree, so any `verdict_sum=0` branch would have errored
# out anyway.

# ── 6. Apply the silent update ────────────────────────────────────
if (( ! RUNNING )); then
  # `resolve_runnable` prefers binaries/current/ over the bundle, so a
  # stale current/ from a prior update would shadow the fresh bundle we
  # just installed and the cold-launched app would run the OLD code.
  # Refresh current/ to the new build (all four) so the launch runs this
  # build regardless of resolve order.
  if [[ -d "$TREE/current" ]]; then
    echo "==> refreshing binaries/current/ to match new bundle (was shadowing)"
    for b in marspot-shell marspot-core marspot-session; do
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
# Launch the app WITHOUT handing it this shell's session identity.
#
# `open(1)` forwards the caller's environment — verified 2026-09-07
# with a canary variable, which arrived intact in the launched app.
# So running this script from inside an agent pane made every pane the
# new app opened carry that agent's `CLAUDE_CODE_CHILD_SESSION`, its
# session id, and its messaging socket and token.  Agents started in
# those panes believed they were children of a session that was never
# theirs: four of them turned transcript saving off and exited without
# a word (2026-09-07).
#
# `marspot_term::pty::SESSION_ENV_PREFIXES` strips the same families
# when a pane spawns its shell; this keeps them out of the app to
# begin with, so the two do not have to agree to be safe.
launch_app_clean() {
  local unset_args=() name
  while IFS='=' read -r name _; do
    case "$name" in
      MARSPOT_*|CLAUDE_CODE_*|CLAUDECODE|CODEX_*) unset_args+=(-u "$name") ;;
    esac
  done < <(env)
  env "${unset_args[@]}" open "$@"
}

  echo "==> no running app — launching"
  launch_app_clean "$APP"
  echo "==> done.  Marspot started from $APP"
  exit 0
fi

# Did they actually take it?
#
# The fanout used to report the number of signals SENT, which is not
# the same claim and hid a real failure: four consecutive installs
# said "SIGTERM'd 13 L3 pids" while every pane went on serving the
# image it already had (2026-09-06).  A signal is a request; the
# only honest report is what is running afterwards.
# The pids that were running before the fanout, recorded so the check
# afterwards can tell "adopted" from "gone".
BEFORE_L3_PIDS=""
snapshot_l3_pids() { BEFORE_L3_PIDS="$(pgrep -f marspot-session 2>/dev/null | sort | tr '\n' ' ')"; }

verify_l3_adoption() {
  local want stale=0 total=0 gone=0 ino p
  want=$(stat -f %i "$TREE/current/marspot-session" 2>/dev/null) || return 0
  sleep 3
  local now
  now="$(pgrep -f marspot-session 2>/dev/null | sort | tr '\n' ' ')"
  # A pane that DIED is not in `now` at all, so counting only what is
  # still running answers "are the survivors up to date" and calls that
  # success.  On 2026-09-07 that printed "all 8 panes are on this
  # image" directly after five of them had been killed mid-execv.  The
  # set that was there before the signal is the only honest denominator.
  for p in $BEFORE_L3_PIDS; do
    case " $now " in
      *" $p "*) ;;
      *) gone=$((gone+1)); echo "    LOST: pid $p did not come back" >&2 ;;
    esac
  done
  for p in $(pgrep -f "$TREE/current/marspot-session" 2>/dev/null); do
    total=$((total+1))
    ino=$(lsof -p "$p" -a -d txt -Fi 2>/dev/null | grep '^i' | tr -d 'i' | head -1)
    [ "$ino" = "$want" ] || stale=$((stale+1))
  done
  if (( gone > 0 )); then
    echo "    FAILED: $gone pane(s) died across the update; $total still running" >&2
    echo "            (their content is in sessions/<id>/bytelog — see" >&2
    echo "             examples/rebuild_scrollback_from_bytelog)" >&2
    return 1
  fi
  if (( stale > 0 )); then
    echo "    WARN: $stale/$total panes are still on the previous image" >&2
    echo "          (a pane whose probe is wedged retries on the next" >&2
    echo "           signal; see l3.execv.probe_stuck in the log)" >&2
  else
    echo "    all $total panes are on this image, none lost"
  fi
}

# Session-only fast path: when only marspot-session changed (no L1/L2
# bin diff), the supervisor's SIGUSR1 path won't fire — it only
# promotes pending/marspot-core (see L1 main.rs apply_update gate on
# `binaries.has_pending()`).  Promote pending/marspot-session → current/
# ourselves, then SIGTERM every running L3.  Each L3's SIGTERM handler
# compares its rodata MARSPOT_FP to current/marspot-session's
# fingerprint; mismatched ones execv into the new image with
# control_stream_fd carry-across, matching ones clean-exit (then L2
# respawns).  No L1 flash, no L2 swap — silent + lossless across the
# L3 image bump.
if (( STAGED )) \
   && (( ! SHELL_CHANGED )) && (( ! CORE_CHANGED )) \
   && (( SESSION_CHANGED )); then
  echo "==> session-only update (L3 self-execv via SIGTERM fanout)"
  install -m 0755 "$TREE/pending/marspot-session" "$TREE/current/marspot-session"
  xattr -c "$TREE/current/marspot-session" 2>/dev/null || true
  rm -f "$TREE/pending/marspot-session"
  # Same cold-inode warm-up as the silent-update path below.
  MARSPOT_NO_REDIRECT=1 "$TREE/current/marspot-session" --version >/dev/null 2>&1 \
    || echo "    WARN: current/marspot-session did not start; L3s will refuse it" >&2
  snapshot_l3_pids
  signalled=0
  for pid in $(pgrep -f marspot-session 2>/dev/null); do
    if kill -TERM "$pid" 2>/dev/null; then
      signalled=$((signalled+1))
    fi
  done
  echo "    promoted current/marspot-session; SIGTERM'd $signalled L3 pids"
  verify_l3_adoption
  echo "==> done."
  exit 0
fi

if (( STAGED )); then
  echo "==> triggering silent update (window + sessions survive)"
  # Trigger ONCE up front.  Previous behaviour was to fire SIGUSR1
  # every 3s until pending/ drained, but supervisor enters probation
  # on the first trigger and ignores subsequent ones with a
  # `shell.sigusr1.ignored_not_idle` warn — a steady drumbeat that
  # spammed the log every install and (worse) raced the supervisor
  # state machine on the 2026-06-15 install-local that aborted with
  # `pending_core_HELLO_timeout`.  The pending → current move
  # happens at the spawned core's boot (SESSION_PROMOTE + the L2
  # promote), which is well under a second on a healthy build, so
  # this single trigger drains pending/ within the first few polls.
  # Backstop: if pending/ is still there after FALLBACK_RETRIGGER_S,
  # we assume the SIGUSR1 was genuinely lost (rare) and fire once
  # more — not a 3-second drumbeat.
  "$MACOS/marspot-shell" --trigger >/dev/null 2>&1 || true
  TRIGGER_AT=$(date +%s)
  RETRIGGERED=0
  FALLBACK_RETRIGGER_S=8
  DEADLINE=$(( $(date +%s) + 60 ))
  while :; do
    left=0
    [[ -f "$TREE/pending/marspot-shell"   ]] && left=1
    [[ -f "$TREE/pending/marspot-core"    ]] && left=1
    [[ -f "$TREE/pending/marspot-session" ]] && left=1
    (( left == 0 )) && break
    now=$(date +%s)
    (( now >= DEADLINE )) && { echo "WARN: pending/ not consumed in 60s — see marspot-shell --status" >&2; exit 1; }
    if (( ! RETRIGGERED && now - TRIGGER_AT >= FALLBACK_RETRIGGER_S )); then
      "$MACOS/marspot-shell" --trigger >/dev/null 2>&1 || true
      RETRIGGERED=1
    fi
    sleep 0.5
  done
  echo "    applied.  $(tail -1 "$SUP_LOG" 2>/dev/null)"

  # A silent update swaps L1 and L2.  It does NOT swap the L3s that
  # are already running: the new core promotes pending/marspot-session
  # into current/, so only panes spawned AFTER this point get the new
  # image, and every existing pane keeps the old one for as long as it
  # lives.  Measured 2026-09-06: L3s from two days earlier were still
  # serving every pane, so two session-layer fixes had never once run
  # on the machine they were installed on — and the report they were
  # meant to close ("进 history 还是会闪黑") was correct.
  #
  # Same mechanism the session-only fast path above uses, and it is
  # lossless: each L3 compares its own fingerprint against current/
  # and either execv's into the new image carrying its PTY across, or
  # clean-exits for L2 to respawn.  No PTY is restarted either way.
  # Warm the freshly-promoted image before asking anyone to adopt it.
  #
  # Each L3 probes the candidate by running it once, and refuses to
  # execv if that fails.  A just-installed binary is a NEW inode, so
  # the first exec pays a cold Gatekeeper verdict — and thirteen panes
  # probing a cold one at the same instant all got a failure back
  # within 144 ms, refused, and then sat on their old image while four
  # consecutive installs reported success (2026-09-06; measured, and
  # confirmed by a single manual SIGTERM succeeding once the verdict
  # had been warmed by hand).
  #
  # One exec here pays that once, for everyone.
  if (( SESSION_CHANGED )); then
    MARSPOT_NO_REDIRECT=1 "$TREE/current/marspot-session" --version >/dev/null 2>&1 \
      || echo "    WARN: current/marspot-session did not start; L3s will refuse it" >&2
    snapshot_l3_pids
    signalled=0
    for pid in $(pgrep -f marspot-session 2>/dev/null); do
      kill -TERM "$pid" 2>/dev/null && signalled=$((signalled+1))
    done
    echo "    L3 self-execv: SIGTERM'd $signalled session pids"
    verify_l3_adoption
  fi
else
  echo "==> running app already matches this build"
fi

# ── 7. (retired) shelld update path ───────────────────────────────
# Removed 2026-06-25: RFC-003 retired L4 shelld in 2026-06-17
# (L3 owns its own PTY + UDS listener + registry; nothing dials
# shelld anymore).  `bin/install-shelld.sh` was deleted at the same
# time, leaving an unreachable `--with-shelld` flag here that pointed
# at a missing script.  Section 5 above tears down the LaunchAgent;
# this section is intentionally empty so the daemon is never
# resurrected via install-local.

# ── 8. Claude Code status-line hook ───────────────────────────────
# Nothing to do here.  The hook's registration follows the
# `claudecode.statusline_hook` setting, which the running shell
# reconciles on its own sweep — including repointing an installed hook
# at this build's binary.  Installing marspot does not touch Claude
# Code's settings; the switch in the settings panel does.

echo "==> done."

#!/usr/bin/env bash
#
# Dev-loop silent update: push the local build into the RUNNING
# Marspot.app without touching the window or any shelld session.
#
#   build → stage changed binaries into binaries/pending/ → SIGUSR1
#
# The running shell applies them through the exact same path a real
# release takes (promote → swap/exec → probation → auto-rollback),
# so every dev-push also exercises the update machinery.  Only
# binaries that actually differ from the running slots are staged:
# a core-only change swaps the renderer invisibly; a shell change
# self-execs (one window flash, sessions persist).
#
# shelld is NOT pushed — restarting the daemon kills every session.
# When it differs the script says so and leaves the call to you
# (`bin/install-shelld.sh --apply-pending`).
#
# Usage:
#   bin/dev-push.sh            # build + stage + trigger
#   bin/dev-push.sh --no-build # stage + trigger existing target/release
#
# Exits 0 when everything staged was triggered (or nothing differed).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="$ROOT/target/release"
TREE="$HOME/Library/Caches/marspot/binaries"
SUP_LOG="$HOME/Library/Logs/Marspot/supervisor.log"

if [[ "${1:-}" != "--no-build" ]]; then
  echo "==> building marspot-shell + marspot-core (release)"
  ( cd "$ROOT" && cargo build --release --bin marspot-shell --bin marspot-core 2>&1 | tail -2 )
fi

# The binary the running process actually came from: current/ slot
# if a previous update populated it, else the app bundle.
running_equivalent() {
  local bin="$1"
  if [[ -f "$TREE/current/$bin" ]]; then
    echo "$TREE/current/$bin"
  else
    echo "$HOME/.local/Marspot.app/Contents/MacOS/$bin"
  fi
}

stage_if_changed() {
  local bin="$1"
  local src="$TARGET/$bin"
  [[ -x "$src" ]] || { echo "ERROR: $src missing"; exit 1; }
  local ref
  ref="$(running_equivalent "$bin")"
  if [[ -f "$ref" ]] && cmp -s "$src" "$ref"; then
    echo "    $bin: unchanged — skip"
    return 1
  fi
  mkdir -p "$TREE/pending"
  cp "$src" "$TREE/pending/$bin"
  xattr -d com.apple.quarantine "$TREE/pending/$bin" 2>/dev/null || true
  xattr -d com.apple.provenance "$TREE/pending/$bin" 2>/dev/null || true
  echo "    $bin: staged → pending/"
  return 0
}

echo "==> staging changed binaries"
STAGED_SHELL=0
STAGED_CORE=0
stage_if_changed marspot-shell && STAGED_SHELL=1 || true
stage_if_changed marspot-core  && STAGED_CORE=1  || true

# shelld: report drift, never push.
if [[ -x "$TARGET/marspot-shelld" ]]; then
  ref="$(running_equivalent marspot-shelld)"
  if [[ -f "$ref" ]] && ! cmp -s "$TARGET/marspot-shelld" "$ref"; then
    echo "    marspot-shelld differs — NOT pushed (kills sessions)."
    echo "    apply explicitly: cp target/release/marspot-shelld \\"
    echo "      $TREE/pending/ && bin/install-shelld.sh --apply-pending"
  fi
fi

if (( STAGED_SHELL == 0 && STAGED_CORE == 0 )); then
  echo "==> nothing to push — running Marspot already matches the build"
  exit 0
fi

if ! pgrep -f 'marspot-shell$' >/dev/null 2>&1; then
  echo "==> no running marspot-shell — staged only; next launch picks it up"
  exit 0
fi

# Trigger until pending/ drains.  A staged shell consumes the first
# SIGUSR1 (self-exec); the fresh shell needs a beat before the next
# trigger applies the core.  The supervisor also (correctly) refuses
# triggers while a previous update is still inside its 30 s
# probation, so we keep re-triggering every 3 s up to 60 s instead
# of trusting a single shot.
echo "==> triggering running shell"
DEADLINE=$(( $(date +%s) + 60 ))
NEXT_TRIGGER=0
while :; do
  pending_left=0
  (( STAGED_SHELL )) && [[ -f "$TREE/pending/marspot-shell" ]] && pending_left=1
  (( STAGED_CORE ))  && [[ -f "$TREE/pending/marspot-core"  ]] && pending_left=1
  (( pending_left == 0 )) && break
  now=$(date +%s)
  if (( now >= DEADLINE )); then
    echo "WARN: pending/ not consumed within 60 s — check 'marspot-shell --status'" >&2
    exit 1
  fi
  if (( now >= NEXT_TRIGGER )); then
    "$TARGET/marspot-shell" --trigger >/dev/null 2>&1 || true
    NEXT_TRIGGER=$(( now + 3 ))
  fi
  sleep 0.5
done
echo "==> pushed.  recent supervisor events:"
tail -4 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'

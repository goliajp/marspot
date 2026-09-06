#!/usr/bin/env bash
#
# The bundle lander must never move the app BACKWARDS.
#
# A changed L1 cannot be written into the bundle while its own process
# runs (AMFI kills a process whose on-disk CDHash stops matching), so
# install-local arms a one-shot lander that writes it on the next full
# quit.  The hand-made version of that mechanism kept its staged copy
# after landing and re-landed it at every login: on 2026-09-07 a reboot
# silently reverted the user's bundle to binaries from the previous
# morning.  An updater that can regress is worse than none.
#
# The invariant that replaces it: the stage is valid ONLY while it is
# byte-identical to binaries/current/, which every install refreshes to
# the newest build.  This drives the generated script against a sandbox
# and asserts it in both directions, plus that landing is one-shot.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SB="$(mktemp -d)"; trap 'rm -rf "$SB"' EXIT
fail=0
ok()   { echo "  ✓ $1"; }
bad()  { echo "  ✗ $1"; fail=1; }

# Extract the lander exactly as install-local.sh writes it, so the test
# reads the shipped text and not a copy that can drift from it.
awk '/^  cat > "\$LANDER_DIR\/land.sh" <<.LANDER.$/{f=1;next} /^LANDER$/{f=0} f' \
  "$ROOT/bin/install-local.sh" > "$SB/land.sh"
[[ -s "$SB/land.sh" ]] || { echo "could not extract land.sh from install-local.sh"; exit 1; }
chmod +x "$SB/land.sh"

export MARSPOT_STATE_DIR="$SB/state"
export MARSPOT_APP="$SB/Marspot.app"
export MARSPOT_LANDER_PLIST="$SB/agent.plist"
# The real app is running while this test runs; point the "wait for it
# to quit" check at a name that is definitely not.
export MARSPOT_LANDER_PROCESS="marspot-lander-test-no-such-process"
STAGE="$MARSPOT_STATE_DIR/pending-bundle"
CURRENT="$MARSPOT_STATE_DIR/binaries/current"
MACOS="$MARSPOT_APP/Contents/MacOS"

seed() { # seed <stage-content> <current-content> <bundle-content>
  rm -rf "$MARSPOT_STATE_DIR" "$MARSPOT_APP"
  mkdir -p "$STAGE" "$CURRENT" "$MACOS"
  for b in marspot-shell marspot-core marspot-session; do
    printf '%s' "$1" > "$STAGE/$b";   chmod +x "$STAGE/$b"
    printf '%s' "$2" > "$CURRENT/$b"
    printf '%s' "$3" > "$MACOS/$b"
  done
  # A version probe the script can actually run.
  printf '#!/bin/sh\necho "marspot-shell %s"\n' "$1" > "$MACOS/marspot-shell.probe"
  : > "$MARSPOT_LANDER_PLIST"
}

echo "== 1. a stage that no longer matches binaries/current must not land =="
seed NEW-BUT-SUPERSEDED NEWEST BUNDLE-HAS-NEWEST
"$SB/land.sh" >/dev/null 2>&1
[[ "$(cat "$MACOS/marspot-shell")" == BUNDLE-HAS-NEWEST ]] \
  && ok "the bundle was left alone" || bad "the bundle was overwritten by a stale stage"
[[ ! -e "$STAGE/marspot-shell" ]] \
  && ok "the stale stage deleted itself" || bad "the stale stage is still armed"
[[ ! -e "$MARSPOT_LANDER_PLIST" ]] \
  && ok "the LaunchAgent was removed" || bad "the LaunchAgent is still installed"
grep -q "stale" "$STAGE/land.log" 2>/dev/null \
  && ok "it said why" || bad "no reason recorded in land.log"

echo "== 2. a stage that matches binaries/current lands, once =="
seed NEWEST NEWEST OLD-BUNDLE
"$SB/land.sh" >/dev/null 2>&1
[[ "$(cat "$MACOS/marspot-shell")" == NEWEST ]] \
  && ok "the bundle was updated" || bad "the bundle was not updated"
[[ ! -e "$STAGE/marspot-shell" ]] \
  && ok "the stage cleared itself after landing" || bad "the stage would land again"
[[ ! -e "$MARSPOT_LANDER_PLIST" ]] \
  && ok "the LaunchAgent was removed" || bad "the LaunchAgent would fire again"

echo "== 3. running it a second time is a no-op, not a regression =="
BEFORE="$(cat "$MACOS/marspot-shell")"
"$SB/land.sh" >/dev/null 2>&1
[[ "$(cat "$MACOS/marspot-shell")" == "$BEFORE" ]] \
  && ok "the bundle is unchanged" || bad "a second run changed the bundle"

if (( fail )); then echo "FAIL"; exit 1; fi
echo "PASS"

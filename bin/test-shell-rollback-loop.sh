#!/usr/bin/env bash
#
# Shell crash-loop auto-rollback test.  Stages a broken
# `binaries/current/marspot-shell` (exits immediately) and launches
# the bundle binary repeatedly — the redirect-and-die cycle the
# crash-loop guard exists to break.  Verifies that on the 3rd launch
# inside the 60 s window the bundle binary:
#
#   - quarantines the broken current/ shell,
#   - restores prev/ into current/ (case A) or leaves current/ empty
#     and runs as itself (case B, no prev),
#   - logs SHELL_AUTO_ROLLBACK either way.
#
# Run after `cargo build --release`.  Exits 0 on success.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SHELL_BIN="$ROOT/target/release/marspot-shell"
SUP_LOG="$HOME/Library/Logs/Marspot/supervisor.log"
TREE="$HOME/Library/Caches/marspot/binaries"
LAUNCH_LOG="$HOME/Library/Caches/marspot/shell_launches.tsv"
MARKER=/tmp/marspot-rollback-loop-marker

fail() {
  echo "FAIL: $*"
  echo "  (last 25 supervisor events):"
  tail -25 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'
  echo "  (launch journal):"
  tail -10 "$LAUNCH_LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() {
  pkill -9 -f '/marspot-shell( |$)' >/dev/null 2>&1 || true
  pkill -9 -f '/marspot-core( |$)'  >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Broken shell: dies instantly, the way a bad self-update would.
stage_broken_current() {
  mkdir -p "$TREE/current"
  cat >"$TREE/current/marspot-shell" <<'BROKE'
#!/bin/bash
exit 1
BROKE
  chmod 0755 "$TREE/current/marspot-shell"
}

# Known-good "previous" shell: proves rollback exec'd it by writing
# a marker, then exits cleanly.
stage_good_prev() {
  mkdir -p "$TREE/prev"
  cat >"$TREE/prev/marspot-shell" <<PREV
#!/bin/bash
touch "$MARKER"
exit 0
PREV
  chmod 0755 "$TREE/prev/marspot-shell"
}

reset() {
  cleanup
  rm -rf "$TREE" "$LAUNCH_LOG" "$MARKER"
  : > "$SUP_LOG" 2>/dev/null || true
}

if [[ ! -x "$SHELL_BIN" ]]; then
  ( cd "$ROOT" && cargo build --release --bin marspot-shell 2>&1 | tail -3 )
fi
[[ -x "$SHELL_BIN" ]] || fail "no $SHELL_BIN after build"

# --- Case A: crash loop with a prev/ to fall back to ----------------
reset
stage_broken_current
stage_good_prev

# Launches 1 + 2: redirect into the broken script, which exits 1.
"$SHELL_BIN" >/dev/null 2>&1
"$SHELL_BIN" >/dev/null 2>&1
[[ -f "$MARKER" ]] && fail "prev/ ran before the loop threshold was reached"

# Launch 3: threshold hit — rollback, then exec into restored prev/.
"$SHELL_BIN" >/dev/null 2>&1

grep -q 'SHELL_AUTO_ROLLBACK.*restored prev/' "$SUP_LOG" \
  || fail "case A: SHELL_AUTO_ROLLBACK (restored prev/) not logged"
[[ -f "$TREE/quarantine/marspot-shell" ]] \
  || fail "case A: broken shell not quarantined"
grep -q 'exit 1' "$TREE/quarantine/marspot-shell" \
  || fail "case A: quarantine/ holds the wrong file"
[[ -f "$TREE/current/marspot-shell" ]] \
  || fail "case A: current/ empty after rollback despite prev/"
grep -q "$MARKER" "$TREE/current/marspot-shell" \
  || fail "case A: current/ is not the restored prev script"
[[ -f "$MARKER" ]] \
  || fail "case A: restored prev shell never exec'd (no marker)"
echo "[1/2] case A OK — quarantined broken current/, restored + exec'd prev/"

# --- Case B: crash loop with no prev/ -------------------------------
reset
stage_broken_current

"$SHELL_BIN" >/dev/null 2>&1
"$SHELL_BIN" >/dev/null 2>&1
# Launch 3 continues as the real bundle shell (GUI) — background it.
nohup "$SHELL_BIN" >/dev/null 2>&1 < /dev/null &
disown

START=$(date +%s)
until grep -q 'SHELL_AUTO_ROLLBACK.*no prev/' "$SUP_LOG" 2>/dev/null; do
  if (( $(date +%s) - START > 10 )); then
    fail "case B: SHELL_AUTO_ROLLBACK (no prev/) not logged within 10 s"
  fi
  sleep 0.5
done
[[ -f "$TREE/quarantine/marspot-shell" ]] \
  || fail "case B: broken shell not quarantined"
[[ -e "$TREE/current/marspot-shell" ]] \
  && fail "case B: current/ still populated — bundle fallback impossible"
echo "[2/2] case B OK — quarantined broken current/, bundle binary ran as itself"

cleanup
rm -f "$MARKER"
echo "ALL PASS"

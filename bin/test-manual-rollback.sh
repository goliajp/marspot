#!/usr/bin/env bash
#
# Manual-rollback CLI test — `marspot-shell --rollback-shell` and
# `--rollback-core`.  These are the operator's escape hatch when a
# silently-updated binary is bad but hasn't crash-looped into an
# automatic rollback: quarantine `binaries/current/<layer>`, restore
# `binaries/prev/<layer>`, log MANUAL_ROLLBACK, exit 0.  The
# BinaryTree::rollback_to_prev primitive has unit tests, but the CLI
# command path (dispatch BEFORE the current/ redirect, exit codes,
# logging, the no-prev fallback) had none.
#
# rollback_to_prev only renames files — it never executes them — so the
# slot contents here are plain marker strings, not real binaries.  Fast:
# no updater stagger, no network.  Run after `cargo build --release`.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
SHELL_BIN="$ROOT/target/release/marspot-shell"
SUP_LOG="$MARSPOT_STATE_DIR/logs/supervisor.log"
TREE="$MARSPOT_STATE_DIR/binaries"

fail() {
  echo "FAIL: $*"
  echo "  (last 10 supervisor events):"
  tail -10 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

# Seed current/ + (optionally) prev/ for one layer with marker content.
# $1=layer (shell|core), $2=prev content or empty string for no-prev.
seed_layer() {
  local layer="$1" prev_content="$2"
  local bin="marspot-$layer"
  rm -rf "$TREE"
  mkdir -p "$TREE/current"
  printf 'CURRENT-BAD-%s' "$layer" > "$TREE/current/$bin"
  if [[ -n "$prev_content" ]]; then
    mkdir -p "$TREE/prev"
    printf '%s' "$prev_content" > "$TREE/prev/$bin"
  fi
}

content() { cat "$1" 2>/dev/null; }

dev_kill_shell_core
mkdir -p "$(dirname "$SUP_LOG")"

# --- Case 1+2: rollback-shell / rollback-core with a prev/ present ----
for layer in shell core; do
  bin="marspot-$layer"
  good="PREV-GOOD-$layer"
  seed_layer "$layer" "$good"
  : > "$SUP_LOG" 2>/dev/null || true

  "$SHELL_BIN" "--rollback-$layer" >/tmp/marspot-rollback-$layer.out 2>&1
  rc=$?
  [[ $rc -eq 0 ]] || fail "[$layer] --rollback-$layer exited $rc (expected 0)"

  # current/ now holds the restored prev content.
  [[ "$(content "$TREE/current/$bin")" == "$good" ]] \
    || fail "[$layer] current/$bin is not the restored prev/ content"
  # The bad binary was quarantined, not deleted.
  [[ "$(content "$TREE/quarantine/$bin")" == "CURRENT-BAD-$layer" ]] \
    || fail "[$layer] bad binary was not quarantined"
  # prev/ was consumed (renamed into current/).
  [[ ! -e "$TREE/prev/$bin" ]] \
    || fail "[$layer] prev/$bin still present after rollback"
  # MANUAL_ROLLBACK logged.
  grep -q "MANUAL_ROLLBACK.*$layer" "$SUP_LOG" 2>/dev/null \
    || fail "[$layer] no MANUAL_ROLLBACK in supervisor.log"
  echo "[ok] --rollback-$layer: prev/ restored, bad quarantined, logged"
done

# --- Case 3: rollback with NO prev/ → quarantine + bundle fallback ----
# Must still exit 0 (Ok(false) is not an error) and leave current/ empty
# so the next launch falls back to the bundle binary.
seed_layer shell ""
: > "$SUP_LOG" 2>/dev/null || true
"$SHELL_BIN" --rollback-shell >/tmp/marspot-rollback-noprev.out 2>&1
rc=$?
[[ $rc -eq 0 ]] || fail "[no-prev] exited $rc (expected 0 — empty prev is not an error)"
[[ ! -e "$TREE/current/marspot-shell" ]] \
  || fail "[no-prev] current/marspot-shell still present (should be quarantined away)"
[[ "$(content "$TREE/quarantine/marspot-shell")" == "CURRENT-BAD-shell" ]] \
  || fail "[no-prev] bad binary not quarantined"
grep -q "MANUAL_ROLLBACK.*bundle fallback" "$SUP_LOG" 2>/dev/null \
  || fail "[no-prev] no bundle-fallback MANUAL_ROLLBACK logged"
echo "[ok] --rollback-shell (no prev): quarantined, current/ empty, bundle fallback logged"

rm -rf "$TREE"
echo "ALL PASS — manual rollback CLI quarantines current/, restores prev/, handles no-prev"

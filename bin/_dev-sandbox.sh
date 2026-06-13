#!/usr/bin/env bash
# _dev-sandbox.sh — sourced by every dev / test script so they run in
# an isolated MARSPOT_STATE_DIR with their own shelld, and NEVER touch
# the installed app (~/.local/Marspot.app + ~/Library/Caches/marspot).
#
# Source it after defining $ROOT:
#     source "$ROOT/bin/_dev-sandbox.sh"
#
# Guarantees:
#   - MARSPOT_STATE_DIR points at a sandbox (default /tmp/marspot-dev),
#     exported so every spawned shell/core/shelld uses it via
#     marspot::paths.  The installed instance is on the DEFAULT state
#     dir, so the two never share a socket, binary tree, or log.
#   - dev_kill_shell_core only matches sandbox / target-release argv —
#     it can't reach the bundle-path production processes.
#   - dev_wipe_state removes the SANDBOX tree only.

# Sandbox state dir.  /tmp keeps the unix socket path short (macOS caps
# sun_path at 104 bytes) and survives nothing — a fresh box is fine.
export MARSPOT_STATE_DIR="${MARSPOT_STATE_DIR:-/tmp/marspot-dev}"

DEV_TARGET="$ROOT/target/release"
DEV_SHELLD="$DEV_TARGET/marspot-shelld"
DEV_SOCK="$MARSPOT_STATE_DIR/shelld.sock"

# Kill only sandbox / dev-build shell+core — paths the installed app
# never uses.  Leaves the sandbox shelld (shared across a suite) alone.
#
# The trailing `( |$)` anchor is load-bearing: `marspot-shell` is a
# prefix of `marspot-shell`d and `marspot-core` of `marspot-core`shim,
# so an un-anchored `pkill -f .../marspot-shell` would ALSO kill the
# sandbox shelld (and `.../marspot-core` the coreshim). That stayed
# invisible for as long as the clients hard-coded the production socket
# — killing the sandbox daemon was harmless because nothing connected
# to it — and surfaced the moment core started honouring
# MARSPOT_STATE_DIR. The anchor matches the binary at end-of-argv or
# followed by a space (its CLI args), never the `d`/`shim` suffix.
dev_kill_shell_core() {
  pkill -9 -f "$DEV_TARGET/marspot-shell( |\$)" >/dev/null 2>&1 || true
  pkill -9 -f "$DEV_TARGET/marspot-core( |\$)"  >/dev/null 2>&1 || true
  pkill -9 -f "$MARSPOT_STATE_DIR/binaries/.*/marspot-shell( |\$)" >/dev/null 2>&1 || true
  pkill -9 -f "$MARSPOT_STATE_DIR/binaries/.*/marspot-core( |\$)"  >/dev/null 2>&1 || true
}

# Wipe the sandbox binary tree + supervisor log (NOT sessions — a
# running sandbox shelld owns those).  Sandbox-scoped: the production
# tree under ~/Library/Caches/marspot is never named here.
dev_wipe_state() {
  rm -rf "$MARSPOT_STATE_DIR/binaries" "$MARSPOT_STATE_DIR/shell_launches.tsv"
  : > "$MARSPOT_STATE_DIR/logs/supervisor.log" 2>/dev/null || true
}

# Bring up a sandbox shelld if one isn't already listening.  Idempotent
# — the suite shares one across all test scripts.  Builds on demand.
dev_ensure_shelld() {
  if pgrep -f "$DEV_SHELLD" >/dev/null 2>&1; then
    return 0
  fi
  if [[ ! -x "$DEV_SHELLD" ]]; then
    ( cd "$ROOT" && cargo build --release --bin marspot-shelld 2>&1 | tail -2 )
  fi
  mkdir -p "$MARSPOT_STATE_DIR"
  nohup "$DEV_SHELLD" >"$MARSPOT_STATE_DIR/shelld.out" 2>&1 < /dev/null &
  disown
  for _ in $(seq 1 50); do
    [[ -S "$DEV_SOCK" ]] && return 0
    sleep 0.1
  done
  echo "dev-sandbox: shelld did not create $DEV_SOCK within 5s" >&2
  return 1
}

# Stop the sandbox shelld (suite teardown).  Scoped to the dev build.
dev_stop_shelld() {
  pkill -9 -f "$DEV_SHELLD" >/dev/null 2>&1 || true
}

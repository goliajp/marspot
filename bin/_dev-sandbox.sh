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
# prefix of `marspot-shell`d, so an un-anchored `pkill -f
# .../marspot-shell` would ALSO kill the sandbox shelld (and any
# future `marspot-core`-prefixed sibling). That stayed
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

# Wipe the sandbox binary tree + structured marspot.log (NOT sessions —
# a running sandbox shelld owns those).  Sandbox-scoped: the production
# tree under ~/Library/Caches/marspot is never named here.
#
# `rm -f` (not `: > …`) because long-running processes hold marspot.log
# open via logx Sinks — truncating under them races the next write and
# can leave a sparse hole.  rm + next-open-recreates is the safe path.
dev_wipe_state() {
  rm -rf "$MARSPOT_STATE_DIR/binaries" "$MARSPOT_STATE_DIR/shell_launches.tsv"
  rm -f "$MARSPOT_STATE_DIR/logs/marspot.log" 2>/dev/null || true
}

# RFC-003 Phase 6: L4 shelld retired.  These helpers stay as no-ops
# for backwards compat with any test script still calling them.
dev_ensure_shelld() { return 0; }
dev_stop_shelld() { return 0; }

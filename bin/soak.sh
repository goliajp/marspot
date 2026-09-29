#!/usr/bin/env bash
set -euo pipefail

# bin/soak.sh — run the long-running stability assertions.
#
# Three tiers:
#
#   bin/soak.sh --smoke
#     Just the pty spawn-loop tests (fd / child / RSS leak over
#     1000 cycles).  ~30 s on a warm cache, ~2 min cold.  The pre-push
#     tier: catches OS-resource leaks every push without slowing the
#     gate by minutes.
#
#   bin/soak.sh   (no args)
#     All 5 ignored soak tests:
#       pty:      no-fd-leak, no-unreaped-children, no-memory-growth
#                 (1000 spawn-drop cycles each)
#       terminal: scrollback-bounded mem + disk variants
#                 (10 M scrolled lines each; ~5-10 min total)
#     The pre-release / weekly tier.
#
#   bin/soak.sh <filter>
#     Pass-through.  e.g. `bin/soak.sh soak_scrollback` runs just one.
#
# Release profile so timing / memory characteristics match production.
# --test-threads=1 because soak tests measure whole-process resources
# (fd count, RSS) and would race each other.
#
# End-to-end process soak (mcli + glyph atlas + Metal + AppKit) lives
# elsewhere: `bin/scenarios/idle-9x.sh marspot <out> --extended` (30 min
# CPU + RSS drift) and the `active-9x-soak` scenario in
# `bin/bench-run.sh --extended`.  Those need a GUI session and are
# manual / nightly.  This script stays headless cargo-test territory.

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

case "${1:-}" in
  --help|-h)
    sed -n '2,32p' "$0"; exit 0 ;;
  --smoke)
    shift
    exec cargo test --release -- --ignored --test-threads=1 pty::tests::soak_ "$@" ;;
  *)
    exec cargo test --release -- --ignored --test-threads=1 "$@" ;;
esac

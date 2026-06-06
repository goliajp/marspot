#!/usr/bin/env bash
# bin/miri.sh — run Miri on the pure-Rust subset of the lib.
#
# Why: catches UB and pointer errors that ordinary tests miss. The
# project has a "cannot get slower the longer it runs" commitment;
# leaks and stale references are exactly the class of bug that
# silently degrades long-running terminals.
#
# Miri cannot execute most of marspot's FFI (objc2, libc::madvise,
# pthread, mmap, Metal). The default filter therefore covers only
# the four modules that are pure Rust end-to-end:
#
#   parser   — VT/xterm escape-sequence state machine
#   grid     — cell grid + scrollback ring
#   tmux     — tmux control-mode protocol decoder
#   input    — keyboard event → byte mapping
#
# Override with MIRI_MODULES, e.g.:
#   MIRI_MODULES="parser:: grid::" bin/miri.sh
#
# Modules that intentionally cannot run under Miri today:
#   scrollback  — libc::madvise on disk-backed mmap
#   pty         — forkpty / ioctl / setsid
#   terminal    — pulls in scrollback transitively
#   render*, font_cache, glyph_atlas, app, layout, session — Metal /
#                 CoreText / AppKit FFI
#
# These are covered by integration tests against a real process; Miri
# only protects the pure-Rust core, where it can.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

MIRI_MODULES="${MIRI_MODULES:-parser:: grid:: tmux:: input::}"

if ! rustup toolchain list | grep -q '^nightly-'; then
  echo "nightly toolchain missing: rustup toolchain install nightly --component miri" >&2
  exit 2
fi
if ! rustup component list --toolchain nightly --installed 2>/dev/null | grep -q '^miri-'; then
  echo "miri component missing: rustup component add miri --toolchain nightly" >&2
  exit 2
fi

# shellcheck disable=SC2086
exec cargo +nightly miri test --lib -- $MIRI_MODULES

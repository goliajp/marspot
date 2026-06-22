#!/usr/bin/env bash
# bin/test.sh — run the lib test suite via cargo-nextest.
#
# Why nextest over `cargo test`: per-test process isolation surfaces
# panics with their owning test name, parallel scheduling cuts wall
# clock noticeably at ~140 tests, and the failure output is one line
# per failure instead of a wall of output to grep through.
#
# Passes through any extra args:
#   bin/test.sh parser::
#   bin/test.sh --failure-output immediate

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# --workspace so the marspot-term crate's tests (the terminal engine —
# parser/grid/terminal/scrollback/… extracted for target #4) run too,
# not just the GUI crate's handful of lib tests.
#
# `--all-targets` (instead of `--lib`) includes integration tests —
# in particular `crates/marspot-term/tests/scrollback_display.rs`, the
# user-perspective gate that catches scrollback display bugs (blank
# pushes leaking into scrollback, torn-write history loss, scroll
# cap mismatches, resize content corruption) BEFORE they ship.
exec cargo nextest run --workspace --all-targets "$@"

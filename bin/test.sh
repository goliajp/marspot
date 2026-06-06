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
exec cargo nextest run --lib "$@"

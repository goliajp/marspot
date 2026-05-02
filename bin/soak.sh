#!/usr/bin/env bash
set -euo pipefail

# Run the soak (long-running stability) test suite.  These are gated by
# #[ignore] so they don't slow down `cargo test`; this script opts in.
#
# Release profile so the timing/memory characteristics match production.
# --test-threads=1 because soak tests measure whole-process resources
# (fd count, RSS) and would race each other.

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

cargo test --release -- --ignored --test-threads=1 "$@"

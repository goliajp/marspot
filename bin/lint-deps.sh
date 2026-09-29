#!/usr/bin/env bash
# bin/lint-deps.sh — dependency hygiene gate.
#
# Three independent checks, sequenced:
#   1. cargo audit       — RustSec advisories on Cargo.lock
#   2. cargo deny check  — licenses + bans + sources (advisories
#                          intentionally skipped, audit covers them)
#   3. cargo machete     — unused dependencies declared in Cargo.toml
#
# Each tool prints its own diagnostics. The gate fails if any of the
# three exits non-zero. Intended to run pre-push alongside bin/bench.sh.

set -uo pipefail   # not -e: we want to keep going past individual failures

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

fail=0
run() {
  local label=$1; shift
  echo "==> $label"
  if "$@"; then
    echo "    ✓ $label"
  else
    echo "    ✗ $label FAILED"
    fail=1
  fi
}

run "cargo audit"       cargo audit --quiet
run "cargo deny check"  cargo deny --log-level warn check licenses bans sources
run "cargo machete"     cargo machete

echo
if (( fail )); then
  echo "GATE FAILED"
  exit 1
fi
echo "GATE PASSED"

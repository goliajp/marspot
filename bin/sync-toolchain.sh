#!/usr/bin/env bash
# bin/sync-toolchain.sh — install the canonical dev tooling set,
# idempotently, on the current (or a remote) host.
#
# Why: bench-remote / lint-deps / fuzz / miri / nextest / samply all
# assume specific binaries exist. When onboarding a new bench host or
# rebuilding yours, this is one command instead of a chase.
#
# Usage:
#   bin/sync-toolchain.sh                # this host
#   HOST=mini bin/sync-toolchain.sh      # over ssh

set -euo pipefail

# Canonical tool set. Pinning to versions is intentionally avoided —
# `cargo install <name>` resolves to the latest compatible release;
# the dep-hygiene gate (cargo deny / audit) catches anything weird.
TOOLS=(
  # bench harness
  hyperfine
  cargo-sweep
  # dep hygiene
  cargo-audit cargo-deny cargo-machete cargo-bloat
  # correctness
  cargo-fuzz cargo-llvm-cov
  # tests + profiling
  cargo-nextest samply flamegraph    # `flamegraph` crate installs the `cargo-flamegraph` subcommand
  # release / maintenance
  cargo-outdated cargo-edit
)

HOST="${HOST:-}"
ssh_or_local() {
  if [[ -n "$HOST" ]]; then
    ssh "$HOST" "$@"
  else
    bash -c "$*"
  fi
}

label="$HOST"
[[ -z "$label" ]] && label="(local)"

echo "==> [$label] ensuring nightly toolchain + miri"
ssh_or_local "rustup toolchain install nightly --component miri --no-self-update 2>&1 | tail -2"

echo "==> [$label] probing installed cargo binaries"
installed="$(ssh_or_local "cargo install --list 2>/dev/null" \
  | awk '/^[a-zA-Z0-9_-]+ v[0-9]/ {print $1}')"

missing=()
for t in "${TOOLS[@]}"; do
  if ! grep -qx "$t" <<<"$installed"; then
    missing+=("$t")
  fi
done

if (( ${#missing[@]} == 0 )); then
  echo "==> [$label] all ${#TOOLS[@]} tools present"
  exit 0
fi

echo "==> [$label] installing ${#missing[@]} missing: ${missing[*]}"
ssh_or_local "cargo install ${missing[*]}"

echo "==> [$label] done"

#!/usr/bin/env bash
# bin/_madcawl-host.sh — print the path of a `madcawl-shell` built from
# the madcawl commit Cargo.toml pins `madcawl-wire` to, building it the
# first time under target/madcawl/<commit>/.
#
# The web pane's window suite needs madcawl's host on the runner, and
# nothing of the developer's machine is used there: the three public
# repositories madcawl builds from (it reaches alt and torajs by
# relative path) are fetched at fixed commits, side by side.  Build
# output goes to stderr; stdout is the path alone.  MADCAWL_SHELL names a
# host built elsewhere instead.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MADCAWL_REV="$(sed -n 's/^madcawl-wire = .*rev = "\([0-9a-f]\{40\}\)".*/\1/p' "$ROOT/Cargo.toml")"
[[ -n $MADCAWL_REV ]] || { echo "no madcawl-wire rev in Cargo.toml" >&2; exit 2; }
ALT_REV=8ced5a0c9b64c9e35fa155d80446132476dd1493
TORAJS_REV=096d113ddec5fc4106624d07ec2a1f96daea227b

# a host built elsewhere from the same commit, shipped to the runner
if [[ -n ${MADCAWL_SHELL:-} ]]; then
  [[ -x $MADCAWL_SHELL ]] || { echo "MADCAWL_SHELL=$MADCAWL_SHELL is not an executable" >&2; exit 2; }
  echo "$MADCAWL_SHELL"
  exit 0
fi
BASE="$ROOT/target/madcawl/$MADCAWL_REV"
EXE="$BASE/target/release/madcawl-shell"
if [[ -x $EXE ]]; then
  echo "$EXE"
  exit 0
fi

fetch() { # name, commit
  local dir="$BASE/$1"
  if [[ ! -d $dir/.git ]]; then
    mkdir -p "$dir"
    git -C "$dir" init -q
  fi
  git -C "$dir" fetch -q --depth 1 "https://github.com/goliajp/$1" "$2"
  git -C "$dir" checkout -q --detach FETCH_HEAD
}

{
  fetch madcawl "$MADCAWL_REV"
  fetch alt "$ALT_REV"
  fetch torajs "$TORAJS_REV"
  command -v cargo >/dev/null || source "$HOME/.cargo/env"
  # the host's speed is not under test: no whole-program optimisation
  cd "$BASE/madcawl"
  CARGO_TARGET_DIR="$BASE/target" CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
    cargo build --release -p madcawl-shell
} >&2
echo "$EXE"

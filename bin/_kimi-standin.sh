# shellcheck shell=bash
# Sourced by the suites that run a Kimi Code: the stand-in in
# bin/kimi-standin instead of the real CLI, which never runs on a
# runner.  Do not execute.

KIMI_STANDIN_DIR="$ROOT/bin/kimi-standin"

# kimi_standin_build OUT: the compiled `kimi` at OUT
kimi_standin_build() {
  local out=${1:?out} py
  py="$(command -v python3)" || { echo "no python3 on PATH" >&2; return 1; }
  mkdir -p "$(dirname "$out")"
  cc -O1 -DNAME='"kimi"' -DPYTHON="\"$py\"" -DSTANDIN="\"$KIMI_STANDIN_DIR/standin.py\"" \
    -o "$out" "$ROOT/bin/codex-standin/launcher.c"
}

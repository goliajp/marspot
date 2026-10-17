#!/usr/bin/env bash
# _dev-sandbox.sh — sourced by every dev / test script so they run in
# an isolated MARSPOT_STATE_DIR, and NEVER touch the installed app
# (~/.local/Marspot.app + its default state dir).
#
# Source it after defining $ROOT:
#     source "$ROOT/bin/_dev-sandbox.sh"
#
# Guarantees:
#   - MARSPOT_STATE_DIR points at a sandbox (default /tmp/marspot-dev),
#     exported so every spawned shell/core/session uses it via
#     marspot::paths.  The installed instance is on the DEFAULT state
#     dir, so the two never share a socket, binary tree, or log.
#   - dev_kill_shell_core stops only processes this sandbox recorded
#     (shell.pid, the session registry, dev_record_pid) and what they
#     started, and only while they still run with this state dir.
#   - dev_wipe_state removes the SANDBOX tree only.

# Sandbox state dir.  /tmp keeps the unix socket path short (macOS caps
# sun_path at 104 bytes) and survives nothing — a fresh box is fine.
# Callers set ROOT before sourcing this; define it when they have not, so
# a function here cannot build a path out of an empty variable.  Unset, it
# expanded to an absolute-looking `/crates/...` that reads as a missing
# file rather than a missing variable.
ROOT="${ROOT:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"

export MARSPOT_STATE_DIR="${MARSPOT_STATE_DIR:-/tmp/marspot-dev}"

# A sandbox runs under the developer's own HOME, so anything the app
# writes outside its state dir lands in the developer's real
# environment.  The claudecode plugin keeps a status-line hook in
# `~/.claude/settings.json`, installing it while it runs and removing
# it on the way out — and a sandbox doing that fights the installed app
# over the user's own config, once a minute, removing the hook the real
# app just put back (2026-09-28, seen live).  This says "you are not
# the user's marspot"; what reads it refuses to write outside the
# sandbox.
export MARSPOT_DEV_SANDBOX=1

DEV_TARGET="$ROOT/target/release"

# Write down a pid this script launched (a shell, a standalone marspot,
# an mcli), so dev_kill_shell_core can find it again.  shell.pid alone
# is not enough: each shell overwrites it, and an earlier one that is
# still running would be lost.
dev_record_pid() {
  mkdir -p "$MARSPOT_STATE_DIR"
  echo "$1" >> "$MARSPOT_STATE_DIR/dev.pids"
}

# True when `pid` runs one of our binaries with this sandbox's state
# dir.  A pid read back from a file can belong to anything by now; the
# installed app has no MARSPOT_STATE_DIR, so it never passes.
_dev_is_ours() {
  local p=$1 comm
  [[ $p =~ ^[0-9]+$ ]] || return 1
  comm=$(ps -o comm= -p "$p" 2>/dev/null) || return 1
  case "${comm##*/}" in
    marspot|marspot-shell|marspot-core|marspot-session|mcli) ;;
    *) return 1 ;;
  esac
  ps -ww -E -o command= -p "$p" 2>/dev/null | tr ' ' '\n' \
    | grep -qxF "MARSPOT_STATE_DIR=$MARSPOT_STATE_DIR"
}

_dev_descendants() {
  local c
  for c in $(pgrep -P "$1" 2>/dev/null); do
    echo "$c"
    _dev_descendants "$c"
  done
}

# The running shell of this sandbox, by its pid file, or nothing.
dev_shell_pid() {
  local p
  p=$(cat "$MARSPOT_STATE_DIR/shell.pid" 2>/dev/null)
  if _dev_is_ours "$p"; then echo "$p"; fi
  return 0
}

# The shell's core children (normally one), one per line.
dev_core_pids() {
  local s k comm
  s=$(dev_shell_pid)
  [[ -n $s ]] || return 0
  for k in $(pgrep -P "$s" 2>/dev/null); do
    comm=$(ps -o comm= -p "$k" 2>/dev/null) || continue
    if [[ ${comm##*/} == marspot-core ]]; then echo "$k"; fi
  done
  return 0
}

# This sandbox's live L3s, by the session registry, one per line.
dev_session_pids() {
  local e p
  for e in "$MARSPOT_STATE_DIR"/sessions/*/entry.toml; do
    [[ -f $e ]] || continue
    p=$(sed -n 's/^pid = \([0-9][0-9]*\)$/\1/p' "$e")
    if [[ -n $p ]] && _dev_is_ours "$p"; then echo "$p"; fi
  done
  return 0
}

# Every live process this sandbox is responsible for, one per line: the
# recorded roots that are still ours, then everything they started.
dev_sandbox_pids() {
  local sd=$MARSPOT_STATE_DIR p roots=()
  # shell.pid has no trailing newline: read it on its own, or it runs
  # into the first line of dev.pids
  for p in $(dev_shell_pid) $(cat "$sd/dev.pids" 2>/dev/null); do
    if _dev_is_ours "$p"; then roots+=("$p"); fi
  done
  for p in $(dev_session_pids); do roots+=("$p"); done
  for p in ${roots[@]+"${roots[@]}"}; do
    echo "$p"
    _dev_descendants "$p"
  done | awk '!seen[$0]++'
}

# Stop this sandbox's shell, core and sessions, and nothing else.
#
# Shells and cores get TERM first and a few seconds to go.  Sessions get
# KILL, and only after the shell and core are down: an L3 treats TERM as
# "update" when binaries/current/marspot-session differs from its own
# image and execs into it instead of exiting (Amendment 16), and L3s are
# meant to outlive their L1/L2, so a TERM from a dying core would not
# end them either.  The shells inside the sessions go with them.
#
# 2026-07-28: 176 sessions leaked in one evening because scripts killed
# shell and core and then removed sessions/, leaving no registry to find
# the L3s by.  So the registry is read here, before anything is removed,
# and an L3 whose registry entry disappears ends itself.
dev_kill_shell_core() {
  local p comm pids upper=() i
  pids=$(dev_sandbox_pids)
  [[ -n $pids ]] || return 0
  for p in $pids; do
    comm=$(ps -o comm= -p "$p" 2>/dev/null) || continue
    case "${comm##*/}" in
      marspot|marspot-shell|marspot-core|mcli) upper+=("$p") ;;
    esac
  done
  # kill(1) succeeds when any one pid took the signal, so `kill -0` over
  # the list is "some are still alive"
  if (( ${#upper[@]} > 0 )); then
    kill -TERM "${upper[@]}" 2>/dev/null || true
    for ((i = 0; i < 25; i++)); do
      kill -0 "${upper[@]}" 2>/dev/null || break
      sleep 0.2
    done
  fi
  # shellcheck disable=SC2086
  kill -KILL $pids 2>/dev/null || true
  for ((i = 0; i < 25; i++)); do
    # shellcheck disable=SC2086
    kill -0 $pids 2>/dev/null || break
    sleep 0.2
  done
  if [[ -f $MARSPOT_STATE_DIR/dev.pids ]]; then
    : > "$MARSPOT_STATE_DIR/dev.pids"
  fi
  local left=""
  for p in $pids; do
    if kill -0 "$p" 2>/dev/null; then left+=" $p"; fi
  done
  if [[ -n $left ]]; then
    echo "dev_kill_shell_core: still running after KILL:$left" >&2
    return 1
  fi
  return 0
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

# Pay the Gatekeeper assessment on freshly built binaries BEFORE a test
# starts timing anything.
#
# macOS assesses a newly created executable on its first exec, and that
# assessment is unbounded — a concurrent cargo build elsewhere on the
# box can flood `syspolicyd` and push it into the tens of seconds (204 s
# was observed on 2026-07-29).  A suite that runs `cargo build` and then
# gives the app 5 s to hand-shake is timing macOS's scheduler, not the
# app, and fails for reasons that have nothing to do with the change
# under test.
#
# So burn the cost here, outside every stopwatch.  This is the same
# trick the supervisor plays before a swap (`binary_tree::can_start`),
# for the same reason.  Call it after any build step.
# Build one L3 probe from source, or refuse.
#
# Seven of the eleven probes the soak scripts name lost their source in
# the June history rebuild, and every one of them still had a binary from
# mid-June sitting in `target/release/examples/`.  The scripts checked
# `-x` on the path, found those, and ran them -- so for three and a half
# months the L3 soak suite was exercising June's code against today's
# session binary and reporting whatever came out.  One of them still
# tries to connect to shelld, a daemon RFC-003 deleted, which is the
# only reason this was noticeable at all.
#
# So: the source is what is checked, before anything else, and the build's
# exit code is not allowed into a pipe.
dev_require_probe() {
  local name="$1"
  local src="$ROOT/crates/marspot-session/examples/$name.rs"
  if [[ ! -f "$src" ]]; then
    echo "FAIL: probe '$name' has no source at $src" >&2
    echo "      A binary left in target/ is not coverage.  Write the probe" >&2
    echo "      or stop naming it." >&2
    return 1
  fi
  local log
  log=$(cd "$ROOT" && cargo build --release -p marspot-session --example "$name" 2>&1) || {
    echo "FAIL: probe '$name' did not build" >&2
    echo "$log" | tail -20 >&2
    return 1
  }
  return 0
}

# The marspot-session children of a running probe, one per line.
dev_probe_sessions() {
  local k comm
  for k in $(pgrep -P "$1" 2>/dev/null); do
    comm=$(ps -o comm= -p "$k" 2>/dev/null) || continue
    if [[ ${comm##*/} == marspot-session ]]; then echo "$k"; fi
  done
  return 0
}

# Stop whichever of the given session pids are still running, and say
# how many there were.  The pids came from dev_probe_sessions, so each
# is checked to still be a marspot-session before it is signalled.
dev_reap_probe_sessions() {
  local k comm n=0
  for k in "$@"; do
    comm=$(ps -o comm= -p "$k" 2>/dev/null) || continue
    [[ ${comm##*/} == marspot-session ]] || continue
    kill -KILL "$k" 2>/dev/null && n=$((n + 1))
  done
  echo "$n"
}

# Run an L3 probe and make sure the sessions it starts end with it.
#
# The sessions are read off the probe (its children) while it runs, and
# whichever of them outlives it is an orphan: it is killed here, and the
# call returns 3 if the probe itself passed.  Nothing is looked up by
# name or command line, so another checkout's sessions on the same host
# are never counted or touched.  Sampling is fast for the first two
# seconds, when probes spawn, and slow after.
dev_run_probe() {
  "$@" &
  local pid=$! kids=" " k rc n=0 orphans
  while kill -0 "$pid" 2>/dev/null; do
    for k in $(dev_probe_sessions "$pid"); do
      [[ $kids == *" $k "* ]] || kids+="$k "
    done
    if (( n < 40 )); then sleep 0.05; else sleep 0.5; fi
    n=$((n + 1))
  done
  wait "$pid"
  rc=$?
  # shellcheck disable=SC2086
  orphans=$(dev_reap_probe_sessions $kids)
  if (( orphans > 0 )); then
    echo "dev_run_probe: $orphans marspot-session(s) outlived the probe; killed them" >&2
    (( rc != 0 )) || rc=3
  fi
  return "$rc"
}

# Build one binary from source, or refuse.
#
# The companion to `dev_require_probe`, and for the same reason: a path in
# `target/` holding an executable proves only that something was built
# there once.  `marspot-shelld` is the case that made this necessary --
# RFC-003 deleted the L4 daemon in June, `cargo build --bin
# marspot-shelld` now answers "no such target", and a binary from June 17
# sat in `target/release/` where three soak scripts picked it up, ran it,
# and reported ALL PASSED for a layer the product no longer has.
dev_require_bin() {
  local name="$1"
  local log
  log=$(cd "$ROOT" && cargo build --release --bin "$name" 2>&1) || {
    echo "FAIL: '$name' is not a build target any more" >&2
    echo "$log" | tail -10 >&2
    echo "      A binary left in target/ is not the product.  If this" >&2
    echo "      script tests a layer that was removed, delete the script." >&2
    return 1
  }
  return 0
}

dev_warm_binaries() {
  for b in marspot-shell marspot-core marspot-session; do
    [[ -x "$DEV_TARGET/$b" ]] && \
      MARSPOT_NO_REDIRECT=1 "$DEV_TARGET/$b" --version >/dev/null 2>&1
  done
  return 0
}

# RFC-003 Phase 6: L4 shelld retired.  These helpers stay as no-ops
# for backwards compat with any test script still calling them.
dev_ensure_shelld() { return 0; }
dev_stop_shelld() { return 0; }

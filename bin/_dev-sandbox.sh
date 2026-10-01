#!/usr/bin/env bash
# _dev-sandbox.sh — sourced by every dev / test script so they run in
# an isolated MARSPOT_STATE_DIR with their own shelld, and NEVER touch
# the installed app (~/.local/Marspot.app + ~/Library/Caches/marspot).
#
# Source it after defining $ROOT:
#     source "$ROOT/bin/_dev-sandbox.sh"
#
# Guarantees:
#   - MARSPOT_STATE_DIR points at a sandbox (default /tmp/marspot-dev),
#     exported so every spawned shell/core/shelld uses it via
#     marspot::paths.  The installed instance is on the DEFAULT state
#     dir, so the two never share a socket, binary tree, or log.
#   - dev_kill_shell_core only matches sandbox / target-release argv —
#     it can't reach the bundle-path production processes.
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
DEV_SOCK="$MARSPOT_STATE_DIR/shelld.sock"

# Kill only sandbox / dev-build shell+core — paths the installed app
# never uses.  Leaves the sandbox shelld (shared across a suite) alone.
#
# The trailing `( |$)` anchor is load-bearing: `marspot-shell` is a
# prefix of `marspot-shell`d, so an un-anchored `pkill -f
# .../marspot-shell` would ALSO kill the sandbox shelld (and any
# future `marspot-core`-prefixed sibling). That stayed
# invisible for as long as the clients hard-coded the production socket
# — killing the sandbox daemon was harmless because nothing connected
# to it — and surfaced the moment core started honouring
# MARSPOT_STATE_DIR. The anchor matches the binary at end-of-argv or
# followed by a space (its CLI args), never the `d`/`shim` suffix.
dev_kill_shell_core() {
  # Both profiles, and the unified `marspot` name as well as the three
  # suffixed ones.  `DEV_TARGET` alone is `target/release` while
  # bin/run.sh defaults to debug and launches the unified binary, so on
  # a default run this matched nothing at all and the sandbox app
  # stayed up -- the shape the 2026-07-28 note below is about.  The
  # anchor still refuses `marspot-coreshim` and `marspotd`, and nothing
  # here can reach the installed app: it is rooted at this checkout.
  pkill -9 -f "$ROOT/target/(debug|release)/marspot(-shell|-core|-session)?( |\$)" >/dev/null 2>&1 || true
  pkill -9 -f "$MARSPOT_STATE_DIR/binaries/.*/marspot-shell( |\$)" >/dev/null 2>&1 || true
  pkill -9 -f "$MARSPOT_STATE_DIR/binaries/.*/marspot-core( |\$)"  >/dev/null 2>&1 || true
  # 2026-07-28 事故:sessions 也必须收。L3 有意在 L2/L1 死后存活
  # (silent update / 崩溃重连的根基),所以杀了 shell/core 之后它们
  # 全部变成 ppid=1 的孤儿;测试脚本又常在下一轮开跑前 rm -rf 掉
  # sessions/ 注册表 —— 注册表没了,任何 reaper 都再也找不到它们。
  # 当晚 176 个泄漏 session 的主力就是这么来的。两道防线:这里按
  # 沙箱二进制路径直接杀(只匹配 dev/target 与沙箱 binaries 路径,
  # 碰不到安装版),加上 L3 自身的 registry 哨兵(条目消失即自杀)。
  pkill -9 -f "$DEV_TARGET/marspot-session( |\$)" >/dev/null 2>&1 || true
  pkill -9 -f "$MARSPOT_STATE_DIR/binaries/.*/marspot-session( |\$)" >/dev/null 2>&1 || true
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

#!/usr/bin/env bash
#
# 2026-07-28 WindowServer 事故的三道防线,各自验一遍:
#
#   1. 崩溃循环刹车 —— 伪造一份"5 分钟内 5 次启动"的 journal,
#      shell 必须进 safe mode:反复 boot 也不许 spawn 任何新 session
#      (只许 reattach),不许复原额外窗口。
#   2. 沙箱清场 —— dev_kill_shell_core 之后,沙箱里的
#      marspot-session 必须在 10 秒内归零(直接杀 + registry 哨兵
#      双保险)。这就是报告第七节第 2 条验收,按沙箱语义落地。
#   3. registry 哨兵 —— 注册表被删的 session 自行退出(rm -rf
#      sessions/ 这种测试脚本的惯用动作不再制造永生孤儿)。
#
# 全程沙箱,绝不触碰安装版。

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

RUNLOG=/tmp/marspot-test-crash-guard.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"

fail() {
  echo "FAIL: $*"
  tail -30 "$APPLOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}
cleanup() { dev_kill_shell_core; }
trap cleanup EXIT

if [[ ! -x "$SHELL_BIN" || ! -x "$SESSION_BIN" ]]; then
  echo "==> building release binaries"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 )
fi

cleanup
dev_wipe_state
rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
mkdir -p "$MARSPOT_STATE_DIR/logs"
rm -f "$APPLOG"

sandbox_session_count() {
  pgrep -f "$ROOT/target/release/marspot-session( |$)" 2>/dev/null | wc -l | tr -d ' '
}

wait_for() { # tag, seconds
  local tag="$1" secs="$2" i
  for (( i = 0; i < secs * 10; i++ )); do
    grep -q "$tag" "$APPLOG" 2>/dev/null && return 0
    sleep 0.1
  done
  return 1
}

# --- 1. 崩溃循环刹车 --------------------------------------------------
echo "==> phase 1: crash-loop brake (seeded journal → safe mode)"
now=$(python3 -c 'import time; print(time.time())')
python3 - "$MARSPOT_STATE_DIR/shell_launches.tsv" "$now" <<'PY'
import sys
path, now = sys.argv[1], float(sys.argv[2])
# 5 次启动散布在最近 5 分钟 —— 事故当晚的形状。mtime 列各不相同,
# 避免误触发旧的"同一 current/ 二进制"回滚器(那是另一道闸)。
rows = [(now - dt, i) for i, dt in enumerate((240, 180, 120, 70, 1))]
open(path, 'w').write(''.join(f"{t}\t{m}\n" for t, m in rows))
PY

MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null &
disown

wait_for SHELL_SAFE_MODE 30 || fail "seeded crash loop did not enter safe mode"
sleep 4
grep -q "L3_SPAWN\b" "$APPLOG" \
  && fail "safe mode spawned a fresh session — the loop can still multiply"
grep -q "core.boot.safe_mode_vacant" "$APPLOG" \
  || fail "safe mode did not leave the empty slots vacant"
echo "    safe mode: no fresh spawns, slots vacant — OK"

# --- 2. 沙箱清场归零 --------------------------------------------------
echo "==> phase 2: dev_kill_shell_core reaps every sandbox session"
cleanup
rm -f "$MARSPOT_STATE_DIR/shell_launches.tsv" "$APPLOG"
MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null &
disown
wait_for WINDOW_FIRST_FRAME 30 || fail "sandbox app never painted"
sleep 2
before=$(sandbox_session_count)
(( before >= 1 )) || fail "expected live sandbox sessions before the kill, got $before"
dev_kill_shell_core
for i in $(seq 1 20); do
  [[ "$(sandbox_session_count)" == "0" ]] && break
  sleep 0.5
done
after=$(sandbox_session_count)
[[ "$after" == "0" ]] \
  || fail "$after sandbox session(s) survived dev_kill_shell_core (had $before)"
echo "    $before sessions before kill → 0 within 10s — OK"

# --- 3. registry 哨兵 -------------------------------------------------
echo "==> phase 3: deleting the registry kills the orphans"
rm -f "$APPLOG"
MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null &
disown
wait_for WINDOW_FIRST_FRAME 30 || fail "sandbox app never painted (phase 3)"
sleep 2
before=$(sandbox_session_count)
(( before >= 1 )) || fail "no sessions to orphan"
# 只杀 shell/core,把 session 留成孤儿 —— 然后删注册表。
pkill -9 -f "$ROOT/target/release/marspot-shell( |$)" 2>/dev/null || true
pkill -9 -f "$ROOT/target/release/marspot-core( |$)"  2>/dev/null || true
sleep 1
orphans=$(sandbox_session_count)
(( orphans >= 1 )) || fail "orphans already gone before the registry delete?"
rm -rf "$MARSPOT_STATE_DIR/sessions"
# 哨兵每 5 秒查一次;给 12 秒。
for i in $(seq 1 24); do
  [[ "$(sandbox_session_count)" == "0" ]] && break
  sleep 0.5
done
left=$(sandbox_session_count)
[[ "$left" == "0" ]] \
  || fail "$left orphan(s) outlived their deleted registry ($orphans before)"
echo "    $orphans orphans + registry deleted → 0 within 12s — OK"

echo
echo "PASS — crash-loop brake, sandbox reaping, registry deadman all hold."

#!/usr/bin/env bash
#
# 2026-07-28 WindowServer 事故的三道防线,各自验一遍:
#
#   1. 崩溃循环刹车 —— 伪造一份"5 分钟内 5 次启动"的 journal,
#      shell 必须进 safe mode:反复 boot 也不许 spawn 任何新 session
#      (只许 reattach),不许复原额外窗口。
#   2. 沙箱清场 —— dev_kill_shell_core 之后,沙箱记下的 shell、core、
#      marspot-session 和它们的子进程必须在 10 秒内全部退出;同时一个
#      命令行里带着沙箱路径、但沙箱没有记下的旁观进程必须活着(清场只按
#      记下的 pid 杀,不按命令行匹配)。
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

# How many of the given pids are still running.
alive() {
  local p n=0
  for p in "$@"; do
    if kill -0 "$p" 2>/dev/null; then n=$((n + 1)); fi
  done
  echo "$n"
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
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
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
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
disown
wait_for WINDOW_FIRST_FRAME 30 || fail "sandbox app never painted"
sleep 2
l3s=$(dev_session_pids)
everything=$(dev_sandbox_pids)
# shellcheck disable=SC2086
before=$(alive $l3s)
(( before >= 1 )) || fail "expected live sandbox sessions before the kill, got $before"
# A bystander whose command line names this sandbox's binaries and
# state dir.  Cleanup used to select by command line, which would have
# killed it; now it must survive, because nothing recorded it.
/bin/sh -c 'sleep 600; :' "$SESSION_BIN" "$MARSPOT_STATE_DIR/binaries/current/marspot-session" &
decoy=$!
sleep 0.3
decoy_kids=$(pgrep -P "$decoy")
# shellcheck disable=SC2086
echo "    recorded: $(for p in $everything; do ps -o comm= -p "$p" | sed 's|.*/||'; done | sort | uniq -c | awk '{printf "%s×%s ", $2, $1}')"
dev_kill_shell_core
for i in $(seq 1 20); do
  # shellcheck disable=SC2086
  [[ "$(alive $everything)" == "0" ]] && break
  sleep 0.5
done
# shellcheck disable=SC2086
after=$(alive $everything)
decoy_alive=$(alive "$decoy")
# shellcheck disable=SC2086
kill $decoy_kids "$decoy" 2>/dev/null
wait "$decoy" 2>/dev/null
(( decoy_alive == 1 )) || fail "dev_kill_shell_core killed a process it had not recorded (pid $decoy)"
[[ "$after" == "0" ]] \
  || fail "$after sandbox process(es) survived dev_kill_shell_core (had $(echo $everything | wc -w | tr -d ' '), $before of them sessions)"
echo "    $before sessions and $(echo $everything | wc -w | tr -d ' ') processes before kill → 0 within 10s, bystander untouched — OK"

# --- 3. registry 哨兵 -------------------------------------------------
echo "==> phase 3: deleting the registry kills the orphans"
rm -f "$APPLOG"
MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
disown
wait_for WINDOW_FIRST_FRAME 30 || fail "sandbox app never painted (phase 3)"
sleep 2
l3s=$(dev_session_pids)
# shellcheck disable=SC2086
before=$(alive $l3s)
(( before >= 1 )) || fail "no sessions to orphan"
# 只杀 shell/core,把 session 留成孤儿 —— 然后删注册表。
# shell 按它写下的 pid 找,core 是它的子进程。
shell_pid=$(dev_shell_pid)
[[ -n $shell_pid ]] || fail "shell.pid does not name this sandbox's shell"
# only these two: the sessions' own shells are descendants too, and
# killing those would end the sessions this phase needs as orphans
upper="$shell_pid $(dev_core_pids)"
# shellcheck disable=SC2086
kill -KILL $upper 2>/dev/null
sleep 1
# shellcheck disable=SC2086
orphans=$(alive $l3s)
(( orphans >= 1 )) || fail "orphans already gone before the registry delete?"
rm -rf "$MARSPOT_STATE_DIR/sessions"
# 哨兵每 5 秒查一次;给 12 秒。
for i in $(seq 1 24); do
  # shellcheck disable=SC2086
  [[ "$(alive $l3s)" == "0" ]] && break
  sleep 0.5
done
# shellcheck disable=SC2086
left=$(alive $l3s)
[[ "$left" == "0" ]] \
  || fail "$left orphan(s) outlived their deleted registry ($orphans before)"
echo "    $orphans orphans + registry deleted → 0 within 12s — OK"

echo
echo "PASS — crash-loop brake, sandbox reaping, registry deadman all hold."

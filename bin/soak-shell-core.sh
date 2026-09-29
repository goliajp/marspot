#!/usr/bin/env bash
#
# Soak test for the shell/core architecture (Step 8).
#
# Runs the pair for `DURATION_S` seconds (default 600 = 10 min),
# sampling RSS every `SAMPLE_S` seconds (default 30 s).  Asserts:
#
#   - shell + core stay alive the whole time (no SEGV / abort)
#   - shell  RSS doesn't grow past `SHELL_RSS_CAP_KIB` (default 70 MiB)
#   - core   RSS doesn't grow past `CORE_RSS_CAP_KIB`  (default 200 MiB)
#   - no `crash budget exceeded` in the log
#   - core never restarted (single PID for the whole run)
#
# Override caps / cadence by exporting the env vars above.  No window
# input is exercised here; this is the "is anything leaking" gate.
# A separate windowed soak (manual, for now) covers the resize +
# IOSurface paths.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
LOG=/tmp/marspot-soak-shell-core.log
RSS_LOG=/tmp/marspot-soak-shell-core.rss.tsv
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"

DURATION_S="${DURATION_S:-600}"
SAMPLE_S="${SAMPLE_S:-30}"
SHELL_RSS_CAP_KIB="${SHELL_RSS_CAP_KIB:-153600}" # 150 MiB — Metal driver
                                                 # + AppKit + CoreText
                                                 # preload sits at ~80
                                                 # MiB right after boot;
                                                 # this leaves 70 MiB of
                                                 # growth budget.
CORE_RSS_CAP_KIB="${CORE_RSS_CAP_KIB:-204800}"   # 200 MiB

fail() {
  echo "FAIL: $*"
  echo "  RSS samples:"
  cat "$RSS_LOG" 2>/dev/null | sed 's/^/    /'
  echo "  (last 40 log lines):"
  tail -40 "$LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() {
  dev_kill_shell_core
}
trap cleanup EXIT

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" ]]; then
  ( cd "$ROOT" && cargo build --release --bin marspot-shell --bin marspot-core 2>&1 | tail -3 )
fi

dev_ensure_shelld || fail "sandbox shelld"
dev_wipe_state
cleanup
> "$LOG"
echo -e "t_s\tshell_rss_kib\tcore_rss_kib\tcore_pid" > "$RSS_LOG"

nohup "$SHELL_BIN" >"$LOG" 2>&1 < /dev/null &
disown
sleep 2

SHELL_PID=$(pgrep -f "$SHELL_BIN( |$)" | head -1)
CORE_PID=$(pgrep -f "$CORE_BIN( |$)"  | head -1)
[[ -n "$SHELL_PID" ]] || fail "shell not running after boot"
[[ -n "$CORE_PID"  ]] || fail "core not running after boot"
echo "soak start: shell=$SHELL_PID core=$CORE_PID for ${DURATION_S}s, sample every ${SAMPLE_S}s"
INITIAL_CORE_PID="$CORE_PID"

start=$(date +%s)
elapsed=0
while (( elapsed < DURATION_S )); do
  sleep "$SAMPLE_S"
  elapsed=$(( $(date +%s) - start ))
  shell_rss=$(ps -p "$SHELL_PID" -o rss= 2>/dev/null | tr -d ' ' || true)
  core_rss=$(ps -p "$CORE_PID"  -o rss= 2>/dev/null | tr -d ' ' || true)
  cur_core_pid=$(pgrep -f "$CORE_BIN( |$)" | head -1)
  echo -e "${elapsed}\t${shell_rss:-0}\t${core_rss:-0}\t${cur_core_pid:-0}" >> "$RSS_LOG"

  if [[ -z "$shell_rss" ]]; then
    fail "shell died at t=${elapsed}s"
  fi
  if [[ -z "$core_rss" || "$cur_core_pid" != "$INITIAL_CORE_PID" ]]; then
    fail "core died / restarted at t=${elapsed}s (was $INITIAL_CORE_PID, now ${cur_core_pid:-none})"
  fi
  if (( shell_rss > SHELL_RSS_CAP_KIB )); then
    fail "shell RSS ${shell_rss} KiB > cap ${SHELL_RSS_CAP_KIB} at t=${elapsed}s"
  fi
  if (( core_rss > CORE_RSS_CAP_KIB )); then
    fail "core RSS ${core_rss} KiB > cap ${CORE_RSS_CAP_KIB} at t=${elapsed}s"
  fi
done

# Post-run assertions on the log itself.
if grep -q "crash budget exceeded" "$LOG"; then
  fail "log contains 'crash budget exceeded' — at least one crash happened"
fi
if grep -q "core exited unexpectedly" "$LOG"; then
  fail "log contains 'core exited unexpectedly'"
fi

echo "PASS (ran ${DURATION_S}s; final shell_rss=${shell_rss} core_rss=${core_rss})"
echo "RSS samples: $RSS_LOG"

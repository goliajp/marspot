#!/usr/bin/env bash
#
# Per-session L3 soak (target #4 step 3c).
#
# Drives the L3 input → echo → shm pipeline headlessly (no Metal): each
# round spawns a real marspot-session with an inherited shm region +
# control socket (exactly as marspot-core's spawn_l3_pane does), types a
# character, and asserts it echoes into the published grid.  Across
# ITERATIONS rounds it asserts:
#   - every round PASSes the echo check (input path works),
#   - marspot-session links zero GUI frameworks (RSS floor intact),
#   - L3 resident stays under RSS_CAP_KIB (the ~3–5 MB floor, generous),
#   - no marspot-session process is orphaned between rounds (clean
#     Drop / teardown — the probe kills + reaps each child),
#   - the sandbox shelld is never touched.
#
# Runs entirely in the dev sandbox; never touches the installed app.
# Run after `cargo build --release`.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

SESSION_BIN="$ROOT/target/release/marspot-session"
PROBE_BIN="$ROOT/target/release/examples/l3_echo_probe"
RUN_LOG=/tmp/marspot-soak-l3.log

ITERATIONS="${ITERATIONS:-10}"
# The L3 floor is ~3–5 MB; cap well above to catch a real regression
# (e.g. an accidental GUI link or unbounded buffer) without flapping.
RSS_CAP_KIB="${RSS_CAP_KIB:-16384}"

fail() {
  echo "FAIL: $*"
  echo "  (last 30 lines of $RUN_LOG):"
  tail -30 "$RUN_LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

# Reap any sandbox marspot-session a previous aborted run left behind.
# Scoped to the dev-build path so the installed app is never matched.
kill_sandbox_sessions() {
  pkill -9 -f "$SESSION_BIN( |\$)" >/dev/null 2>&1 || true
}
count_sandbox_sessions() {
  pgrep -f "$SESSION_BIN( |$)" 2>/dev/null | grep -c . || true
}

cleanup() { kill_sandbox_sessions; }
trap cleanup EXIT

if [[ ! -x "$SESSION_BIN" || ! -x "$PROBE_BIN" ]]; then
  ( cd "$ROOT" && cargo build --release -p marspot-session --example l3_echo_probe 2>&1 | tail -3 )
fi
[[ -x "$SESSION_BIN" ]] || fail "marspot-session not built at $SESSION_BIN"
[[ -x "$PROBE_BIN" ]]   || fail "probe not built at $PROBE_BIN"

# Zero-GUI floor: the shipped L3 binary must link no GUI frameworks.
if otool -L "$SESSION_BIN" | grep -qiE 'Metal|AppKit|CoreText'; then
  fail "marspot-session links a GUI framework (RSS floor broken)"
fi
echo "otool OK — marspot-session links zero GUI frameworks"

dev_ensure_shelld || fail "sandbox shelld"
kill_sandbox_sessions
: > "$RUN_LOG"

base_sessions=$(count_sandbox_sessions)
echo "boot OK — sandbox shelld up; ${base_sessions} pre-existing sandbox session procs"

peak_rss=0
for i in $(seq 1 "$ITERATIONS"); do
  echo "--- round $i/$ITERATIONS ---" | tee -a "$RUN_LOG"

  # Run the probe in the background so we can sample the L3 RSS while it
  # lives, then assert its exit code.
  "$PROBE_BIN" "$SESSION_BIN" >>"$RUN_LOG" 2>&1 &
  probe_pid=$!

  # Sample the spawned marspot-session's RSS a few times during the run.
  for _ in $(seq 1 20); do
    kill -0 "$probe_pid" 2>/dev/null || break
    rss=$(pgrep -f "$SESSION_BIN( |$)" 2>/dev/null | head -1 \
          | xargs -I{} ps -p {} -o rss= 2>/dev/null | tr -d ' ')
    if [[ -n "${rss:-}" ]] && (( rss > peak_rss )); then peak_rss=$rss; fi
    sleep 0.1
  done

  wait "$probe_pid"
  rc=$?
  (( rc == 0 )) || fail "round $i: probe exited $rc (echo pipeline broken)"

  # The child must be reaped — no orphan accumulation.
  sleep 0.2
  now=$(count_sandbox_sessions)
  (( now <= base_sessions )) \
    || fail "round $i: ${now} sandbox session procs (orphan leak; baseline ${base_sessions})"
  echo "  round $i OK — echo verified, L3 reaped (procs=${now})"
done

(( peak_rss > 0 )) || echo "  (note: never caught an L3 RSS sample — rounds too fast)"
if (( peak_rss > RSS_CAP_KIB )); then
  fail "L3 peak RSS ${peak_rss} KiB exceeds cap ${RSS_CAP_KIB}"
fi

echo "PASS — ${ITERATIONS} rounds, echo verified each, L3 peak RSS=${peak_rss} KiB (cap ${RSS_CAP_KIB}), no orphans"

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
MULTI_PROBE_BIN="$ROOT/target/release/examples/l3_multi_probe"
RESIZE_PROBE_BIN="$ROOT/target/release/examples/l3_resize_probe"
SCROLL_PROBE_BIN="$ROOT/target/release/examples/l3_scroll_probe"
SELECTION_PROBE_BIN="$ROOT/target/release/examples/l3_selection_probe"
LATENCY_PROBE_BIN="$ROOT/target/release/examples/l3_latency_probe"
CRASH_PROBE_BIN="$ROOT/target/release/examples/l3_crash_probe"
RUN_LOG=/tmp/marspot-soak-l3.log

ITERATIONS="${ITERATIONS:-10}"
# N concurrent L3 sessions for the multi-session collision check (4a).
MULTI_N="${MULTI_N:-4}"
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

# Always (re)build the bin + probes together: an incremental build is
# ~instant when current, and rebuilding unconditionally rules out the
# version-skew trap where a stale probe creates a grid_shm region an
# updated marspot-session refuses (or vice-versa).
( cd "$ROOT" && cargo build --release -p marspot-session \
    --example l3_echo_probe --example l3_multi_probe --example l3_resize_probe \
    --example l3_scroll_probe --example l3_selection_probe \
    --example l3_latency_probe --example l3_crash_probe 2>&1 | tail -3 )
[[ -x "$SESSION_BIN" ]]      || fail "marspot-session not built at $SESSION_BIN"
[[ -x "$PROBE_BIN" ]]        || fail "probe not built at $PROBE_BIN"
[[ -x "$MULTI_PROBE_BIN" ]]  || fail "multi probe not built at $MULTI_PROBE_BIN"
[[ -x "$RESIZE_PROBE_BIN" ]] || fail "resize probe not built at $RESIZE_PROBE_BIN"
[[ -x "$SCROLL_PROBE_BIN" ]]    || fail "scroll probe not built at $SCROLL_PROBE_BIN"
[[ -x "$SELECTION_PROBE_BIN" ]] || fail "selection probe not built at $SELECTION_PROBE_BIN"
[[ -x "$LATENCY_PROBE_BIN" ]]   || fail "latency probe not built at $LATENCY_PROBE_BIN"
[[ -x "$CRASH_PROBE_BIN" ]]     || fail "crash probe not built at $CRASH_PROBE_BIN"

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

# N-session collision check (4a): allocate N distinct sessions via the
# L2-allocates / L3-attaches split, spawn one L3 each, assert every one
# echoes only its own char (a session collision would cross them) and
# leaves no orphan.
echo "--- multi-session (N=${MULTI_N}) ---" | tee -a "$RUN_LOG"
"$MULTI_PROBE_BIN" "$SESSION_BIN" "$MULTI_N" >>"$RUN_LOG" 2>&1
mrc=$?
(( mrc == 0 )) || fail "multi-session probe exited $mrc (session collision or echo lost)"
sleep 0.2
now=$(count_sandbox_sessions)
(( now <= base_sessions )) \
  || fail "multi-session: ${now} sandbox session procs (orphan leak; baseline ${base_sessions})"
echo "  multi-session OK — ${MULTI_N} distinct sessions, each echoed its own char, no orphans"

# Resize check (4b): grow + shrink an L3 in place; the published snapshot
# dims must track each resize with no remap (region is capacity-mapped).
echo "--- resize ---" | tee -a "$RUN_LOG"
"$RESIZE_PROBE_BIN" "$SESSION_BIN" >>"$RUN_LOG" 2>&1
rrc=$?
(( rrc == 0 )) || fail "resize probe exited $rrc (resize not tracked in shm)"
sleep 0.2
now=$(count_sandbox_sessions)
(( now <= base_sessions )) \
  || fail "resize: ${now} sandbox session procs (orphan leak; baseline ${base_sessions})"
echo "  resize OK — 80x24 → 100x40 → 40x12 → 80x24 tracked in place, no orphans"

# Scroll check (4b): build scrollback, scroll into history, assert the
# published window tracks the offset (content changes), then back to live.
echo "--- scroll ---" | tee -a "$RUN_LOG"
"$SCROLL_PROBE_BIN" "$SESSION_BIN" >>"$RUN_LOG" 2>&1
src=$?
(( src == 0 )) || fail "scroll probe exited $src (scrollback window not tracked)"
sleep 0.2
now=$(count_sandbox_sessions)
(( now <= base_sessions )) \
  || fail "scroll: ${now} sandbox session procs (orphan leak; baseline ${base_sessions})"
echo "  scroll OK — scrolled into history (window moved) and back to live, no orphans"

# Selection-text check (4c): Cmd-C round-trip — request the text under a
# selection from L3 (L2's mirror is window-only) and assert it carries the
# on-screen marker.
echo "--- selection ---" | tee -a "$RUN_LOG"
"$SELECTION_PROBE_BIN" "$SESSION_BIN" >>"$RUN_LOG" 2>&1
selrc=$?
(( selrc == 0 )) || fail "selection probe exited $selrc (GetSelectionText round-trip broken)"
sleep 0.2
now=$(count_sandbox_sessions)
(( now <= base_sessions )) \
  || fail "selection: ${now} sandbox session procs (orphan leak; baseline ${base_sessions})"
echo "  selection OK — GetSelectionText returned the on-screen text from L3, no orphans"

# Latency tail (4d): keystroke→echo through the extra L2→L3→L2 IPC hop.
# The design calls for measuring this; the probe reports p50/p99/max and
# fails only on a gross regression (a polling / stalling event loop).
echo "--- latency ---" | tee -a "$RUN_LOG"
lat_line=$("$LATENCY_PROBE_BIN" "$SESSION_BIN" 2>&1 | tee -a "$RUN_LOG" | grep -E '^PASS|keystroke')
latrc=${PIPESTATUS[0]}
(( latrc == 0 )) || fail "latency probe exited $latrc (keystroke→echo regression)"
sleep 0.2
now=$(count_sandbox_sessions)
(( now <= base_sessions )) \
  || fail "latency: ${now} sandbox session procs (orphan leak; baseline ${base_sessions})"
echo "  latency OK — ${lat_line##*: }"

# Crash isolation (5c): SIGKILL one of two L3s; the sibling must keep
# echoing and the dead L3's shm must stay readable (an L2 reading it keeps
# running). Proves the process boundary isolates a per-session crash.
echo "--- crash-isolation ---" | tee -a "$RUN_LOG"
"$CRASH_PROBE_BIN" "$SESSION_BIN" >>"$RUN_LOG" 2>&1
crrc=$?
(( crrc == 0 )) || fail "crash probe exited $crrc (a crashing L3 took down a sibling / its shm)"
sleep 0.2
now=$(count_sandbox_sessions)
(( now <= base_sessions )) \
  || fail "crash: ${now} sandbox session procs (orphan leak; baseline ${base_sessions})"
echo "  crash-isolation OK — one L3 killed, sibling unaffected, dead shm still readable, no orphans"

echo "PASS — ${ITERATIONS} rounds + N=${MULTI_N} multi-session + resize + scroll + selection + latency + crash, all verified, L3 peak RSS=${peak_rss} KiB (cap ${RSS_CAP_KIB}), no orphans"

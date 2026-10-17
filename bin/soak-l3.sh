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
#   - no marspot-session a probe started outlives it (clean Drop /
#     teardown — the probe kills + reaps each child).
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
SWAP_PROBE_BIN="$ROOT/target/release/examples/l3_swap_probe"
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

# Always (re)build the bin + probes together: an incremental build is
# ~instant when current, and rebuilding unconditionally rules out the
# version-skew trap where a stale probe creates a grid_shm region an
# updated marspot-session refuses (or vice-versa).
# The trap above is the one this used to fall into.  The build was in a
# pipe, so its exit code went to `tail`; the `-x` checks below then found
# binaries from mid-June -- seven of these probes lost their source in the
# history rebuild -- and ran those against today's session binary.  One of
# them still tried to connect to a daemon RFC-003 deleted, and that is
# the only reason it was ever noticed.
for probe in l3_echo_probe l3_multi_probe l3_resize_probe \
             l3_scroll_probe l3_selection_probe l3_latency_probe \
             l3_crash_probe l3_swap_probe; do
  dev_require_probe "$probe" || exit 1
done
[[ -x "$SESSION_BIN" ]]      || fail "marspot-session not built at $SESSION_BIN"
[[ -x "$PROBE_BIN" ]]        || fail "probe not built at $PROBE_BIN"
[[ -x "$MULTI_PROBE_BIN" ]]  || fail "multi probe not built at $MULTI_PROBE_BIN"
[[ -x "$RESIZE_PROBE_BIN" ]] || fail "resize probe not built at $RESIZE_PROBE_BIN"
[[ -x "$SCROLL_PROBE_BIN" ]]    || fail "scroll probe not built at $SCROLL_PROBE_BIN"
[[ -x "$SELECTION_PROBE_BIN" ]] || fail "selection probe not built at $SELECTION_PROBE_BIN"
[[ -x "$LATENCY_PROBE_BIN" ]]   || fail "latency probe not built at $LATENCY_PROBE_BIN"
[[ -x "$CRASH_PROBE_BIN" ]]     || fail "crash probe not built at $CRASH_PROBE_BIN"
[[ -x "$SWAP_PROBE_BIN" ]]      || fail "swap probe not built at $SWAP_PROBE_BIN"

# Zero-GUI floor: the shipped L3 binary must link no GUI frameworks.
if otool -L "$SESSION_BIN" | grep -qiE 'Metal|AppKit|CoreText'; then
  fail "marspot-session links a GUI framework (RSS floor broken)"
fi
echo "otool OK — marspot-session links zero GUI frameworks"

: > "$RUN_LOG"

peak_rss=0
for i in $(seq 1 "$ITERATIONS"); do
  echo "--- round $i/$ITERATIONS ---" | tee -a "$RUN_LOG"

  # Run the probe in the background so we can sample the L3 RSS while it
  # lives, then assert its exit code.  The session is the probe's child;
  # its pid is taken from there, never looked up by name.
  "$PROBE_BIN" "$SESSION_BIN" >>"$RUN_LOG" 2>&1 &
  probe_pid=$!
  kids=""
  for _ in $(seq 1 20); do
    kill -0 "$probe_pid" 2>/dev/null || break
    for k in $(dev_probe_sessions "$probe_pid"); do
      [[ " $kids " == *" $k "* ]] || kids+=" $k"
      rss=$(ps -o rss= -p "$k" 2>/dev/null | tr -d ' ')
      if [[ -n "${rss:-}" ]] && (( rss > peak_rss )); then peak_rss=$rss; fi
    done
    sleep 0.1
  done

  wait "$probe_pid"
  rc=$?
  (( rc == 0 )) || fail "round $i: probe exited $rc (echo pipeline broken)"

  # The child must be reaped — no orphan accumulation.
  sleep 0.2
  # shellcheck disable=SC2086
  orphans=$(dev_reap_probe_sessions $kids)
  (( orphans == 0 )) || fail "round $i: ${orphans} marspot-session(s) outlived the probe (orphan leak)"
  echo "  round $i OK — echo verified, L3 reaped"
done

(( peak_rss > 0 )) || echo "  (note: never caught an L3 RSS sample — rounds too fast)"
if (( peak_rss > RSS_CAP_KIB )); then
  fail "L3 peak RSS ${peak_rss} KiB exceeds cap ${RSS_CAP_KIB}"
fi

# section <label> <what broke> <probe> [args...] — one probe, its exit
# code, and no session of it left running.
section() {
  local label=$1 broke=$2 rc
  shift 2
  echo "--- $label ---" | tee -a "$RUN_LOG"
  dev_run_probe "$@" >>"$RUN_LOG" 2>&1
  rc=$?
  (( rc != 3 )) || fail "$label: a marspot-session outlived the probe (orphan leak)"
  (( rc == 0 )) || fail "$label probe exited $rc ($broke)"
}

# N-session collision check (4a): allocate N distinct sessions via the
# L2-allocates / L3-attaches split, spawn one L3 each, assert every one
# echoes only its own char (a session collision would cross them) and
# leaves no orphan.
section "multi-session (N=${MULTI_N})" "session collision or echo lost" \
  "$MULTI_PROBE_BIN" "$SESSION_BIN" "$MULTI_N"
echo "  multi-session OK — ${MULTI_N} distinct sessions, each echoed its own char, no orphans"

# Resize check (4b): grow + shrink an L3 in place; the published snapshot
# dims must track each resize with no remap (region is capacity-mapped).
section resize "resize not tracked in shm" "$RESIZE_PROBE_BIN" "$SESSION_BIN"
echo "  resize OK — 80x24 → 100x40 → 40x12 → 80x24 tracked in place, no orphans"

# Scroll check (4b): build scrollback, scroll into history, assert the
# published window tracks the offset (content changes), then back to live.
section scroll "scrollback window not tracked" "$SCROLL_PROBE_BIN" "$SESSION_BIN"
echo "  scroll OK — scrolled into history (window moved) and back to live, no orphans"

# Selection-text check (4c): Cmd-C round-trip — request the text under a
# selection from L3 (L2's mirror is window-only) and assert it carries the
# on-screen marker.
section selection "GetSelectionText round-trip broken" "$SELECTION_PROBE_BIN" "$SESSION_BIN"
echo "  selection OK — GetSelectionText returned the on-screen text from L3, no orphans"

# Latency tail (4d): keystroke→echo through the extra L2→L3→L2 IPC hop.
# The design calls for measuring this; the probe reports p50/p99/max and
# fails only on a gross regression (a polling / stalling event loop).
section latency "keystroke→echo regression" "$LATENCY_PROBE_BIN" "$SESSION_BIN"
lat_line=$(grep -E '^PASS|keystroke' "$RUN_LOG" | tail -1)
echo "  latency OK — ${lat_line##*: }"

# Crash isolation (5c): SIGKILL one of two L3s; the sibling must keep
# echoing and the dead L3's shm must stay readable (an L2 reading it keeps
# running). Proves the process boundary isolates a per-session crash.
section crash-isolation "a crashing L3 took down a sibling / its shm" \
  "$CRASH_PROBE_BIN" "$SESSION_BIN"
echo "  crash-isolation OK — one L3 killed, sibling unaffected, dead shm still readable, no orphans"

# Silent swap (5a) — the load-bearing mechanism of silent update.
#
# `l3_swap_probe` stages a copy of marspot-session whose MARSPOT_FP differs
# (re-signed, or macOS refuses to exec it), SIGTERMs the session, and
# asserts the handover: same pid, the replayed grid matching the old one
# bit for bit (FNV-1a over every cell), and input accepted over a fresh
# connection to the listener after the Hello handshake.
#
# Five rounds by default on ONE session (`SWAP_ROUNDS`), each staging
# against the image now running.  That is the property
# `soak-long-connection-execv.sh` claimed -- "survived 5 execv swaps" --
# and `soak-snapshot-survival.sh`'s bit-for-bit grid comparison; both drove
# the L4 daemon RFC-003 deleted in June, so both had been reporting on a
# binary left in `target/`.  They are gone; this covers what they were
# for, on the architecture that exists.
#
# This section was red until 2026-10-01.  The single-pane swap used to
# spawn a replacement L3 on the same session id, which cannot work -- the
# live L3 holds a lock on `sessions/<id>/` and a second one refuses to
# start.  The swap that exists is `swap_idle_l3`: promote the staged
# image, then signal each L3 to execv itself.
section silent-swap "silent-swap mechanism broken" \
  "$SWAP_PROBE_BIN" "$SESSION_BIN" "${SWAP_ROUNDS:-5}"
echo "  silent-swap OK — replacement replayed + took over on the same session, no orphans"

echo "PASS — ${ITERATIONS} rounds + N=${MULTI_N} multi-session + resize + scroll + selection + latency + crash + swap, all verified, L3 peak RSS=${peak_rss} KiB (cap ${RSS_CAP_KIB}), no orphans"

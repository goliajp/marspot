#!/usr/bin/env bash
# bin/bench.sh — perf gate.
#
# Two tiers:
#   bin/bench.sh           default: headless `--bench parse` + `--bench
#                          render` only.  ~1-2 seconds, run on every
#                          commit / before push.
#   bin/bench.sh --build-only   build and stop; --no-build   measure
#                          and do not build.  Split so a caller can
#                          take a shared lock for the compile and an
#                          exclusive one for the measurement.
#   bin/bench.sh --full    also runs the live PTY pipeline through the
#                          app and through L3, the key-to-frame latency
#                          of one pane (a few minutes), AND the Phase 9
#                          SSIM gate.  Use before merging to develop.
#   bin/bench.sh --ssim    add the Phase 9 SSIM gate (font rendering);
#                          implied by --full.
#   bin/bench.sh --update-baseline   rewrite the floors in
#       bench/baseline.json from this run (never a size budget).  On
#       a machine other than the floors' it re-locks every limit and
#       needs --full.
#   bin/bench.sh --size-tables   link the four binaries with a map
#       and write raw module tables to bench/results/size-tables/
#       instead of measuring (bin/_size_budget.py says what for).
#
# Reads bench/baseline.json + bench/font-rendering/snapshots/*.png.
# Exits non-zero on any regression (perf OR SSIM).  One row per metric
# goes to stdout with its unit, limit, the span of its trials and the
# host's load while it was measured; a one-line summary at the end says
# GATE PASSED, GATE FAILED or GATE INCONCLUSIVE for perf, plus SSIM GATE
# PASSED / FAILED when --ssim is on.  Exit 4, before anything is built:
# the floors were locked on another machine, or a re-lock there was asked
# for without --full (bin/_bench_baseline.py).
#
# Every run leaves bench/results/gate-record.json and appends each row to
# bench/results/history/<metric>.jsonl (`bin/_bench_record.py show`).

set -euo pipefail

# See bin/test.sh — inside a marspot terminal the inherited
# MARSPOT_SESSION_ID makes every headless-bench `Terminal::new` write
# the REAL state dir's sessions/<id>/scrollback.bin.  Unset first.
unset MARSPOT_SESSION_ID

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"   # marspot_bin resolves CARGO_TARGET_DIR
# shellcheck source=bin/_bench-window.sh
source "$ROOT/bin/_bench-window.sh"
# BASELINE is overridable via env so tests can point at synthesised
# fixture baselines without touching the real one.
BASELINE="${BASELINE:-$ROOT/bench/baseline.json}"
SCENARIOS_DIR="$ROOT/bench/scenarios"

MODE=fast
UPDATE=0
SIZE_TABLES=0
WITH_SSIM=0
# Build and measure are separable so the caller can hold a different
# lock for each: a build is heavy work that belongs alongside other
# heavy work, a measurement wants the machine to itself.  Holding the
# exclusive lock across both makes everyone else wait for a compile.
DO_BUILD=1
DO_MEASURE=1
for arg in "$@"; do
  case "$arg" in
    --full)              MODE=full ;;
    --update-baseline)   UPDATE=1 ;;
    --size-tables)       SIZE_TABLES=1 ;;
    --ssim)              WITH_SSIM=1 ;;
    --build-only)        DO_MEASURE=0 ;;
    --no-build)          DO_BUILD=0 ;;
    --help|-h)
      sed -n '2,24p' "$0"; exit 0 ;;
    *)
      echo "unknown arg: $arg (try --help)" >&2; exit 2 ;;
  esac
done

# --full implies --ssim by default — pre-merge gates are the place
# the SSIM check belongs.  Skip the implicit chain when the user
# explicitly passes --update-baseline (which is a write-mode op).
if [[ "$MODE" == "full" && "$UPDATE" -eq 0 ]]; then
  WITH_SSIM=1
fi

if [[ ! -f "$BASELINE" ]]; then
  echo "missing $BASELINE" >&2
  exit 2
fi
if (( SIZE_TABLES )); then
  (( ! DO_BUILD )) || "$ROOT/bin/_size_tables.sh" build
  (( ! DO_MEASURE )) || "$ROOT/bin/_size_tables.sh" tables
  exit 0
fi
# floors are only compared on the machine they were locked on; checked
# before the build, so a refused run costs nothing
python3 "$ROOT/bin/_bench_baseline.py" check "$BASELINE" "$UPDATE" "$MODE" || exit 4

# Make sure scenarios exist; regenerate if missing (cheap, deterministic).
#
# Every scenario, not two of them.  The list used to name cat-ascii and
# scroll-history, so adding a scenario meant the remote host — which
# never has them, they are gitignored and not synced — ran the whole
# build and then failed on the first read of the new file.
for _s in cat-ascii cat-mixed cat-cjk cat-emoji cat-styled cat-highlighted cat-clusters scroll-history; do
  if [[ ! -f "$SCENARIOS_DIR/$_s.bin" ]]; then
    echo "==> generating bench scenarios ($_s missing)"
    "$ROOT/bin/gen-scenarios.sh" >/dev/null
    break
  fi
done

# Build release unconditionally — cargo is incremental, so an
# up-to-date tree is a sub-second no-op.  The previous "only if the
# binary is missing" check silently benched a stale binary forever
# once one existed: on the bench host the gate measured a Jun 6 build for
# a month of runs (caught 2026-07-11 via byte-identical size numbers
# across an objc2 major upgrade).
#
# The l3_throughput example too: the measurement runs the run dir's copy
# of it (bin/_lib.sh bench_run_begin), and a probe from an earlier build
# would drive this build's marspot-session over a frame layout it may
# no longer share.
if (( DO_BUILD )); then
  echo "==> building marspot (release)"
  # the example in a build of its own that names no bins: selecting an
  # example unifies dev-dependency features, and naming bins in it (or
  # -p marspot-session alone) would replace shipped binaries with ones
  # that never ship
  ( cd "$ROOT" && cargo build --release 2>&1 | tail -3 )
  ( cd "$ROOT" && cargo build --release --example l3_throughput 2>&1 | tail -3 )
fi

# ---- key-to-frame latency: the timed build ------------------------------
# The time from a key press to the frame that shows it can only be read
# from a `latency-trace` build: the key script that presses keys in a
# window nobody sits at exists only there, so neither a plain build nor
# the `seen` scene (which is that same script plus window captures) can
# read it.  Only the shell and the core carry the feature; the session
# is the shipped binary itself (its package has no such feature).  With
# MARSPOT_LATENCY_TRACE unset the feature costs one relaxed atomic load
# per mark; set, a mark is a clock read and 16 bytes into a buffer
# under an uncontended lock, written out only while the process idles.
# Its own target dir, so the shipped build next to it stays untouched.
LAT_TARGET="$(_mars_target_dir)/latency-trace"
if (( DO_BUILD )) && [[ $MODE == full ]]; then
  echo "==> building marspot-shell and marspot-core with latency-trace"
  _lat_t0=$(date +%s)
  ( cd "$ROOT" && CARGO_TARGET_DIR="$LAT_TARGET" cargo build --release --features latency-trace \
      --bin marspot-shell --bin marspot-core 2>&1 | tail -3 )
  echo "    took $(( $(date +%s) - _lat_t0 ))s"
fi
if (( ! DO_MEASURE )); then
  exit 0
fi
BENCH_SETTLE_S="${BENCH_SETTLE_S:-8}"
bench_settle "$BENCH_SETTLE_S"

# ---- collect current measurements ---------------------------------------
# the run's copies of the binaries, and its scratch, live in the run dir
LOAD_SAMPLER=""
trap '[[ -z $LOAD_SAMPLER ]] || kill "$LOAD_SAMPLER" 2>/dev/null; bench_run_end' EXIT
trap 'exit 143' TERM
trap 'exit 130' INT
trap 'exit 129' HUP
bench_run_begin
echo "==> run dir $MARSPOT_BENCH_RUN_DIR"
CUR_DIR="$MARSPOT_BENCH_RUN_DIR/cur"
mkdir -p "$CUR_DIR"
MARSPOT="$(marspot_bin marspot)"
load1() { sysctl -n vm.loadavg | awk '{print $2}'; }

# The host's load through the whole run, one "<epoch> <load1>" line
# every two seconds: each block of trials below is judged on the samples
# that fall inside it, not on one reading taken before it started.
( while :; do echo "$(date +%s) $(load1)"; sleep 2; done ) > "$CUR_DIR/load.samples" 2>/dev/null &
LOAD_SAMPLER=$!
disown "$LOAD_SAMPLER" 2>/dev/null || true

# run_block <name> <function> — run a block of trials, and run it again
# (up to BLOCK_REQUEUE more times) when the host was busy while it ran.
# A reading taken next to someone else's work is not reported as a
# number: what is left after the last attempt is a block marked invalid,
# and the gate says so for every row that comes from it.
BLOCK_REQUEUE="${BLOCK_REQUEUE:-2}"
run_block() {
  local name=$1 fn=$2 attempt
  for ((attempt = 0; attempt <= BLOCK_REQUEUE; attempt++)); do
    if (( attempt > 0 )); then
      echo "    $name: measuring again ($attempt of $BLOCK_REQUEUE)"
      python3 "$ROOT/bin/_l3_aggregate.py" settle 120 3.0
    fi
    echo "$(date +%s) $(load1)" >> "$CUR_DIR/load.samples"
    date +%s > "$CUR_DIR/$name.begin"
    python3 "$ROOT/bin/_bench_record.py" cpu-snapshot > "$CUR_DIR/$name.cpu0"
    "$fn"
    python3 "$ROOT/bin/_bench_record.py" cpu-snapshot > "$CUR_DIR/$name.cpu1"
    echo "$(date +%s) $(load1)" >> "$CUR_DIR/load.samples"
    date +%s > "$CUR_DIR/$name.end"
    if python3 "$ROOT/bin/_bench_record.py" judge "$CUR_DIR" "$name"; then
      return 0
    fi
  done
  return 0
}

# One `--bench parse` run under time(1), which on macOS also reports the
# instructions the process retired.  Appends the rate to <prefix>.samples
# and the instructions per byte to <prefix>.insn.  Environment passes
# through, which is how the file-backed variant is selected.
parse_trial() {
  local prefix=$1 scenario=$2 bps ipb
  /usr/bin/time -l "$MARSPOT" --bench "parse:$scenario" \
    > "$CUR_DIR/parse.out" 2> "$CUR_DIR/parse.err"
  read -r bps ipb < <(python3 "$ROOT/bin/_bench_record.py" time-l "$CUR_DIR/parse.out" "$CUR_DIR/parse.err")
  echo "$bps" >> "$prefix.samples"
  echo "$ipb" >> "$prefix.insn"
}

# N=5 trials per parse measurement; we take the median.  Single-run
# numbers vary ~5–8 % on the same code (thermal / scheduler / kernel
# cache), so a 1-shot gate flaps without contributing real signal.
#
# Each measurement loop is preceded by a discarded warm-up trial
# (perf-attack E2): trial 1 after a fresh `cargo build --release` pays
# cold-cache cost — file pages, binary text-section paging, glyph
# atlas data dir touches — that drags the median by ~20% and
# manufactures false gate failures.  The warm-up trial pre-pays
# those costs; the recorded trials measure steady-state.
measure_parse() {
  local s i
  for s in cat-ascii cat-mixed cat-cjk cat-emoji cat-styled cat-highlighted cat-clusters; do
    : > "$CUR_DIR/parse-$s.samples"
    : > "$CUR_DIR/parse-$s.insn"
    "$MARSPOT" --bench "parse:$SCENARIOS_DIR/$s.bin" >/dev/null
    for i in 1 2 3 4 5; do
      parse_trial "$CUR_DIR/parse-$s" "$SCENARIOS_DIR/$s.bin"
    done
  done
}
echo "==> headless parse (5 trials each, taking median)"
run_block parse measure_parse

# The SAME parse, with the scrollback a real pane actually has.
#
# `--bench parse` builds a Terminal with no MARSPOT_SESSION_ID, which
# selects the in-RAM scrollback.  Every L3 pane sets that variable and
# gets the file-backed one instead — which trims, encodes and writes
# every scrolled line, and keeps a RAM ring besides.  Measured
# 2026-09-07 on cat-emoji: **192 MB/s in RAM, 105 MB/s file-backed**.
# The gate was reporting a number no user has, and every comparison
# made against it — including a whole afternoon of chasing Alacritty —
# was against the wrong path.
#
# Recorded alongside the RAM numbers rather than replacing them: the
# existing series stays comparable, and a regression that only shows
# in one of the two is worth seeing as exactly that.
# What the box was doing while this ran decides whether it is a reading.
# Measured 2026-09-28 on a busy runner: the same binary, interleaved with
# itself, scored 26-125 MB/s on cat-mixed — a five-fold spread that no
# threshold can read.
measure_parsefile() {
  local s i dir="$MARSPOT_BENCH_RUN_DIR/parsefile"
  mkdir -p "$dir"
  for s in cat-ascii cat-mixed cat-cjk cat-emoji cat-styled cat-highlighted; do
    : > "$CUR_DIR/parsefile-$s.samples"
    : > "$CUR_DIR/parsefile-$s.insn"
    rm -rf "$dir/sessions"; mkdir -p "$dir/sessions/1"
    MARSPOT_STATE_DIR="$dir" MARSPOT_SESSION_ID=1 \
      "$MARSPOT" --bench "parse:$SCENARIOS_DIR/$s.bin" >/dev/null
    for i in 1 2 3 4 5; do
      rm -rf "$dir/sessions"; mkdir -p "$dir/sessions/1"
      MARSPOT_STATE_DIR="$dir" MARSPOT_SESSION_ID=1 \
        parse_trial "$CUR_DIR/parsefile-$s" "$SCENARIOS_DIR/$s.bin"
    done
  done
  rm -rf "${dir:?}"
}
echo "==> headless parse, file-backed scrollback (5 trials, taking median)"
run_block parsefile measure_parsefile

# p99_trials <kind> <arg> <n> <out> — n runs of `--bench <kind>:<arg>`
# after one discarded warm-up, the p99 of each appended to <out>
p99_trials() {
  local kind=$1 arg=$2 n=$3 out=$4 i
  : > "$out"
  "$MARSPOT" --bench "$kind:$arg" >/dev/null 2>&1
  for ((i = 0; i < n; i++)); do
    "$MARSPOT" --bench "$kind:$arg" 2>/dev/null \
      | python3 -c "import sys, json; print(json.load(sys.stdin)['p99_ns'])" >> "$out"
  done
}

measure_render() { p99_trials render 1000 3 "$CUR_DIR/render.samples"; }
echo "==> headless render (3 trials, taking median p99)"
run_block render measure_render

# Scroll-down read-path gate, over the in-memory scrollback a process
# without a session id gets (the file-backed one belongs to L3s).
#
# Cold-cache scroll used to force the disk-backed scrollback through an
# env switch and evict its pages first; that switch and the disk variant
# are gone (the file-backed scrollback is on for every L3, off
# elsewhere), and `--bench scroll-cold` now walks the same in-memory
# scrollback as `scroll`.  Kept so its baseline key keeps a reading.
measure_scroll() {
  p99_trials scroll "$SCENARIOS_DIR/scroll-history.bin" 5 "$CUR_DIR/scroll.samples"
  p99_trials scroll-cold "$SCENARIOS_DIR/scroll-history.bin" 5 "$CUR_DIR/scroll-cold.samples"
}
echo "==> headless scroll and scroll-cold (5 trials each, taking median p99)"
run_block scroll measure_scroll

# ---- binary size: sub-second, so part of the fast gate --------------
# judged against bench/size-budget.json, see bin/_size_budget.py
python3 "$ROOT/bin/_size_budget.py" --self-test > "$CUR_DIR/size-selftest.txt" \
  || { cat "$CUR_DIR/size-selftest.txt"; echo "bin/_size_budget.py --self-test failed" >&2; exit 1; }
tail -1 "$CUR_DIR/size-selftest.txt"
echo "==> binary sizes"
for bin in $(python3 "$ROOT/bin/_size_budget.py" binaries); do
  if [[ -x "$(marspot_bin "$bin")" ]]; then
    stat -f%z "$(marspot_bin "$bin")" > "$CUR_DIR/size-$bin.txt"
    printf "    %-16s %s bytes\n" "$bin" "$(cat "$CUR_DIR/size-$bin.txt")"
  fi
done

if [[ $MODE == "full" ]]; then
  # The product in its window: shell, core and one session, one pane,
  # the same trial script and the same certificate the other terminals
  # in the snapshot were read with.  This is the figure that is divided
  # by the best other terminal's.
  echo "==> live PTY through the product's window (this takes a minute)"
  (cd "$ROOT" && MEASURE_TERMS=marspot MEASURE_ROUNDS="${BENCH_APP_ROUNDS:-4}" \
    bash bin/_remote-measure-others-runner.sh > "$CUR_DIR/app.json" 2> "$CUR_DIR/measure-app.log") || true
  grep -E '^==> (grid|\[marspot)' "$CUR_DIR/measure-app.log" || true

  # Same settle between stages, for the same reason as after the build:
  # by this point the run has already pushed a few hundred MB through a
  # pty and written the disk hard, and the stage that follows is the one
  # most sensitive to both.  Without it the L3 numbers came out 40 %
  # below what the identical binary produced when `bin/measure-l3.sh`
  # was run on its own (106.5 vs 194.5 MB/s on ascii) — a measurement
  # reporting the run's own wake, not the product.
  [[ "$BENCH_SETTLE_S" != "0" ]] && sleep "$BENCH_SETTLE_S"

  # ...and the session alone: every pane a user has is an L3 with a
  # file-backed scrollback, and this is the instruction count a change
  # to marspot-session or marspot-term moves.
  #
  # This was never wired up: the script existed, bench.sh only ever
  # READ its output file, and the probe that produced it had been
  # deleted with the L4 shelld (888bf1b) - so the file went stale, then
  # past the 7-day guard, and the product path vanished from the gate
  # entirely.  Running it here closes that loop.
  echo "==> live PTY through L3 (the shipped path)"
  (cd "$ROOT" && ./bin/measure-l3.sh > "$CUR_DIR/measure-l3.log" 2>&1) || true
  tail -6 "$CUR_DIR/measure-l3.log"
  cp "$ROOT/bench/results/l3-throughput.json" "$CUR_DIR/l3.json" || true

  # Nine panes producing output at once.  One session alone cannot show
  # what sessions cost each other: shared caches, the disk they all
  # write their scrollback to, cores they compete for.  The instructions
  # are summed over the nine and gate; the rate is what the nine
  # absorbed together.
  #
  # This replaces a check that read a nine-window snapshot of three
  # terminals from a file no gate path produced, and so failed on every
  # run.
  MULTI_N="$(python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get('multi_session_l3',{}).get('sessions',0))" "$BASELINE")"
  MULTI_S="$(python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get('multi_session_l3',{}).get('scenario',''))" "$BASELINE")"
  if [[ "$MULTI_N" != "0" && -n "$MULTI_S" ]]; then
    echo "==> $MULTI_N sessions absorbing $MULTI_S at once"
    (cd "$ROOT" && ./bin/measure-l3.sh --sessions "$MULTI_N" --scenario "$MULTI_S" \
      > "$CUR_DIR/measure-l3-multi.log" 2>&1) || true
    tail -3 "$CUR_DIR/measure-l3-multi.log"
    cp "$ROOT/bench/results/l3-multi.json" "$CUR_DIR/l3-multi.json" || true
  fi

  bench_key_latency

  bench_window_gauges
fi

# ---- gate evaluation ---------------------------------------------------

# Verdicts, the table, the kept record and the history: bin/_bench_gate.py.
# Its staleness rule is checked first: a gate that can no longer tell an
# old competitor snapshot from a fresh one must not print a comparison.
if [[ "$MODE" == "full" ]]; then
  python3 "$ROOT/bin/_bench_gate.py" --self-test | tail -1
  [[ ${PIPESTATUS[0]} -eq 0 ]] || { echo "bin/_bench_gate.py --self-test failed" >&2; exit 1; }
fi
set +e
python3 "$ROOT/bin/_bench_gate.py" "$BASELINE" "$CUR_DIR" "$MODE" "$UPDATE" "$ROOT"
PERF_RC=$?
set -e

# ---- SSIM gate (opt-in via --ssim or implied by --full) -----------------
if (( WITH_SSIM )); then
  echo
  echo "==> Phase 9 SSIM gate (threshold ≥ 0.98)"
  set +e
  "$ROOT/bin/font-snapshot-check.sh"
  SSIM_RC=$?
  set -e
  if (( SSIM_RC == 0 )); then
    echo "SSIM GATE PASSED"
  else
    echo "SSIM GATE FAILED"
  fi
else
  SSIM_RC=0
fi

# Combined exit: a regression or an SSIM failure → 1; nothing over its
# limit but a gating row without a valid reading → 3.
if (( PERF_RC == 1 || SSIM_RC != 0 )); then
  exit 1
fi
exit "$PERF_RC"

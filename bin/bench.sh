#!/usr/bin/env bash
# bin/bench.sh — perf gate.
#
# Two tiers:
#   bin/bench.sh           default: headless `--bench parse` + `--bench
#                          render` only.  ~1-2 seconds, run on every
#                          commit / before push.
#   bin/bench.sh --full    also runs the live PTY pipeline through
#                          bin/measure.sh (1-2 minutes), AND the
#                          Phase 9 SSIM gate.  Use before merging to
#                          develop or when chasing a perf fix.
#
#   bin/bench.sh --ssim    add Phase 9 visual-regression SSIM gate
#                          (per docs/font-rendering-design.md §9).
#                          Implied by --full;  pass standalone to
#                          run only the SSIM gate after a fast perf
#                          run.
#
#   bin/bench.sh --update-baseline
#       Re-write bench/baseline.json with the current measurements'
#       MBps minus the configured tolerance.  Use after an
#       intentional perf-affecting change.  Skips the SSIM chain.
#
# Reads bench/baseline.json + bench/font-rendering/snapshots/*.png.
# Exits non-zero on any regression (perf OR SSIM).  Per-line PASS/FAIL
# / current/floor goes to stdout; one-line summary at the end says
# GATE PASSED or GATE FAILED for perf, plus SSIM GATE PASSED / FAILED
# when --ssim is on.

set -euo pipefail

# See bin/test.sh — inside a marspot terminal the inherited
# MARSPOT_SESSION_ID makes every headless-bench `Terminal::new` write
# the REAL state dir's sessions/<id>/scrollback.bin.  Unset first.
unset MARSPOT_SESSION_ID

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"   # marspot_bin resolves CARGO_TARGET_DIR
# BASELINE is overridable via env so tests can point at synthesised
# fixture baselines without touching the real one.
BASELINE="${BASELINE:-$ROOT/bench/baseline.json}"
SCENARIOS_DIR="$ROOT/bench/scenarios"

MODE=fast
UPDATE=0
WITH_SSIM=0
for arg in "$@"; do
  case "$arg" in
    --full)              MODE=full ;;
    --update-baseline)   UPDATE=1 ;;
    --ssim)              WITH_SSIM=1 ;;
    --help|-h)
      sed -n '2,20p' "$0"; exit 0 ;;
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

# Pre-flight: in --full mode the gate computes vs-best-other ratios
# from baseline.competitors_snapshot.  Refuse to run if that snapshot
# is stale (>7 days), otherwise silently-stale numbers can flip the
# gate verdict (perf-attack E1).  Fast tier doesn't use this snapshot
# so the check is mode-gated.
if [[ $MODE == "full" ]]; then
  # MARSPOT_BENCH_ALLOW_STALE_COMPETITORS=1 bypasses this check. Use it
  # when you accept that the vs-best-other ratio is computed against
  # old competitor numbers — e.g. for a quick verification run when
  # you can't refresh measure-other.sh right now. The age is still
  # printed so the cost is visible.
  python3 - "$BASELINE" "${MARSPOT_BENCH_ALLOW_STALE_COMPETITORS:-0}" <<'PY' || exit 2
import json, sys, datetime
b = json.load(open(sys.argv[1]))
allow_stale = sys.argv[2] == "1"
captured = b.get("competitors_snapshot", {}).get("captured_at")
if not captured:
    print("competitors_snapshot.captured_at missing — refresh via "
          "bin/measure-other.sh and set captured_at in baseline.json.",
          file=sys.stderr)
    sys.exit(2)
try:
    cap = datetime.date.fromisoformat(captured)
except ValueError as e:
    print(f"competitors_snapshot.captured_at unparseable ({captured!r}): {e}",
          file=sys.stderr)
    sys.exit(2)
age = (datetime.date.today() - cap).days
if age > 7:
    msg = f"competitors_snapshot is stale: {age} days old (limit 7)."
    if allow_stale:
        print(f"WARN: {msg} Running anyway (MARSPOT_BENCH_ALLOW_STALE_COMPETITORS=1).",
              file=sys.stderr)
    else:
        print(msg, file=sys.stderr)
        print(f"Refresh via bin/measure-other.sh (or set "
              f"MARSPOT_BENCH_ALLOW_STALE_COMPETITORS=1 to skip).",
              file=sys.stderr)
        sys.exit(2)
PY
fi

# Make sure scenarios exist; regenerate if missing (cheap, deterministic).
if [[ ! -f "$SCENARIOS_DIR/cat-ascii.bin" || ! -f "$SCENARIOS_DIR/scroll-history.bin" ]]; then
  echo "==> generating bench scenarios"
  "$ROOT/bin/gen-scenarios.sh" >/dev/null
fi

# Build release unconditionally — cargo is incremental, so an
# up-to-date tree is a sub-second no-op.  The previous "only if the
# binary is missing" check silently benched a stale binary forever
# once one existed: on the mini the gate measured a Jun 6 build for
# a month of runs (caught 2026-07-11 via byte-identical size numbers
# across an objc2 major upgrade).
echo "==> building marspot (release)"
( cd "$ROOT" && cargo build --release 2>&1 | tail -3 )



# Let the machine come down off the build before measuring anything.
#
# The per-scenario warm-up trial below already pays the cold-CACHE cost,
# which is a different thing: a `cargo build --release` with fat LTO
# pins every core for minutes, and the cores are still hot (and clocked
# down) the instant it exits.  Measured 2026-08-19 on mini: headless
# parse inside `--full` read cjk 187.2 / emoji 126.6, while the same
# binary re-measured by hand a few minutes later read 215.7 / 140.7 —
# 13-15 % apart, enough to fail floors that nothing had regressed past.
#
# This does not fix a bench host with a permanent tenant (mini runs
# another project's test suite most of the day); it fixes the part
# this script causes itself.  `BENCH_SETTLE_S=0` opts out.
BENCH_SETTLE_S="${BENCH_SETTLE_S:-8}"
if [[ "$BENCH_SETTLE_S" != "0" ]]; then
  echo "==> settling ${BENCH_SETTLE_S}s after build"
  sleep "$BENCH_SETTLE_S"
  # A fixed wait is not enough on the remote host: a fat-LTO build
  # leaves load high for longer than 8 s, and the FIRST scenario is
  # measured right into it.  Four remote runs on one commit — 185.5 /
  # 202.8 / 203.3 / 207.6 MB/s on cat-cjk when the run included a
  # build, then 229.5 / 235.3 / 238.2 on the next run when the build
  # was a no-op.  Half this metric's gate failures were the machine,
  # not the code.  Wait for load to actually come down, bounded so a
  # busy host still gets measured (the load gate refuses the hopeless
  # ones before we get here).
  _settle_waited=0
  while [[ "$_settle_waited" -lt 45 ]]; do
    _l1="$(sysctl -n vm.loadavg 2>/dev/null | awk '{print $2}')"
    [[ -z "$_l1" ]] && break
    if awk -v l="$_l1" 'BEGIN { exit !(l < 2.5) }'; then
      [[ "$_settle_waited" -gt 0 ]] && echo "    load down to $_l1 after ${_settle_waited}s"
      break
    fi
    sleep 3
    _settle_waited=$((_settle_waited + 3))
  done
  if [[ "$_settle_waited" -ge 45 ]]; then
    echo "    still warm after ${_settle_waited}s (load1=$_l1) — measuring anyway"
  fi
fi

# ---- collect current measurements ---------------------------------------

CUR_DIR=$(mktemp -d)
trap 'rm -rf "$CUR_DIR"' EXIT

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
echo "==> headless parse (5 trials each, taking median)"
for s in cat-ascii cat-mixed cat-cjk cat-emoji; do
  : > "$CUR_DIR/parse-$s.samples"
  "$(marspot_bin marspot)" --bench "parse:$SCENARIOS_DIR/$s.bin" >/dev/null
  for i in 1 2 3 4 5; do
    "$(marspot_bin marspot)" --bench "parse:$SCENARIOS_DIR/$s.bin" \
      | python3 -c "import sys,json; print(json.load(sys.stdin)['bytes_per_sec'])" \
      >> "$CUR_DIR/parse-$s.samples"
  done
done

echo "==> headless render (3 trials, taking median p99)"
: > "$CUR_DIR/render.samples"
"$(marspot_bin marspot)" --bench render:1000 >/dev/null
for trial in 1 2 3; do
  "$(marspot_bin marspot)" --bench render:1000 \
    | python3 -c "import sys, json; print(json.load(sys.stdin)['p99_ns'])" \
    >> "$CUR_DIR/render.samples"
done

# Scroll-down read-path gate.  Whatever scrollback variant the env
# selects (memory by default, disk if MARSPOT_DISK_SCROLLBACK is set) is
# what gets measured — the floor in baseline.json must be calibrated
# for the corresponding default.  When the default flips this gate
# auto-tracks via --update-baseline.
echo "==> headless scroll (5 trials, taking median p99)"
: > "$CUR_DIR/scroll.samples"
"$(marspot_bin marspot)" --bench scroll:"$SCENARIOS_DIR/scroll-history.bin" >/dev/null
for trial in 1 2 3 4 5; do
  "$(marspot_bin marspot)" --bench scroll:"$SCENARIOS_DIR/scroll-history.bin" \
    | python3 -c "import sys, json; print(json.load(sys.stdin)['p99_ns'])" \
    >> "$CUR_DIR/scroll.samples"
done

# Cold-cache scroll: forces disk-backed scrollback and asks the kernel
# to evict its resident pages (madvise(DONTNEED)) before walking.
# Measures the realistic worst case — a user returning to scrollback
# hours after the writes, when the unified buffer cache has reclaimed
# pages.  Catches disk-path read regressions even when memory is the
# default (so this gate is meaningful both pre- and post-flip).
echo "==> headless scroll-cold (disk-on, 5 trials, taking median p99)"
: > "$CUR_DIR/scroll-cold.samples"
MARSPOT_DISK_SCROLLBACK=1 "$(marspot_bin marspot)" \
  --bench scroll-cold:"$SCENARIOS_DIR/scroll-history.bin" >/dev/null 2>&1
for trial in 1 2 3 4 5; do
  MARSPOT_DISK_SCROLLBACK=1 "$(marspot_bin marspot)" \
    --bench scroll-cold:"$SCENARIOS_DIR/scroll-history.bin" 2>/dev/null \
    | python3 -c "import sys, json; print(json.load(sys.stdin)['p99_ns'])" \
    >> "$CUR_DIR/scroll-cold.samples"
done

# ---- binary size + idle memory ----------------------------------------
# Cheap (sub-second) so we can include them in the fast gate.  Catches
# regressions like accidental dep bloat or per-session memory growth.
echo "==> binary sizes"
for bin in marspot mcli; do
  if [[ -x "$(marspot_bin "$bin")" ]]; then
    stat -f%z "$(marspot_bin "$bin")" > "$CUR_DIR/size-$bin.txt"
    printf "    %-6s %s bytes\n" "$bin" "$(cat "$CUR_DIR/size-$bin.txt")"
  fi
done

echo "==> idle memory (3 trials each, taking median)"
for bin in marspot mcli; do
  if [[ ! -x "$(marspot_bin "$bin")" ]]; then continue; fi
  : > "$CUR_DIR/rss-$bin.samples"
  for trial in 1 2 3; do
    # Spawn a fresh instance and sample its OWN PID — do NOT pkill the
    # binary by name first.  Earlier code did `pkill -x "$bin"` to
    # nuke stale instances, but that's friendly-fire on any parallel
    # bench (e.g. an active-9x-soak run already in flight gets killed
    # by a sanity bench.sh invocation).  We measure $pid directly so
    # other instances are irrelevant; perf-attack E7.
    "$(marspot_bin "$bin")" >/dev/null 2>&1 &
    pid=$!
    disown 2>/dev/null || true
    sleep 1.5  # let the binary settle past startup allocs
    rss=$(ps -o rss= -p "$pid" 2>/dev/null | tr -d ' ')
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    sleep 0.1
    [[ -n "$rss" ]] && echo "$rss" >> "$CUR_DIR/rss-$bin.samples"
  done
  if [[ -s "$CUR_DIR/rss-$bin.samples" ]]; then
    printf "    %-6s %s KiB\n" "$bin" \
      "$(python3 -c "import sys; xs=sorted(int(x) for x in open('$CUR_DIR/rss-$bin.samples')); print(xs[len(xs)//2])")"
  fi
done

if [[ $MODE == "full" ]]; then
  echo "==> live PTY (this takes a minute)"
  # No pkill of the marspot binary by name — friendly-fire risk against
  # a parallel active-9x-soak / soak run.  measure.sh manages its own
  # mcli lifecycle by PID; that's sufficient.  perf-attack E7.
  (cd "$ROOT" && ./bin/measure.sh > "$CUR_DIR/measure.log" 2>&1) || true
  cp "$ROOT/bench/results/cross-terminal.json" "$CUR_DIR/live.json" || true

  # Same settle between stages, for the same reason as after the build:
  # by this point the run has already pushed a few hundred MB through a
  # pty and written the disk hard, and the stage that follows is the one
  # most sensitive to both.  Without it the L3 numbers came out 40 %
  # below what the identical binary produced when `bin/measure-l3.sh`
  # was run on its own (106.5 vs 194.5 MB/s on ascii) — a measurement
  # reporting the run's own wake, not the product.
  [[ "$BENCH_SETTLE_S" != "0" ]] && sleep "$BENCH_SETTLE_S"

  # ...and the SHIPPED path.  measure.sh drives mcli: one process, one
  # thread, in-RAM scrollback.  Every pane a user has is an L3 with a
  # file-backed scrollback, and on 2026-08-19 that path measured 35-41
  # MB/s against mcli's 93-105 - the gate was watching a pipeline
  # nobody runs.
  #
  # This was never wired up: the script existed, bench.sh only ever
  # READ its output file, and the probe that produced it had been
  # deleted with the L4 shelld (888bf1b) - so the file went stale, then
  # past the 7-day guard, and the product path vanished from the gate
  # entirely.  Running it here closes that loop.
  echo "==> live PTY through L3 (the shipped path)"
  (cd "$ROOT" && ./bin/measure-l3.sh > "$CUR_DIR/measure-l3.log" 2>&1) || true
  cp "$ROOT/bench/results/l3-throughput.json" "$CUR_DIR/l3.json" || true
fi

# ---- gate evaluation ---------------------------------------------------

set +e
python3 - "$BASELINE" "$CUR_DIR" "$MODE" "$UPDATE" <<'PY'
import json, os, sys, glob

baseline_path, cur_dir, mode, update_str = sys.argv[1:5]
do_update = update_str == "1"

baseline = json.load(open(baseline_path))
results = {"pass": [], "fail": [], "current": {}}

def load_parse(scenario):
    p = os.path.join(cur_dir, f"parse-{scenario}.samples")
    if not os.path.exists(p): return None
    samples = sorted(int(x) for x in open(p).read().split() if x.strip())
    if not samples: return None
    median = samples[len(samples) // 2]
    return median / 1024 / 1024  # MB/s

def load_render():
    p = os.path.join(cur_dir, "render.samples")
    if not os.path.exists(p): return None
    samples = sorted(int(x) for x in open(p).read().split() if x.strip())
    if not samples: return None
    return {"p99_ns": samples[len(samples) // 2]}

def load_scroll():
    p = os.path.join(cur_dir, "scroll.samples")
    if not os.path.exists(p): return None
    samples = sorted(int(x) for x in open(p).read().split() if x.strip())
    if not samples: return None
    return {"p99_ns": samples[len(samples) // 2]}

def load_scroll_cold():
    p = os.path.join(cur_dir, "scroll-cold.samples")
    if not os.path.exists(p): return None
    samples = sorted(int(x) for x in open(p).read().split() if x.strip())
    if not samples: return None
    return {"p99_ns": samples[len(samples) // 2]}

_L3_THROUGHPUT = None
_L3_LOADED = False

def _l3_throughput():
    # bench/results/l3-throughput.json — the L3 PARSE rate (shell→core→L3
    # scroll_push throughput), produced by bin/measure-l3.sh. INFORMATIONAL
    # ONLY — printed by --full, NOT fed into the live / vs-best gate.
    #
    # It used to say here that this could not be gated: the competitor
    # numbers were `time cat` ABSORPTION rates (cat dumps into buffers
    # and returns, parse catches up async) while this is the PARSE rate,
    # ~0.4x of it — apples-to-oranges, and true at the time.
    #
    # It stopped being true on 2026-08-19, when every live trial (all
    # terminals, this probe included) grew a DSR round-trip: `CSI 6 n`
    # cannot be answered until everything queued ahead of it has been
    # processed, so both sides now report work the terminal has
    # CONFIRMED it consumed.  Same metric, so it gates — see
    # `mars_l3_MBps_min` in the baseline.
    #
    # The 0.4x did not go away.  It is the real distance between mcli
    # and a shipped pane, and the shipped pane is the number a user
    # lives with.  Stale guard 7 days; gitignored/transient so absence
    # just means "skip".
    global _L3_THROUGHPUT, _L3_LOADED
    if _L3_LOADED:
        return _L3_THROUGHPUT
    _L3_LOADED = True
    p = os.path.normpath(os.path.join(os.path.dirname(baseline_path), "results", "l3-throughput.json"))
    if os.path.exists(p):
        import time
        age_days = (time.time() - os.path.getmtime(p)) / 86400.0
        if age_days <= 7:
            try:
                _L3_THROUGHPUT = json.load(open(p))
            except Exception:
                _L3_THROUGHPUT = None
    return _L3_THROUGHPUT

def live_measurement_is_trustworthy():
    # `vs_best_other` divides the L3 number by a frozen competitor
    # figure, so it inherits the L3 measurement's conditions exactly.
    l3 = _l3_throughput()
    if l3 is None:
        return True
    hl = l3.get("_host_load1", -1.0)
    return not (isinstance(hl, (int, float)) and hl > 5.0)

def load_live(scenario):
    # The SHIPPED path first.  `vs_best_other` asks "how does marspot
    # compare to the best other terminal", and the honest subject of
    # that question is what a user's pane does (core -> L3, file-backed
    # scrollback), not mcli.  Until 2026-08-19 this could only be the
    # mcli number, because the L3 probe was broken and the two metrics
    # were not comparable anyway; both are fixed now — same certified
    # protocol on both sides.
    l3 = _l3_throughput()
    if l3 is not None:
        bps = l3.get(scenario, {}).get("bytes_per_sec", 0)
        if bps > 0:
            return bps / 1e6
    return _load_live_mcli(scenario)

def _load_live_mcli(scenario):
    # marspot's live cat-* number for the vs-best-other comparison.  MUST
    # be the same metric as the competitors it's compared against — the
    # `time cat` ABSORPTION rate co-measured in competitors_snapshot. (The
    # L3 parse rate in l3-throughput.json is a different, slower metric;
    # see _l3_throughput — it is NOT used here.)  Falls back to measure.sh's
    # standalone-mcli live.json only when the snapshot lacks marspot.
    marspot_snap = baseline.get("competitors_snapshot", {}).get("marspot", {})
    co = marspot_snap.get(f"{scenario}_MBps")
    if isinstance(co, (int, float)) and co > 0:
        return float(co)
    p = os.path.join(cur_dir, "live.json")
    if not os.path.exists(p): return None
    j = json.load(open(p))
    s = j.get(scenario, {}).get("marspot", {})
    bps = s.get("bytes_per_sec", 0)
    if bps <= 0: return None
    return bps / 1024 / 1024

def best_other_mbps(baseline, sid):
    # Best competitor throughput for a scenario, across every recorded
    # competitor (iterm2, warp, ghostty, …).  Single source of truth
    # for BOTH the gate check and --update-baseline floor relocking —
    # they diverged once before (update used max(iterm2, warp) while
    # the gate included ghostty), which locks floors the very next
    # gate run fails.  Excluded by design:
    #   - terminal: OS-vendor reference floor, not a competitor.
    #   - marspot:  the subject; it lives in this dict only because
    #               the refresh script measures it in the same
    #               sequential cycle as the competitors (fair load
    #               conditions), but it can't compete with itself.
    cs = baseline["competitors_snapshot"]
    key = f"{sid}_MBps"
    excluded = {"terminal", "marspot"}
    vals = [
        v[key] for name, v in cs.items()
        if isinstance(v, dict) and name not in excluded and key in v
    ]
    return max(vals) if vals else 0

def check(label, current, floor, lower_better=False):
    if current is None:
        results["pass"].append(("skip", label, "no measurement", None))
        return
    ok = (current <= floor) if lower_better else (current >= floor)
    bucket = "pass" if ok else "fail"
    results[bucket].append((bucket, label, current, floor))
    results["current"][label] = current

def fmt_num(n):
    if n is None: return "-"
    if isinstance(n, str): return n
    return f"{n:.1f}"

# The shipped path.  Gated since 2026-08-19 (see `_l3_throughput` for why
# it could not be before): this is what a user's pane does, and it had
# been invisible to the gate for two months.
if mode == "full":
    l3 = _l3_throughput()
    if l3 is not None:
        # A contended host cannot measure this.  The probe keeps the
        # fastest of five trials precisely to survive a busy machine,
        # but past a point every window is contended and the number
        # stops being about marspot: the same build read 169.9 MB/s at
        # load1 4.5 and 115.7 at load1 6.3 on cjk.  Report that as
        # "not measured", not as a regression — a red that means
        # "someone else was compiling" trains people to ignore reds.
        host_load = l3.get("_host_load1", -1.0)
        too_busy = isinstance(host_load, (int, float)) and host_load > 5.0
        for entry in baseline["scenarios"]:
            sid = entry["id"]
            floor = entry.get("mars_l3_MBps_min")
            if not isinstance(floor, (int, float)) or floor <= 0:
                continue
            bps = l3.get(sid, {}).get("bytes_per_sec", 0)
            cur = bps / 1e6 if bps > 0 else None
            if too_busy:
                results["pass"].append(
                    ("skip", f"L3 {sid}", f"host busy (load1 {host_load})", None)
                )
                continue
            check(f"L3 {sid}", cur, float(floor))
        parts = []
        for sid in ("cat-ascii", "cat-mixed", "cat-cjk", "cat-emoji"):
            bps = l3.get(sid, {}).get("bytes_per_sec", 0)
            if bps > 0:
                parts.append(f"{sid.removeprefix('cat-')} {bps/1024/1024:.0f}")
        if parts:
            print("L3 parse rate (MiB/s, informational, not gated): " + ", ".join(parts))
    print("live gate source: competitors_snapshot.marspot (`time cat` absorption rate, "
          "competitor-comparable)")

# Parse + ratio per scenario
for entry in baseline["scenarios"]:
    sid = entry["id"]
    cur_parse = load_parse(sid)
    check(f"parse {sid:10}", cur_parse, entry["mars_parse_MBps_min"])

    if mode == "full":
        cur_live = load_live(sid)
        trust = live_measurement_is_trustworthy()
        if not trust:
            results["pass"].append(("skip", f"live  {sid:10}", "host busy", None))
            results["pass"].append(("skip", f"vs-best {sid:10}", "host busy", None))
        else:
            check(f"live  {sid:10}", cur_live, entry["mars_live_MBps_min"])
            best_other = best_other_mbps(baseline, sid)
            if cur_live is not None and best_other > 0:
                ratio = cur_live / best_other
                check(f"vs-best {sid:10}", ratio, entry["mars_vs_best_other_min"])

# Render
render = load_render()
if render is not None:
    p99_us = render["p99_ns"] / 1000
    check("render p99 (µs)", p99_us, baseline["render_full_repaint"]["p99_us_max"], lower_better=True)

# Scroll (read-path under simulated downward scrolling)
scroll = load_scroll()
if scroll is not None and "scroll_repaint" in baseline:
    p99_us = scroll["p99_ns"] / 1000
    check("scroll p99 (µs)", p99_us, baseline["scroll_repaint"]["p99_us_max"], lower_better=True)

# Scroll-cold (disk-backed, post-MADV_DONTNEED)
scroll_cold = load_scroll_cold()
if scroll_cold is not None and "scroll_cold_repaint" in baseline:
    p99_us = scroll_cold["p99_ns"] / 1000
    check("scroll-cold p99 (µs)", p99_us, baseline["scroll_cold_repaint"]["p99_us_max"], lower_better=True)

# Binary size: lower-better, ceiling = baseline value
for bin_name, ceiling in baseline.get("binary_size_bytes_max", {}).items():
    if bin_name.startswith("_"):
        continue
    p = os.path.join(cur_dir, f"size-{bin_name}.txt")
    if not os.path.exists(p):
        continue
    bytes_now = int(open(p).read().strip())
    check(f"size {bin_name:6} (bytes)", bytes_now, ceiling, lower_better=True)

# Idle memory (KiB): lower-better
for bin_name, ceiling in baseline.get("memory_idle_kb_max", {}).items():
    if bin_name.startswith("_"):
        continue
    p = os.path.join(cur_dir, f"rss-{bin_name}.samples")
    if not os.path.exists(p):
        continue
    samples = sorted(int(x) for x in open(p).read().split() if x.strip())
    if not samples:
        continue
    median = samples[len(samples) // 2]
    check(f"rss  {bin_name:6} (KiB)", median, ceiling, lower_better=True)

# Multi-session vs-best-other ratio gate (the structural protection
# against silent competitive slippage — marspot and competitors can both
# slow down and the parse/render gate would still pass; this catches
# it).  Reads the latest bench-run.sh snapshot (cross-terminal.json
# symlink); doesn't run anything itself.  Sub-millisecond.
#
# **--full only.**  The numbers in the snapshot are single-trial
# multi-session-9x / scrollback-1m measurements; per perf.md and
# baseline.json's own _comment, they're ±10–20 % volatile to thermal /
# foreground-load on the dev box.  Floors are locked clean-machine
# (≥3-trial median outside the harness), so dev-box pre-push runs flap
# even when nothing regressed.  Gate them at `--full` (intended to run
# on `ssh mini` via bench-remote.sh — clean idle Apple Silicon) and
# leave fast pre-push deterministic.
multi_cfg = baseline.get("multi_session_thresholds")
if multi_cfg and mode == "full":
    snap_path = os.path.join(os.path.dirname(baseline_path), "..", "bench", "results", "cross-terminal.json")
    snap_path = os.path.normpath(snap_path)
    snap = None
    snap_age_hours = None
    if os.path.exists(snap_path):
        try:
            snap = json.load(open(snap_path))
            mtime = os.path.getmtime(snap_path)
            import datetime, time
            snap_age_hours = (time.time() - mtime) / 3600.0
        except Exception:
            snap = None

    max_age = multi_cfg.get("snapshot_max_age_hours", 168)
    if snap is None:
        results["fail"].append((
            "fail",
            "multi-session snapshot",
            "missing",
            "bench-run.sh first",
        ))
    elif snap_age_hours is not None and snap_age_hours > max_age:
        results["fail"].append((
            "fail",
            "multi-session snapshot",
            f"{snap_age_hours:.0f}h old",
            f"≤{max_age}h",
        ))
    else:
        for sid, cfg in multi_cfg.get("scenarios", {}).items():
            metric = cfg["metric"]
            scen = snap.get("scenarios", {}).get(sid, {})
            mars_v = scen.get("marspot", {}).get("metrics", {}).get(metric)
            others = []
            for tid in ("iterm", "terminal"):
                v = scen.get(tid, {}).get("metrics", {}).get(metric)
                if v is not None:
                    others.append(v)
            if mars_v is None or not others:
                results["pass"].append((
                    "skip",
                    f"vs-best {sid:14}",
                    "no data",
                    None,
                ))
                continue
            check(f"throughput {sid:10}", mars_v, cfg["mars_throughput_min"])
            ratio = mars_v / max(others)
            check(f"vs-best   {sid:10}", ratio, cfg["mars_vs_best_other_min"])

# Print
print()
header = f"{'metric':<24} {'current':>10} {'floor':>10}   {'verdict'}"
print(header)
print("-" * len(header))
all_rows = results["pass"] + results["fail"]
for verdict, label, current, floor in all_rows:
    cur_s = fmt_num(current)
    floor_s = fmt_num(floor)
    verdict_s = {"pass": "✓ pass", "fail": "✗ FAIL", "skip": "- skip"}[verdict]
    print(f"{label:<24} {cur_s:>10} {floor_s:>10}   {verdict_s}")
print()

n_fail = len(results["fail"])
if n_fail == 0:
    print(f"GATE PASSED ({len(results['pass'])} checks)")
else:
    print(f"GATE FAILED ({n_fail} regression(s) of {len(results['pass']) + n_fail} checks)")

if do_update:
    if n_fail > 0:
        print()
        print("--update-baseline refused: gate already failing, fix the regressions first")
        sys.exit(1)
    # Update baseline floors based on current measurements.
    print()
    print("==> updating baseline floors with 7% headless / 10% live safety margin")
    for entry in baseline["scenarios"]:
        sid = entry["id"]
        cp = load_parse(sid)
        if cp is not None:
            entry["mars_parse_MBps_min"] = round(cp * 0.93)
        if mode == "full":
            cl = load_live(sid)
            if cl is not None:
                entry["mars_live_MBps_min"] = round(cl * 0.90)
                best_other = best_other_mbps(baseline, sid)
                if best_other > 0:
                    entry["mars_vs_best_other_min"] = round(cl / best_other * 0.90, 2)
    # Lower-better ceilings: use math.ceil so the noise margin actually
    # carries.  round() on something like 1.9 * 1.30 = 2.47 → 2 wipes
    # the margin out and the gate flaps on identical code; ceil → 3.
    # Per-metric multipliers reflect observed run-to-run noise:
    #   - parse: ~5 % noise → 1.07 margin (handled in scenarios loop)
    #   - render: ~30 % noise (GPU thermal) → 1.50 to absorb worst case
    #   - scroll / scroll-cold: ~30 % noise (low-µs regime, one cache
    #     eviction skews p99) → 1.30
    # If a metric needs a different margin than auto-derived, edit
    # baseline.json directly and skip --update-baseline for that field.
    import math
    if render is not None:
        baseline["render_full_repaint"]["p99_us_max"] = math.ceil(render["p99_ns"] / 1000 * 1.50)
    if scroll is not None and "scroll_repaint" in baseline:
        baseline["scroll_repaint"]["p99_us_max"] = math.ceil(scroll["p99_ns"] / 1000 * 1.30)
    if scroll_cold is not None and "scroll_cold_repaint" in baseline:
        baseline["scroll_cold_repaint"]["p99_us_max"] = math.ceil(scroll_cold["p99_ns"] / 1000 * 1.30)
    # Size: 10 % ceiling above current.
    if "binary_size_bytes_max" in baseline:
        for bin_name in list(baseline["binary_size_bytes_max"].keys()):
            if bin_name.startswith("_"): continue
            p = os.path.join(cur_dir, f"size-{bin_name}.txt")
            if os.path.exists(p):
                cur = int(open(p).read().strip())
                baseline["binary_size_bytes_max"][bin_name] = round(cur * 1.10)
    # Memory: 20 % ceiling above current (more variance than size).
    if "memory_idle_kb_max" in baseline:
        for bin_name in list(baseline["memory_idle_kb_max"].keys()):
            if bin_name.startswith("_"): continue
            p = os.path.join(cur_dir, f"rss-{bin_name}.samples")
            if os.path.exists(p):
                samples = sorted(int(x) for x in open(p).read().split() if x.strip())
                if samples:
                    median = samples[len(samples) // 2]
                    baseline["memory_idle_kb_max"][bin_name] = round(median * 1.20)
    # Multi-session thresholds: refresh from latest cross-terminal.json
    # snapshot.  10 % safety margin on marspot throughput AND on the
    # vs-best ratio — the ratio is what protects against silent slip,
    # so re-locking it from the same observation is the right move.
    #
    # `snap` is only loaded in `--full` mode (see line ~430); in fast
    # tier the variable name doesn't exist at all, so the bare
    # `is not None` check used to NameError out and abort the whole
    # baseline write.  Guard by checking `mode == "full"` here too.
    multi_cfg = baseline.get("multi_session_thresholds")
    snap_local = locals().get("snap") if mode == "full" else None
    if multi_cfg and snap_local is not None:
        snap = snap_local
        for sid, cfg in multi_cfg.get("scenarios", {}).items():
            metric = cfg["metric"]
            scen = snap.get("scenarios", {}).get(sid, {})
            mars_v = scen.get("marspot", {}).get("metrics", {}).get(metric)
            others = []
            for tid in ("iterm", "terminal"):
                v = scen.get(tid, {}).get("metrics", {}).get(metric)
                if v is not None:
                    others.append(v)
            if mars_v is not None and others:
                cfg["mars_throughput_min"] = round(mars_v * 0.90)
                cfg["mars_vs_best_other_min"] = round(mars_v / max(others) * 0.90, 2)
    json.dump(baseline, open(baseline_path, "w"), indent=2)
    print("==> wrote", baseline_path)

sys.exit(1 if n_fail > 0 else 0)
PY
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

# Combined exit: any failure → 1.  Perf gate's row-by-row PASS/FAIL
# already printed above; SSIM gate prints its own per-snapshot lines.
if (( PERF_RC != 0 || SSIM_RC != 0 )); then
  exit 1
fi
exit 0

#!/usr/bin/env bash
# bin/bench.sh — perf gate.
#
# Two tiers:
#   bin/bench.sh           default: headless `--bench parse` + `--bench
#                          render` only.  ~1-2 seconds, run on every
#                          commit / before push.
#   bin/bench.sh --full    also runs the live PTY pipeline through
#                          bin/measure.sh (1-2 minutes).  Use before
#                          merging to develop or when chasing a perf
#                          fix.
#
#   bin/bench.sh --update-baseline
#       Re-write bench/baseline.json with the current measurements'
#       MBps minus the configured tolerance.  Use after an
#       intentional perf-affecting change.
#
# Reads bench/baseline.json.  Exits non-zero on any regression.
# Per-line PASS/FAIL / current/floor goes to stdout; a one-line
# summary at the end says GATE PASSED or GATE FAILED.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASELINE="$ROOT/bench/baseline.json"
SCENARIOS_DIR="$ROOT/bench/scenarios"

MODE=fast
UPDATE=0
for arg in "$@"; do
  case "$arg" in
    --full)              MODE=full ;;
    --update-baseline)   UPDATE=1 ;;
    --help|-h)
      sed -n '2,20p' "$0"; exit 0 ;;
    *)
      echo "unknown arg: $arg (try --help)" >&2; exit 2 ;;
  esac
done

if [[ ! -f "$BASELINE" ]]; then
  echo "missing $BASELINE" >&2
  exit 2
fi

# Make sure scenarios exist; regenerate if missing (cheap, deterministic).
if [[ ! -f "$SCENARIOS_DIR/cat-ascii.bin" || ! -f "$SCENARIOS_DIR/scroll-history.bin" ]]; then
  echo "==> generating bench scenarios"
  "$ROOT/bin/gen-scenarios.sh" >/dev/null
fi

# Build release if missing or stale.
if [[ ! -x "$ROOT/target/release/mars" ]]; then
  echo "==> building mars (release)"
  ( cd "$ROOT" && cargo build --release 2>&1 | tail -3 )
fi

# ---- collect current measurements ---------------------------------------

CUR_DIR=$(mktemp -d)
trap 'rm -rf "$CUR_DIR"' EXIT

# N=5 trials per parse measurement; we take the median.  Single-run
# numbers vary ~5–8 % on the same code (thermal / scheduler / kernel
# cache), so a 1-shot gate flaps without contributing real signal.
echo "==> headless parse (5 trials each, taking median)"
for s in cat-ascii cat-mixed cat-cjk cat-emoji; do
  : > "$CUR_DIR/parse-$s.samples"
  for i in 1 2 3 4 5; do
    "$ROOT/target/release/mars" --bench "parse:$SCENARIOS_DIR/$s.bin" \
      | python3 -c "import sys,json; print(json.load(sys.stdin)['bytes_per_sec'])" \
      >> "$CUR_DIR/parse-$s.samples"
  done
done

echo "==> headless render (3 trials, taking median p99)"
: > "$CUR_DIR/render.samples"
for trial in 1 2 3; do
  "$ROOT/target/release/mars" --bench render:1000 \
    | python3 -c "import sys, json; print(json.load(sys.stdin)['p99_ns'])" \
    >> "$CUR_DIR/render.samples"
done

# Scroll-down read-path gate.  Whatever scrollback variant the env
# selects (memory by default, disk if MARS_DISK_SCROLLBACK is set) is
# what gets measured — the floor in baseline.json must be calibrated
# for the corresponding default.  When the default flips this gate
# auto-tracks via --update-baseline.
echo "==> headless scroll (5 trials, taking median p99)"
: > "$CUR_DIR/scroll.samples"
for trial in 1 2 3 4 5; do
  "$ROOT/target/release/mars" --bench scroll:"$SCENARIOS_DIR/scroll-history.bin" \
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
for trial in 1 2 3 4 5; do
  MARS_DISK_SCROLLBACK=1 "$ROOT/target/release/mars" \
    --bench scroll-cold:"$SCENARIOS_DIR/scroll-history.bin" 2>/dev/null \
    | python3 -c "import sys, json; print(json.load(sys.stdin)['p99_ns'])" \
    >> "$CUR_DIR/scroll-cold.samples"
done

# ---- binary size + idle memory ----------------------------------------
# Cheap (sub-second) so we can include them in the fast gate.  Catches
# regressions like accidental dep bloat or per-session memory growth.
echo "==> binary sizes"
for bin in mars mcli; do
  if [[ -x "$ROOT/target/release/$bin" ]]; then
    stat -f%z "$ROOT/target/release/$bin" > "$CUR_DIR/size-$bin.txt"
    printf "    %-6s %s bytes\n" "$bin" "$(cat "$CUR_DIR/size-$bin.txt")"
  fi
done

echo "==> idle memory (3 trials each, taking median)"
for bin in mars mcli; do
  if [[ ! -x "$ROOT/target/release/$bin" ]]; then continue; fi
  : > "$CUR_DIR/rss-$bin.samples"
  for trial in 1 2 3; do
    pkill -x "$bin" 2>/dev/null || true
    sleep 0.2
    "$ROOT/target/release/$bin" >/dev/null 2>&1 &
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
  pkill -x mars 2>/dev/null || true
  (cd "$ROOT" && ./bin/measure.sh > "$CUR_DIR/measure.log" 2>&1) || true
  cp "$ROOT/bench/results/cross-terminal.json" "$CUR_DIR/live.json" || true
fi

# ---- gate evaluation ---------------------------------------------------

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

def load_live(scenario):
    p = os.path.join(cur_dir, "live.json")
    if not os.path.exists(p): return None
    j = json.load(open(p))
    s = j.get(scenario, {}).get("mars", {})
    bps = s.get("bytes_per_sec", 0)
    if bps <= 0: return None
    return bps / 1024 / 1024

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

# Parse + ratio per scenario
for entry in baseline["scenarios"]:
    sid = entry["id"]
    cur_parse = load_parse(sid)
    check(f"parse {sid:10}", cur_parse, entry["mars_parse_MBps_min"])

    if mode == "full":
        cur_live = load_live(sid)
        check(f"live  {sid:10}", cur_live, entry["mars_live_MBps_min"])
        # vs best other
        best_other = max(
            baseline["competitors_snapshot"]["iterm2"][f"{sid}_MBps"],
            baseline["competitors_snapshot"]["warp"][f"{sid}_MBps"],
        )
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
                best_other = max(
                    baseline["competitors_snapshot"]["iterm2"][f"{sid}_MBps"],
                    baseline["competitors_snapshot"]["warp"][f"{sid}_MBps"],
                )
                if best_other > 0:
                    entry["mars_vs_best_other_min"] = round(cl / best_other * 0.90, 2)
    if render is not None:
        baseline["render_full_repaint"]["p99_us_max"] = round(render["p99_ns"] / 1000 * 1.10)
    if scroll is not None and "scroll_repaint" in baseline:
        baseline["scroll_repaint"]["p99_us_max"] = round(scroll["p99_ns"] / 1000 * 1.30)
    if scroll_cold is not None and "scroll_cold_repaint" in baseline:
        baseline["scroll_cold_repaint"]["p99_us_max"] = round(scroll_cold["p99_ns"] / 1000 * 1.30)
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
    json.dump(baseline, open(baseline_path, "w"), indent=2)
    print("==> wrote", baseline_path)

sys.exit(1 if n_fail > 0 else 0)
PY

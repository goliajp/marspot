#!/usr/bin/env bash
# bin/test-awake-cost.sh — what the app costs while its panes are awake.
#
# bin/test-idle-wakeups.sh holds the app to almost nothing when every
# pane sits at a prompt.  A pane running a coding agent is never that:
# the agent redraws its status line every second, so the pane is never
# quiet and the supervisor's clock never rests.  This lays out N panes,
# each running bin/_fake-agent-tui.py as its shell's foreground job (a
# status line rewritten once a second with the cursor saved and
# restored), and reads each process's timer wake-ups per second and CPU
# with bin/_idle_wakeups.py, for N = 1, 4 and 12.
#
# Each layout is held to a ceiling of timer wake-ups per second, all the
# app's processes summed (AWAKE_BUDGETS).  Prints one table and writes
# it, with date, commit, host and load, to
# bench/results/awake-cost-<stamp>.json.
#
# Opens a real window and is a measurement, so it runs on the runner:
#   E2E_LOCK=bench bin/test-e2e-remote.sh test-awake-cost.sh
# which copies the JSON back into this tree's bench/results/.
#
#   AWAKE_LAYOUTS   default "1x1 2x2 4x3"
#   AWAKE_SETTLE    seconds after the panes are up (15)
#   AWAKE_WINDOW    measured window in seconds (25)
#   AWAKE_BUDGETS   ceilings by pane count, "panes:wake-ups/s,..."; see
#                   below for where the defaults come from
#
# Exit codes, those of bin/measure-idle-marspot.sh: 0 measured and
# within the ceilings; 1 a layout measured on a quiet host was over its
# ceiling; 2 no reading, or a fake agent that did not draw through the
# window; 3 the host was busy, so the reading is printed and written but
# is no verdict next to other work.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# test-e2e-remote.sh forwards every IDLE_* of the caller; these would
# change what measure-idle-marspot.sh measures or how it judges it
unset IDLE_LAYOUTS IDLE_WAKEUP_BUDGET IDLE_RESIDENT IDLE_WINDOW_STATE

LAYOUTS="${AWAKE_LAYOUTS:-1x1 2x2 4x3}"
# Measured on mini once the supervisor stopped beating once a second
# while panes are awake (2026-11-22): one pane 15.6, 16.1, 15.6; four
# 38.2, 36.6, 38.0; twelve 106.9, 104.0, 106.9, and 110.0 measured alone.
# The ceilings are the highest of those and a little of the spread on top.
BUDGETS="${AWAKE_BUDGETS-1:18,4:41,12:112}"
SETTLE="${AWAKE_SETTLE:-15}"
WINDOW="${AWAKE_WINDOW:-25}"
work="$(mktemp -d /tmp/marspot-awake.XXXXXX)"
trap 'rm -rf "${work:?}"' EXIT

rc=0
for layout in $LAYOUTS; do
  n=$(( ${layout%x*} * ${layout#*x} ))
  budget="$(tr ',' '\n' <<<"$BUDGETS" | sed -n "s/^$n://p")"
  echo "════ $n panes, each a fake agent redrawing its status line once a second ════"
  FAKE_AGENT_TICKS="$work/ticks-$n" IDLE_COLS="${layout%x*}" IDLE_ROWS="${layout#*x}" \
    IDLE_WAKEUP_BUDGET="$budget" \
    IDLE_SHELL="$ROOT/bin/_fake-agent-shell.sh" IDLE_SETTLE="$SETTLE" IDLE_WINDOW="$WINDOW" \
    IDLE_READINGS=1 IDLE_JSON="$work/$n.json" "$ROOT/bin/measure-idle-marspot.sh"
  one=$?
  # a failure outranks no reading, which outranks no verdict
  for c in 1 2 3; do
    if [[ $rc == "$c" || $one == "$c" ]]; then rc=$c; break; fi
  done
done

commit="${E2E_COMMIT:-}"
[[ -z $commit && -d $ROOT/.git ]] && commit="$(git -C "$ROOT" rev-parse --short=8 HEAD)"
mkdir -p "$ROOT/bench/results"
out="$ROOT/bench/results/awake-cost-$(date +%Y%m%d-%H%M%S).json"
python3 - "$work" "$out" "$commit" "$(hostname -s)" "$SETTLE" "$WINDOW" "$rc" "$BUDGETS" $LAYOUTS <<'PY'
import datetime, json, os, sys
work, out, commit, host, settle, window, rc, budgets = sys.argv[1:9]
settle, window, rc = float(settle), float(window), int(rc)
budgets = {int(k): float(v) for k, v in (kv.split(":") for kv in budgets.split(",") if kv)}
runs, broken = [], []
for layout in sys.argv[9:]:
    c, r = layout.split("x")
    n = int(c) * int(r)
    path = os.path.join(work, f"{n}.json")
    if not os.path.exists(path):
        broken.append(f"{n} panes: no reading")
        continue
    reading = json.load(open(path))["readings"][0]
    # every agent drew a frame a second from before the settle to after the window
    tdir = os.path.join(work, f"ticks-{n}")
    ticks = sorted(int(open(os.path.join(tdir, f)).read()) for f in os.listdir(tdir) if f.isdigit()) if os.path.isdir(tdir) else []
    if len(ticks) != n or ticks[0] < settle + window - 2:
        broken.append(f"{n} panes: {len(ticks)} fake agents drew, fewest frames {ticks[:1]}, "
                      f"want {n} with at least {settle + window - 2:.0f}")
    kinds = {}
    for p in reading["processes"]:
        k = kinds.setdefault(p["name"], {"n": 0, "wakeups_per_sec": 0.0, "idle_wakeups_per_sec": 0.0, "cpu_pct": 0.0})
        k["n"] += 1
        k["wakeups_per_sec"] += p["intr_wakeups_per_sec"]
        k["idle_wakeups_per_sec"] += p["idle_wakeups_per_sec"]
        k["cpu_pct"] += p["cpu_pct"]
    for k in kinds.values():
        k["wakeups_per_sec_each"] = round(k["wakeups_per_sec"] / k["n"], 2)
        k["cpu_pct_each"] = round(k["cpu_pct"] / k["n"], 4)
        for f in ("wakeups_per_sec", "idle_wakeups_per_sec"):
            k[f] = round(k[f], 2)
        k["cpu_pct"] = round(k["cpu_pct"], 4)
    runs.append({
        "panes": n, "window_secs": reading["window_secs"], "budget_wakeups_per_sec": budgets.get(n),
        "load1_first": reading["load1_first"], "load1_max": reading["load1_max"],
        "agent_frames_min": ticks[0] if ticks else None,
        "total": {"wakeups_per_sec": reading["intr_wakeups_per_sec"],
                  "idle_wakeups_per_sec": reading["idle_wakeups_per_sec"], "cpu_pct": reading["cpu_pct"]},
        "by_kind": kinds, "processes": reading["processes"],
    })

print()
print("════ awake cost: timer wake-ups/s and CPU % of one core, per process kind ════")
print(f"{'panes':>5}  {'process':<16} {'n':>3}  {'wk/s sum':>9} {'wk/s each':>9}  {'CPU% sum':>9} {'CPU% each':>9}  load1")
for r in runs:
    for name in ("marspot-shell", "marspot-core", "marspot-session"):
        k = r["by_kind"].get(name)
        if k is None:
            print(f"{r['panes']:>5}  {name:<16} {0:>3}  (no process)")
            continue
        print(f"{r['panes']:>5}  {name:<16} {k['n']:>3}  {k['wakeups_per_sec']:>9.2f} {k['wakeups_per_sec_each']:>9.2f}"
              f"  {k['cpu_pct']:>9.3f} {k['cpu_pct_each']:>9.3f}  {r['load1_first']}..{r['load1_max']}")
    t = r["total"]
    ceiling = f"  ceiling {r['budget_wakeups_per_sec']}" if r["budget_wakeups_per_sec"] is not None else ""
    print(f"{r['panes']:>5}  {'all':<16} {len(r['processes']):>3}  {t['wakeups_per_sec']:>9.2f} {'':>9}  {t['cpu_pct']:>9.3f}{ceiling}")
doc = {
    "suite": "awake-cost",
    "date": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
    "commit": commit or None, "host": host,
    "agent": "status line rewritten once a second, cursor saved and restored (bin/_fake-agent-tui.py)",
    "settle_secs": settle, "window_secs": window, "exit": 2 if broken else rc,
    "problems": broken, "runs": runs,
}
with open(out, "w") as f:
    json.dump(doc, f, indent=1)
    f.write("\n")
print(f"written: {out}")
for b in broken:
    print(f"FAIL: {b}")
sys.exit(2 if broken else rc)
PY

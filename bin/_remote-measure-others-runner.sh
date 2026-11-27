#!/usr/bin/env bash
# CARVE-OUT: pre-existing runner over the size limit; it shrinks on every touch
# bin/_remote-measure-others-runner.sh — runs on the bench host, driven
# from bin/remote-measure-others.sh. Emits a single JSON object on
# stdout shaped like:
#
#   {
#     "terminal": {"cat-ascii_MBps": 12.3, ...},
#     "iterm":    {...},
#     "warp":     {...}
#   }
#
# Direct invocation from a dev machine is supported but unusual — the
# canonical entry point is bin/remote-measure-others.sh on the
# triggering host.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

source "$ROOT/bin/_lib.sh"   # LIVE_MIN_BYTES + write_live_matrix_script
SCENARIOS=(cat-ascii cat-mixed cat-cjk cat-emoji)
TRIALS=3
# Console mode drives all four terminals. ssh mode drives what it can
# reach (see the ssh block below). Terminal.app onboarding is
# the one-time Automation grant during install-bench-launchagent.sh
# first-fire, after which subsequent LaunchAgent-spawned runs reuse
# the cached TCC scope.
# Measurement order matters: marspot in the SAME cycle as every
# competitor so all five are measured under identical load. The
# cycle is sequential — at any moment exactly one terminal is doing
# PTY work — so per-cell contention is zero. This is what makes the
# vs-best-other ratio fair: every cell is the terminal vs an idle box.
# (cooldown between terminals lets caches / scheduler settle.)
#
# The marspot row is the product: marspot-shell, marspot-core and one
# marspot-session, built from this tree, one window with one pane.
# MEASURE_TERMS narrows the set (space separated) for an experiment.
# shellcheck disable=SC2206
TERMS=(${MEASURE_TERMS:-iterm warp ghostty terminal marspot})
# The cycle is run this many times, so every terminal is read in every
# round and a drift of the machine lands on all of them.  With three or
# more rounds the first is a warm-up and is left out of the figure.
ROUNDS=${MEASURE_ROUNDS:-3}
# 4 scenarios × 3 trials × ~2 MB/s worst case = ~96 s budget; cap at
# 120 s so a single-terminal failure (Warp keystroke mangle, paste
# warning, frozen surface) doesn't hang the whole refresh for 10 min
# of wait-loop dead time. Terminals that don't produce a marker by
# then are skipped — their previous-day snapshot stays via per-
# terminal merge in remote-measure-others.sh.
MARKER_TIMEOUT_S=${MEASURE_MARKER_TIMEOUT_S:-120}
# Seconds between one terminal's close and the next one's launch.
# Lets the host cool: GPU thermal, scheduler-cached affinity, page-cache
# warmth for /tmp scenarios. 3 s is empirically enough for variance
# to settle to ~5 % run-to-run.
COOLDOWN_S=3

# From ssh.  The ssh login of the console user reaches the console
# session's window server, so Ghostty and the product are started from
# this shell as they are, as this user.  The AppleScript-driven three
# answer only while they already run in the console session (asked per
# round below), and Warp's keystroke path needs an Accessibility grant
# for whoever sends it; an ssh run that cannot reach one leaves its
# snapshot from the same host as it was.
SSH_MODE=0
if [[ -n "${SSH_CONNECTION:-}" ]]; then
  SSH_MODE=1
  echo "==> ssh mode: Ghostty and the product start from this login; iTerm/Warp/Terminal only when they answer" >&2
fi

# Make sure scenarios are present. gen-scenarios.sh writes the .bin
# files this script's cat targets read.
if [[ ! -f bench/scenarios/cat-ascii.bin ]]; then
  echo "==> generating scenarios" >&2
  ./bin/gen-scenarios.sh >&2
fi

# Our short terminal id -> the name AppleScript knows it by.
applescript_name() {
  case "$1" in
    iterm)    echo "iTerm" ;;
    warp)     echo "Warp" ;;
    terminal) echo "Terminal" ;;
    ghostty)  echo "Ghostty" ;;
    *)        echo "$1" ;;
  esac
}

build_one_cmd() {
  # Was: one `cat` of the scenario file per trial, inlined into a
  # multi-KB command string.  Two things were wrong with it and both
  # showed up as competitor numbers that could not be reconciled with
  # each other (2026-08-18: the same terminal, the same day, 0.27 s and
  # 0.08 s for "cat-cjk" — different byte counts, not different loads).
  #
  #   - byte count.  A single pass of an 8 MiB scenario does not keep
  #     `cat` blocked long enough to measure a fast terminal at all;
  #     the marspot-side script of the time had already moved to
  #     LIVE_MIN_BYTES and this script had not, so the two sides of
  #     the comparison were pushing different amounts of data.
  #   - certification.  `cat` returning proves the bytes reached the
  #     PTY, not that the terminal processed them; a terminal that
  #     drops under pressure finishes early and wins.
  #
  # write_live_matrix_script (bin/_lib-live.sh) fixes both and hands every
  # terminal the same file to run.
  local marker=$1
  # per terminal and round (a slow one may still read its own); run dir only
  local script="$MARSPOT_BENCH_RUN_DIR/live-matrix-$2.sh"
  local paths=()
  local s
  for s in "${SCENARIOS[@]}"; do
    paths+=("$ROOT/bench/scenarios/$s.bin")
  done
  write_live_matrix_script "$script" "$marker" "$TRIALS" "${paths[@]}"
  # Run it by its own shebang rather than naming a shell here. The
  # generator moved to zsh when the timing moved to `$EPOCHREALTIME`
  # (macOS bash is 3.2 and has no such variable) and this line was left
  # saying `bash`, so every terminal was handed a zsh script to run
  # under bash: it died on `zmodload` at line 2, wrote no marker, and
  # all five drivers timed out looking for one. Naming the shell twice
  # is what let the two drift apart; now only the shebang says it.
  echo "$script; exit"
}

# ---- pre-flight inventory + trap cleanup ----------------------------
# Test must not leave residue. Inventory records what state each app
# was in BEFORE we touched it, and the cleanup trap uses that to undo
# only what we created. Three categories of state are tracked:
#
#   - iTerm / Terminal.app windows: AppleScript-trackable by integer
#     id. Drivers return the ids of windows they create; trap closes
#     exactly those (no collateral close of the user's other work).
#   - App processes: each driver checks right before its launch whether
#     the app runs; only an instance its launch brought up is recorded,
#     and only recorded pids are stopped (bin/_app-pids.sh).  One that
#     was up, or that someone else started between rounds, is left alone.
#   - Temp wrapper files: always cleaned at exit; markers live in the
#     run dir.
SESSION_DIR="/tmp/marspot-bench-session-$$"
mkdir -p "$SESSION_DIR"

# Sweep orphan session dirs from prior runs that were SIGKILL'd before
# their EXIT trap fired (parent killed -9, host rebooted mid-run, etc).
# A dir whose pid is no longer running can be safely removed — bench
# wrapper / marker leaks from that aborted run are addressed below in
# the normal cleanup paths.
for stale in /tmp/marspot-bench-session-*; do
  [[ -d "$stale" ]] || continue
  pid=${stale##*-}
  [[ "$pid" == "$$" ]] && continue
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "==> pre-flight: sweeping orphan session dir $stale (pid $pid dead)" >&2
    rm -rf "$stale" 2>/dev/null || true
  fi
done

# shellcheck source=bin/_app-pids.sh
source "$ROOT/bin/_app-pids.sh"
# Snapshot pre-existing state, by the identity the drivers record (for
# the log: each driver decides again right before its own launch).
is_up() { [[ -n $(app_pid "$1") ]] && echo 1 || echo 0; }
PRE_ITERM=$(is_up com.googlecode.iterm2)
PRE_TERMINAL=$(is_up com.apple.Terminal)
PRE_WARP=$(is_up dev.warp.Warp-Stable)
PRE_GHOSTTY=$(is_up com.mitchellh.ghostty)
printf 'PRE_ITERM=%s\nPRE_TERMINAL=%s\nPRE_WARP=%s\nPRE_GHOSTTY=%s\n' \
  "$PRE_ITERM" "$PRE_TERMINAL" "$PRE_WARP" "$PRE_GHOSTTY" \
  > "$SESSION_DIR/inventory"
echo "==> pre-flight: iterm=$PRE_ITERM terminal=$PRE_TERMINAL warp=$PRE_WARP ghostty=$PRE_GHOSTTY" >&2

# macOS ships bash 3.2 → no `declare -A`. Use one var per terminal.
WIN_iterm=""
WIN_terminal=""

# What this run started is stopped by the pids recorded when it started
# it, never by a pattern.  The drivers record into
# $SESSION_DIR/<terminal>.pids: iTerm, Terminal and Warp only when that
# launch brought the app up; Ghostty always, since its driver starts a
# process of its own.
GHOSTTY_PIDS="$SESSION_DIR/ghostty.pids"
app_binary() {
  case "$1" in
    iterm)    echo "/Applications/iTerm.app/Contents/MacOS/iTerm2" ;;
    terminal) echo "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal" ;;
    warp)     echo "/Applications/Warp.app/Contents/MacOS/stable" ;;
    ghostty)  echo "/Applications/Ghostty.app/Contents/MacOS/ghostty" ;;
  esac
}

stop_app() {
  [[ -s $SESSION_DIR/$1.pids ]] || return 0
  stop_recorded_pids "$SESSION_DIR/$1.pids" "$(app_binary "$1")"
  echo "==> stopped what this run started of $(applescript_name "$1")" >&2
}

cleanup() {
  local rc=$?
  # Disarm the trap before doing any work so a fail inside cleanup
  # doesn't re-enter via set -e (we'd recurse forever). Also drop -e
  # so partial cleanup is preferred over an early abort.
  trap - EXIT INT TERM
  set +e

  # iTerm / Terminal: precise close by the ids we recorded — leaves the
  # user's other windows untouched even if both were already running.
  [[ -n "$WIN_iterm" ]]    && bin/drivers/iterm.sh    close-windows "$WIN_iterm"    >/dev/null 2>&1 || true
  [[ -n "$WIN_terminal" ]] && bin/drivers/terminal.sh close-windows "$WIN_terminal" >/dev/null 2>&1 || true

  # what this run started of each app, by its recorded pids
  local t
  for t in iterm terminal warp ghostty; do stop_app "$t"; done

  # the product, if a round was interrupted while it was up
  bin/drivers/marspot.sh close-app >/dev/null 2>&1 || true

  # driver wrappers; the markers go with the run dir
  rm -f /tmp/iterm-wrapper-* /tmp/terminal-wrapper-* /tmp/ghostty-wrapper-* /tmp/warp-wrapper-* 2>/dev/null || true
  rm -f /tmp/marspot-live-matrix-*.sh 2>/dev/null || true
  rm -rf "$SESSION_DIR" 2>/dev/null || true
  bench_run_end
  echo "==> cleanup done (script rc=$rc)" >&2
  exit "$rc"
}
trap cleanup EXIT INT TERM
bench_run_begin   # the product runs from the run dir's copies
# Each round's marker, in a dir made fresh for this run and owned by the
# user running it: a round that was skipped or timed out leaves no file,
# and only files here are read.
MARKER_DIR=$(mktemp -d "$MARSPOT_BENCH_RUN_DIR/markers.XXXXXX")

# Per-terminal close — runs immediately after that terminal's marker
# completes (or times out). Closes only this terminal; the others
# haven't been launched yet thanks to the sequential cycle below. Only
# recorded pids are stopped, as in the EXIT trap.
close_one() {
  local t=$1
  case "$t" in
    iterm)    [[ -z $WIN_iterm ]] || bin/drivers/iterm.sh close-windows "$WIN_iterm" >/dev/null 2>&1 || true ;;
    terminal) [[ -z $WIN_terminal ]] || bin/drivers/terminal.sh close-windows "$WIN_terminal" >/dev/null 2>&1 || true ;;
  esac
  case "$t" in
    # The app quits when its one pane's shell exits.  This stops what
    # is left of it, by the pids its own state dir recorded, and
    # removes that state dir.
    marspot) bin/drivers/marspot.sh close-app >&2 || true ;;
    *)       stop_app "$t" ;;
  esac
  return 0
}

# The grid every terminal that can be told its size opens with: the one
# the product's window gives its pane.  Learned by asking, once.
GRID_COLS=""
GRID_ROWS=""
probe_grid() {
  local out="$SESSION_DIR/grid" probe="$MARSPOT_BENCH_RUN_DIR/grid-probe.sh"
  printf '#!/bin/sh\nstty size > %s.tmp < /dev/tty\nmv %s.tmp %s\n' "$out" "$out" "$out" > "$probe"
  chmod 0755 "$probe"
  bin/drivers/marspot.sh run-app "$probe" >&2 || return 0
  local i
  for ((i = 0; i < 100; i++)); do
    [[ -s $out ]] && break
    sleep 0.1
  done
  bin/drivers/marspot.sh close-app >&2 || true
  if [[ -s $out ]]; then
    read -r GRID_ROWS GRID_COLS < "$out"
    echo "==> grid: ${GRID_COLS} columns x ${GRID_ROWS} rows (the product's pane)" >&2
  else
    echo "==> grid: the product did not report one; terminals open at their defaults" >&2
  fi
}
case " ${TERMS[*]} " in
  *" marspot "*) probe_grid ;;
esac

load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
TIMED_OUT=""

# The product was built a moment ago inside this lock hold; let the
# build's own load drain before the first reading.
for ((i = 0; i < 18; i++)); do
  awk -v l="$(load1)" 'BEGIN { exit !(l > 2.5) }' || break
  sleep 10
done
echo "==> load1 $(load1) before the first round" >&2

# Sequential cycles: launch → wait → close → cooldown, every terminal in
# every round. At any moment exactly one terminal is doing PTY work, so
# per-cell CPU / IO contention is zero.
for ((round = 1; round <= ROUNDS; round++)); do
for t in "${TERMS[@]}"; do
  # Ghostty and marspot are started from this login and work from
  # ssh.  The AppleScript-driven three are reachable only
  # while the app is already running in the user's GUI session
  # (verified 2026-08-19: `tell application "iTerm" to count windows`
  # answered from a plain ssh shell).  So ask instead of assuming: one
  # cheap AppleEvent decides, and the skip message says what failed.
  if [[ $SSH_MODE -eq 1 && "$t" != "ghostty" && "$t" != "marspot" && "$t-${MEASURE_WARP_VIA:-}" != "warp-run-launch" ]]; then
    ae_app="$(applescript_name "$t")"
    if ! osascript -e "with timeout of 5 seconds" \
                   -e "tell application \"$ae_app\" to count windows" \
                   -e "end timeout" >/dev/null 2>&1; then
      echo "==> [${t} r${round}] skip (ssh cannot reach it — open $ae_app on the bench host first)" >&2
      continue
    fi
  fi

  # A terminal that did not finish a round is not started again: what
  # it was still doing when its window was closed runs into its next
  # round (iTerm2's emoji trials take minutes each), and a driver that
  # never produced a marker would cost its full timeout every round.
  if [[ " $TIMED_OUT " == *" $t "* ]]; then
    echo "==> [${t} r${round}] skip (did not finish an earlier round)" >&2
    continue
  fi

  marker="$MARKER_DIR/measure-${t}-r${round}.txt"
  cmd="$(build_one_cmd "$marker" "${t}-r${round}")"

  load_start="$(load1)"
  echo "==> [${t} r${round}] launch (load1 ${load_start})" >&2
  launch_rc=0
  wrapper=""
  # shellcheck disable=SC2086
  case "$t" in
    iterm)    WIN_iterm="$(bin/drivers/iterm.sh run-tabs --pid-file "$SESSION_DIR/iterm.pids" 1 "$cmd")" || launch_rc=$? ;;
    warp)     bin/drivers/warp.sh "${MEASURE_WARP_VIA:-run-single}" --pid-file "$SESSION_DIR/warp.pids" "$cmd" || launch_rc=$? ;;
    ghostty)  bin/drivers/ghostty.sh run-single --pid-file "$GHOSTTY_PIDS" "$cmd" $GRID_COLS $GRID_ROWS || launch_rc=$? ;;
    terminal) WIN_terminal="$(bin/drivers/terminal.sh run-single --pid-file "$SESSION_DIR/terminal.pids" "$cmd")" || launch_rc=$? ;;
    marspot)
      # the pane's shell is a wrapper around the same command every
      # other terminal got
      wrapper="$MARSPOT_BENCH_RUN_DIR/wrapper-$t-r$round"
      printf '#!/bin/bash\n%s\n' "$cmd" > "$wrapper"
      chmod 0755 "$wrapper"
      bin/drivers/marspot.sh run-app "$wrapper" >&2 || launch_rc=$?
      ;;
  esac

  if (( launch_rc != 0 )); then
    echo "==> [${t} r${round}] driver failed rc=$launch_rc — skipping" >&2
    continue
  fi

  saw_done=0
  for _ in $(seq 1 "$MARKER_TIMEOUT_S"); do
    if [[ -s "$marker" ]] && grep -q '==ALL_DONE==' "$marker"; then
      saw_done=1
      break
    fi
    sleep 1
  done
  if (( saw_done == 0 )); then
    echo "==> [${t} r${round}] marker not done after ${MARKER_TIMEOUT_S}s — moving on" >&2
    TIMED_OUT+=" $t"
  fi
  echo "load1 ${load_start} $(load1)" >> "$marker" 2>/dev/null || true

  close_one "$t"
  [[ -z $wrapper ]] || rm -f "$wrapper"
  sleep "$COOLDOWN_S"
done
done

# Note: window/process cleanup runs in the EXIT trap above so it
# triggers on success AND on any mid-run die (SIGINT from the
# LaunchAgent receiver, set -e from a driver, marker-wait timeout,
# anything). Don't duplicate cleanup here.

# Parse markers → JSON on stdout.
LIVE_MIN_BYTES="$LIVE_MIN_BYTES" LIVE_SENTINEL_CPR="$LIVE_SENTINEL_CPR" MEASURE_ROUNDS="$ROUNDS" \
  MEASURE_MARKER_DIR="$MARKER_DIR" \
  python3 - "${SCENARIOS[@]}" <<'PY'
import json, re, os, sys, datetime, subprocess, platform
SCENARIOS = sys.argv[1:]
# Internal driver / marker names use "iterm" (matches iterm.sh); the
# baseline.json snapshot uses "iterm2" (matches the product / bundle id
# `com.googlecode.iterm2`). Map on the way out so the merge in the
# parent script doesn't create two separate competitor entries.
TERMS = ["iterm", "warp", "ghostty", "terminal", "marspot"]
TERM_TO_KEY = {"iterm": "iterm2"}

# App bundle paths per terminal — used to probe version + bundle_id so
# each refresh records what was measured against what build. Terminal.app
# lives under /System/Applications/Utilities on macOS 13+. marspot is
# measured from the build tree; its version comes from its binaries.
APP_PATH = {
    "iterm":    "/Applications/iTerm.app",
    "warp":     "/Applications/Warp.app",
    "ghostty":  "/Applications/Ghostty.app",
    "terminal": "/System/Applications/Utilities/Terminal.app",
}

def plist_read(plist, key):
    try:
        return subprocess.check_output(
            ["defaults", "read", plist, key],
            text=True, stderr=subprocess.DEVNULL
        ).strip()
    except Exception:
        return None

def probe_version(term):
    if term == "marspot":
        # the three binaries that were just measured say what they are
        lines = subprocess.run(
            ["bin/drivers/marspot.sh", "app-version"], cwd=ROOT,
            capture_output=True, text=True).stdout.split("\n")
        m = re.match(r"marspot-shell (\S+) \(git (\S+) ", lines[0]) if lines else None
        if not m or len(lines) < 3:
            return None, "com.goliajp.marspot"
        product = "?"
        for line in open(f"{ROOT}/Cargo.toml"):
            pm = re.match(r'version = "([^"]+)"', line)
            if pm:
                product = pm.group(1)
                break
        # the copy on the bench host has no repository; the triggering
        # host says which commit it sent
        commit = os.environ.get("MARSPOT_BENCH_COMMIT") or m.group(2)
        return (f"{product} (shell {m.group(1)}, core {lines[1].strip()}, "
                f"session {lines[2].strip()}, git {commit})"), "com.goliajp.marspot"
    p = APP_PATH.get(term)
    if not p: return None, None
    plist = f"{p}/Contents/Info.plist"
    if not os.path.exists(plist): return None, None
    v = plist_read(plist, "CFBundleShortVersionString") or ""
    build = plist_read(plist, "CFBundleVersion") or ""
    bundle = plist_read(plist, "CFBundleIdentifier") or ""
    version = f"{v} (build {build})" if build and build != v else v
    return version or None, bundle or None

def probe_host():
    out = {"name": platform.node().split(".")[0],
           "arch": f"{platform.machine()} ({platform.system()} {platform.release()})"}
    try:
        sw = subprocess.check_output(["sw_vers"], text=True)
        prod, ver, build = "macOS", "?", "?"
        for line in sw.splitlines():
            if line.startswith("ProductVersion:"): ver = line.split(":", 1)[1].strip()
            if line.startswith("BuildVersion:"):   build = line.split(":", 1)[1].strip()
        out["os"] = f"{prod} {ver} (build {build})"
    except Exception:
        out["os"] = "unknown"
    return out

def median(xs):
    s = sorted(xs)
    return s[len(s) // 2]

def read_marker(path):
    """One terminal, one round: seconds per certified trial by scenario,
    the grid, the load at both ends, how many trials had no certificate."""
    by_scn, grid, load, dropped = {}, None, None, 0
    current, certified = None, False
    for line in open(path, errors="replace"):
        line = line.strip()
        m = re.match(r"^==GRID== (\d+) (\d+)", line)
        if m:
            grid = f"{m.group(2)}x{m.group(1)}"
            continue
        m = re.match(r"^==SCN== (\S+)", line)
        if m:
            current = m.group(1)
            continue
        m = re.match(r"^load1 (\S+) (\S+)", line)
        if m:
            load = [float(m.group(1)), float(m.group(2))]
            continue
        # `cpr=<row>,<col>` is the terminal's reply to "where is the
        # cursor", asked right after moving it to the sentinel position
        # behind the corpus.  Only that position certifies the trial.
        m = re.match(r"^cpr=(.*)$", line)
        if m:
            certified = m.group(1) == SENTINEL
            dropped += 0 if certified else 1
            continue
        m = re.match(r"^real\s+(\S+)$", line)
        if m and current and certified:
            certified = False
            by_scn.setdefault(current, []).append(float(m.group(1)))
    return by_scn, grid, load, dropped

ROOT = os.environ.get("PWD", os.getcwd())
# Mirrors LIVE_MIN_BYTES in bin/_lib-live.sh — passed through the
# environment by the shell half so the two cannot drift apart.
LIVE_MIN_BYTES = int(os.environ.get("LIVE_MIN_BYTES", 32 * 1024 * 1024))
SENTINEL = os.environ["LIVE_SENTINEL_CPR"]
ROUNDS = int(os.environ["MEASURE_ROUNDS"])
# this run's own markers only: a fixed path would read an earlier run's
MARKER_DIR = os.environ["MEASURE_MARKER_DIR"]
# a round taken above this 1-minute load is not a reading (the gate's
# own ceiling)
LOAD_MAX = float(os.environ.get("MARSPOT_BENCH_LOAD_MAX", "5.0"))
today = datetime.date.today().isoformat()
out = {"host": probe_host()}
loads = []
for t in TERMS:
    rounds = []
    grid = None
    for r in range(1, ROUNDS + 1):
        marker = os.path.join(MARKER_DIR, f"measure-{t}-r{r}.txt")
        if not os.path.exists(marker):
            continue
        by_scn, g, load, dropped = read_marker(marker)
        grid = g or grid
        rd = {"round": r}
        if load:
            rd["load1"] = load
            loads += load
            if max(load) > LOAD_MAX:
                rd["polluted"] = True
        if dropped:
            rd["uncertified_trials"] = dropped
        if by_scn.get("overhead"):
            rd["overhead_ms"] = round(median(by_scn["overhead"]) * 1000, 2)
        for s in SCENARIOS:
            vs = by_scn.get(s)
            if not vs:
                continue
            # Bytes actually pushed = file size x the live repeat count
            # the trial script used, NOT the file size.  Getting this
            # wrong is how one afternoon produced a "419 MB/s"
            # competitor reading that was really 105.
            size = os.path.getsize(f"{ROOT}/bench/scenarios/{s}.bin")
            reps = max(1, -(-LIVE_MIN_BYTES // size))
            rd[f"{s}_MBps"] = round(size * reps / median(vs) / 1e6, 1)  # MB = 10^6 bytes
            rd[f"{s}_trials_s"] = [round(v, 4) for v in vs]
        rounds.append(rd)
    rec = {}
    version, bundle = probe_version(t)
    if version: rec["version"] = version
    if bundle:  rec["bundle_id"] = bundle
    rec["captured_at"] = today
    rec["unit"] = "MB/s"  # 10^6 bytes per second
    rec["method"] = os.environ.get("MEASURE_METHOD") or (
                     "ssh→LaunchAgent (com.marspot.bench-trigger)"
                     if os.environ.get("XPC_SERVICE_NAME", "").startswith("com.marspot")
                     else "ssh login of the console user" if os.environ.get("SSH_CONNECTION")
                     else "console / Screen Sharing")
    if grid:
        rec["grid"] = grid
    # The figure: the median over the rounds of each round's median.
    # The first of three or more rounds is a warm-up; a round taken
    # under load is not a reading, unless that is all there is.
    used = rounds[1:] if len(rounds) >= 3 else rounds
    clean = [rd for rd in used if not rd.get("polluted")]
    if not clean and used:
        rec["polluted"] = True
    for s in SCENARIOS:
        vals = [rd[f"{s}_MBps"] for rd in (clean or used) if f"{s}_MBps" in rd]
        if vals:
            rec[f"{s}_MBps"] = median(vals)
    rec["rounds"] = rounds
    # Only emit if we got at least one MBps measurement; metadata alone
    # isn't worth surfacing.
    if any(k.endswith("_MBps") for k in rec):
        out[TERM_TO_KEY.get(t, t)] = rec
# What else the machine was doing travels with the numbers: load only
# depresses a reading.
if loads:
    out["host"]["load1_at_capture"] = max(loads)
print(json.dumps(out, indent=2))
PY

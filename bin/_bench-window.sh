#!/usr/bin/env bash
# The parts of bin/bench.sh --full that open a sandbox window: the key to
# frame latency and the window gauges.  Sourced by bin/bench.sh after the
# run dir exists; reads CUR_DIR, LAT_TARGET and run_block from it.

# shellcheck source=bin/_console.sh
source "$ROOT/bin/_console.sh"
# From ssh a window needs the console session: stop the run when this
# login cannot see it, before any minutes go into measuring.
bench_console_check() {
  [[ -n ${SSH_CONNECTION:-} ]] || return 0
  console_check "the ssh login on $(hostname -s)" /bin/sh -c "$CONSOLE_LIST" || exit 1
}

# ---- key-to-frame latency ---------------------------------------------
# One pane in one window, LAT_GATE_ROUNDS sandbox apps one after
# another, each typing 200 keys (bin/measure-latency.sh, scene type-1).
# Read from the key event's timestamp to the shell's present call
# returning: the frame is in the layer then, and the window server
# shows it at the next composite.  The `seen` scene's window captures
# would add that last step, but they load the window server while the
# frame is on its way, so they are not the latency of the product.
# Windows need the GUI login session, which an ssh login of the console
# user reaches as it is.
measure_key_latency() {
  local sd="/tmp/msp-lat-$$" dir="$CUR_DIR/lat-bins" b rc=0
  rm -rf "${CUR_DIR:?}/latency" && mkdir -p "$dir" "$CUR_DIR/latency"
  for b in marspot-shell marspot-core; do cp "$LAT_TARGET/release/$b" "$dir/$b"; done
  cp "$(marspot_bin marspot-session)" "$dir/marspot-session"
  rm -rf "${sd:?}" && mkdir -p "$sd"
  # the run dir is in the environment, so the pane script lands in it too
  (cd "$ROOT" && env MARSPOT_STATE_DIR="$sd" LAT_SIDES="ship=$dir" LAT_SCENES=type-1 \
    LAT_ROUNDS="${LAT_GATE_ROUNDS:-4}" LAT_OUT="$CUR_DIR/latency" \
    bin/_bounded.sh 900 bin/measure-latency.sh) > "$CUR_DIR/measure-latency.log" 2>&1 || rc=$?
  # a run that stopped part way leaves fewer reports: the rows read this
  echo "${rc:-0} ${LAT_GATE_ROUNDS:-4}" > "$CUR_DIR/latency.rc"
  "$ROOT/bin/kill-sandbox-marspot.sh" --state-dir "$sd" >/dev/null 2>&1 || true
  rm -rf "${sd:?}"
}
bench_key_latency() {
  bench_console_check
  # the rows can fail and can refuse a broken run, checked before minutes are spent measuring
  python3 "$ROOT/bin/_bench_latency.py" --self-test > "$CUR_DIR/latency-selftest.txt" \
    || { cat "$CUR_DIR/latency-selftest.txt"; echo "bin/_bench_latency.py --self-test failed" >&2; exit 1; }
  tail -1 "$CUR_DIR/latency-selftest.txt"
  echo "==> key press to frame, one pane (${LAT_GATE_ROUNDS:-4} runs of 200 keys)"
  run_block latency measure_key_latency
  grep -E '^(== |FAIL)' "$CUR_DIR/measure-latency.log" | sed 's/^/    /' || true
  # kept with the run's results, so the verdict can be read again later
  rm -rf "$ROOT/bench/results/latency"
  cp -R "$CUR_DIR/latency" "$ROOT/bench/results/latency"
  cp "$CUR_DIR/measure-latency.log" "$CUR_DIR/latency.host" "$CUR_DIR/latency.rc" \
    "$ROOT/bench/results/latency/" 2>/dev/null || true
}

# bench_window_gauge <name> <script> <out-var>: one gauge in a sandbox
# window of its own, its JSON to $CUR_DIR/<name>.json
bench_window_gauge() {
  local name=$1 script=$2 outvar=$3 sd="/tmp/msp-gauge-$$-$1"
  echo "==> $script (a sandbox window)"
  rm -rf "${sd:?}" && mkdir -p "$sd"
  env MARSPOT_STATE_DIR="$sd" "$outvar=$CUR_DIR/$name.json" \
    /bin/bash -l "$ROOT/bin/$script" > "$CUR_DIR/$name.log" 2>&1 || true
  grep -E '^(footprint12|startup|pane-control) |^    (round|trial) |FAIL' "$CUR_DIR/$name.log" || tail -5 "$CUR_DIR/$name.log"
  "$ROOT/bin/kill-sandbox-marspot.sh" --state-dir "$sd" || true
  rm -rf "${sd:?}"
}

bench_window_gauges() {
  bench_console_check
  # The shipped app in a window: its memory with 12 panes open, and how
  # long it takes to show a pane's output after launch.  A window needs
  # the console session, which an ssh login of the console user reaches.
  bench_window_gauge footprint measure-app-footprint.sh FP_OUT
  bench_window_gauge startup measure-startup.sh STARTUP_OUT
  bench_window_gauge panectl measure-pane-control.sh PANECTL_OUT
}

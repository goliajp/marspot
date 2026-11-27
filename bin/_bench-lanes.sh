#!/usr/bin/env bash
# The live-throughput lane of bin/bench.sh --full, and the lane selection
# of bin/bench.sh --lanes.  Sourced by bin/bench.sh after the run dir
# exists; reads ROOT, CUR_DIR, BASELINE and BENCH_SETTLE_S from it.
#
# Lanes (--lanes=a,b):
#   startup  bin/measure-startup.sh in a sandbox window: the cold and
#            warm launch of the shipped three processes
#   bps      bytes per second: the product in its window, then
#            marspot-session alone and nine of them at once
# A lanes run builds what the fast tier builds, measures only those
# lanes and judges only their rows; it never re-locks a floor.
BENCH_LANES_KNOWN="startup bps"

# bench_lanes_check <list>: exits 2 with the reason when the list names
# an unknown lane or is empty
bench_lanes_check() {
  local l
  [[ -n $1 ]] || { echo "--lanes needs a list: $BENCH_LANES_KNOWN" >&2; exit 2; }
  for l in ${1//,/ }; do
    [[ " $BENCH_LANES_KNOWN " == *" $l "* ]] \
      || { echo "unknown lane '$l' (known: $BENCH_LANES_KNOWN)" >&2; exit 2; }
  done
}

# bench_lane <name>: whether this run measures that lane
bench_lane() {
  [[ $MODE == full ]] && return 0
  [[ $MODE == lanes && ",$LANES_ARG," == *",$1,"* ]]
}

bench_bps() {
  bench_console_check
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
  echo "==> live PTY through L3 (the shipped path)"
  (cd "$ROOT" && ./bin/measure-l3.sh > "$CUR_DIR/measure-l3.log" 2>&1) || true
  tail -6 "$CUR_DIR/measure-l3.log"
  cp "$ROOT/bench/results/l3-throughput.json" "$CUR_DIR/l3.json" || true

  # Nine panes producing output at once.  One session alone cannot show
  # what sessions cost each other: shared caches, the disk they all
  # write their scrollback to, cores they compete for.  The instructions
  # are summed over the nine and gate; the rate is what the nine
  # absorbed together.
  local n s
  n="$(python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get('multi_session_l3',{}).get('sessions',0))" "$BASELINE")"
  s="$(python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get('multi_session_l3',{}).get('scenario',''))" "$BASELINE")"
  if [[ "$n" != "0" && -n "$s" ]]; then
    echo "==> $n sessions absorbing $s at once"
    (cd "$ROOT" && ./bin/measure-l3.sh --sessions "$n" --scenario "$s" \
      > "$CUR_DIR/measure-l3-multi.log" 2>&1) || true
    tail -3 "$CUR_DIR/measure-l3-multi.log"
    cp "$ROOT/bench/results/l3-multi.json" "$CUR_DIR/l3-multi.json" || true
  fi
}

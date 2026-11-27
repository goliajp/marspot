#!/usr/bin/env bash
# The live trial: how many bytes it pushes through a terminal and the
# script every terminal runs for it.  Sourced by bin/_lib.sh; do not
# execute.

# ---- live measurement sizing --------------------------------------------
# Minimum bytes a LIVE trial must push through a terminal.
#
# The `time cat` trick only measures a terminal once `cat` starts
# blocking on PTY writes.  Below that the kernel buffer swallows the
# file and `cat` returns having measured nothing — a 64 KiB scenario
# reports 0 ms on every terminal — while a few-MiB one spends most of
# its window in the warm-up region.  Measured 2026-08-18 on the emoji
# corpus: one binary reported 56 MB/s at 8 MiB and 140 MB/s at 67 MiB.
# A pipeline cannot have two throughputs; the small sample was lying,
# and it lies WORSE the faster the terminal is — the wrong bias for
# this repo.
#
# Every live consumer (bin/_remote-measure-others-runner.sh for the
# product and the competitors, bin/measure-other.sh for a manual run) cat's each scenario as many times as it takes to
# clear this bar, so both sides of a cross-terminal comparison push the
# same number of bytes.  The scenario FILES keep their original sizes:
# `--bench parse` feeds bytes directly with no PTY in the loop, so the
# headless floors in bench/baseline.json stay comparable across this
# change.
LIVE_MIN_BYTES=$((32 * 1024 * 1024))

# Repeat count for one scenario file to clear LIVE_MIN_BYTES.
live_repeat() {
  local size
  size=$(stat -f%z "$1")
  local n=$(( (LIVE_MIN_BYTES + size - 1) / size ))
  [[ "$n" -lt 1 ]] && n=1
  echo "$n"
}

# The repeated argument list: "<path> <path> ..." n times.
live_cat_args() {
  local path=$1 n=$2 out="" i=1
  while [[ $i -le $n ]]; do out="$out $path"; i=$((i + 1)); done
  echo "$out"
}

# ---- live trial scripts --------------------------------------------------
# Why a script file rather than a command string: every terminal here is
# driven differently (a wrapper script for marspot/Ghostty, AppleScript
# `write text` for iTerm, System Events keystrokes for Warp), and the
# only way to guarantee they all do byte-identical work is to hand them
# all the same path to run.  It also ends the quoting spiral of building
# a multi-KB one-liner.
#
# What the trial does, and why the second half matters:
#
#     cat <scenario> ... <scenario>     # LIVE_MIN_BYTES worth
#     printf '\033[6n'; read until R    # ask where the cursor is
#
# `cat` returning only proves the bytes reached the PTY.  A terminal
# that drops or coalesces under pressure lets `cat` finish early and
# "wins" the benchmark — measured 2026-08-18: the same terminal timed
# 0.27 s and 0.08 s for the same corpus on the same day, which is not
# a load story.  A DSR (`CSI 6 n`) cannot be answered until everything
# queued ahead of it has been processed, so the reply is the terminal
# certifying that it actually consumed the corpus.
#
# A terminal that never answers times out after 5 s and writes an empty
# `cpr=` line; parsers must treat that trial as INVALID rather than
# folding a 5 s timeout into the throughput number.

# write_live_matrix_script <out> <marker> <trials> <scenario-path>...
# The whole matrix in one terminal session, appending `==SCN== <id>`
# headers so one marker can be parsed per scenario.  Ends with
# `==ALL_DONE==` so a poller knows it is safe to read.
#
# What a trial certifies.  After the corpus the script moves the cursor
# to row 7, column 11 and asks where the cursor is.  A reply of 7;11 can
# only come from a terminal that applied that move, and the move sits
# behind the whole corpus in the byte stream: every byte before it has
# been consumed from the PTY and applied to the terminal's state.  A
# reply with any other position (answered early, or left over from an
# earlier trial) is not a certificate, and the reader drops the trial.
# Drawing is not certified, for any terminal.
#
# Before the scenarios: `==GRID==` records the rows and columns the
# terminal gave the script, and the `overhead` trials run the same
# sequence with no corpus -- what the instrument itself costs inside
# each timed region (a shell, two `stty`, one round trip).
LIVE_SENTINEL_CPR="7,11"
write_live_matrix_script() {
  local out=$1 marker=$2 trials=$3
  shift 3
  {
    # zsh, not bash, and not `/usr/bin/time -p`.
    #
    # `time -p` prints two decimals, so every reading is quantised to
    # 10 ms.  At these speeds a scenario takes about a quarter of a
    # second, which makes the step between adjacent readings worth
    # several MB/s -- and the published table says so out loud: three of
    # its four numbers landed on exactly 128.0, which is the timer
    # talking, not the terminal.
    #
    # zsh's `zsh/datetime` gives `$EPOCHREALTIME` in microseconds from
    # inside the shell, so nothing is spawned inside the timed region.
    # macOS ships bash 3.2, which has no such variable, and reading the
    # clock with `python3 -c` would charge its own startup -- about
    # 20 ms, the thing being fixed -- to the measurement.
    #
    # `read -t 5 -srd R` behaves the same in both shells; checked.
    echo '#!/bin/zsh'
    echo 'zmodload zsh/datetime'
    echo 'if [[ -z $EPOCHREALTIME ]]; then'
    echo '  print -u2 "no EPOCHREALTIME: zsh/datetime did not load"; exit 1'
    echo 'fi'
    echo "rm -f \"$marker\""
    echo "echo \"==GRID== \$(stty size < /dev/tty)\" >> \"$marker\""
    local scenario reps args t
    for scenario in overhead "$@"; do
      if [[ $scenario == overhead ]]; then
        args=" /dev/null"
      else
        reps=$(live_repeat "$scenario")
        args=$(live_cat_args "$scenario" "$reps")
      fi
      for ((t = 1; t <= trials; t++)); do
        echo "echo '==SCN== $(basename "$scenario" .bin)' >> \"$marker\""
        echo '__s=$EPOCHREALTIME'
        echo "{ /bin/zsh -c '"
        echo "  /bin/cat$args"
        echo '  stty raw -echo 2>/dev/null'
        echo '  printf "\033[7;11H\033[6n" > /dev/tty'
        echo '  IFS="[;" read -t 5 -srd R _ _row _col < /dev/tty'
        echo '  stty sane 2>/dev/null'
        echo '  printf "cpr=%s,%s\n" "$_row" "$_col" >&2'
        echo "'; } 2>> \"$marker\""
        echo '__e=$EPOCHREALTIME'
        echo "printf 'real %.6f\n' \$((__e - __s)) >> \"$marker\""
      done
    done
    echo "echo '==ALL_DONE==' >> \"$marker\""
  } > "$out"
  chmod 0755 "$out"
}

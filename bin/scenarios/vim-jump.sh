#!/usr/bin/env bash
# bin/scenarios/vim-jump.sh — vim opens a large file, jumps to bottom + back.
#
# Why this scenario exists: vim editing a long file is the canonical
# "escape-density + cursor-move + scroll" stress test for terminal
# emulators.  Each scroll redraws ~30 lines worth of cells with
# attribute changes, plus cursor positioning, plus syntax-highlight
# colour codes.  The classic vtebench scenario.
#
# Method: drive vim entirely via `-c` command-line flags — no
# interactive keystrokes involved.  This is the explicit safety
# choice over System Events keystroke driving: vim does its own
# work via -c, terminal renders the output, we measure wall time.
#
#   vim -u NONE -c 'normal G' -c 'normal gg' -c 'q!' <bigfile>
#
# Cross-terminal: yes — same vim invocation in marspot / iTerm2 /
# Terminal.app / Warp.
#
# Usage:
#   bin/scenarios/vim-jump.sh <terminal> <out-json>

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

terminal=${1:?usage: vim-jump.sh <terminal> <out-json>}
out_json=${2:?usage: vim-jump.sh <terminal> <out-json>}

N_LINES=50000
SCENARIO_FILE="$SCENARIOS_DIR/vim-jump-50k.txt"

# Generate the 50k-line file on demand.  Each line tagged with index +
# fixed-width content so the line is long enough to stress wrap +
# colourisation logic.
if [[ ! -f "$SCENARIO_FILE" ]]; then
  echo "==> generating vim-jump-50k.txt ($N_LINES lines)" >&2
  python3 - "$SCENARIO_FILE" "$N_LINES" <<'PY'
import sys
out, n = sys.argv[1], int(sys.argv[2])
with open(out, "w") as f:
    for i in range(n):
        f.write(f"line {i:06d}: lorem ipsum dolor sit amet, consectetur adipiscing elit\n")
PY
fi

RUN_DIR="$MARKER_PREFIX/vim-jump-$terminal-$$"
rm -rf "$RUN_DIR"; mkdir -p "$RUN_DIR"
WIN_IDS_FILE="$RUN_DIR/window-ids.txt"
RUN_TAG="marspot-bench-vim-$$"

# Worker: time the vim run.  -u NONE skips the user's vimrc so the
# scenario is deterministic.  Output goes to a per-trial timing file.
# Force vim to actually redraw between motions — `-c 'normal G'` in
# batch mode skips rendering, which would make the test measure vim's
# CPU not the terminal's.  An external script with explicit `redraw`
# and short `sleep`s gives the terminal real frames to draw.
SCRIPT="$RUN_DIR/jump.vim"
cat > "$SCRIPT" <<'VIM'
normal! G
redraw!
sleep 300m
normal! gg
redraw!
sleep 300m
normal! G
redraw!
sleep 300m
normal! gg
redraw!
sleep 300m
quit!
VIM

WORKER="$RUN_DIR/worker.sh"
cat > "$WORKER" <<EOF
#!/bin/sh
printf '\033]0;%s\007' "$RUN_TAG"
out="$RUN_DIR/timing.txt"
/usr/bin/time -p /usr/bin/vim -u NONE -S "$SCRIPT" "$SCENARIO_FILE" 2> "\$out"
exit 0
EOF
chmod +x "$WORKER"

cleanup_windows() {
  local ids=()
  [[ -f "$WIN_IDS_FILE" ]] || return 0
  while IFS= read -r line; do
    line=${line//[$'\r\n\t ']/}
    [[ -n "$line" ]] && ids+=("$line")
  done < "$WIN_IDS_FILE"
  (( ${#ids[@]} > 0 )) || return 0
  case "$terminal" in
    iterm)    "$ROOT/bin/drivers/iterm.sh"    close-windows "${ids[@]}" 2>/dev/null || true ;;
    terminal) "$ROOT/bin/drivers/terminal.sh" close-windows "${ids[@]}" 2>/dev/null || true ;;
  esac
}

case "$terminal" in
  marspot)     sample_proc=mcli ;;
  *)        sample_proc=$terminal ;;
esac
baseline_kib=$(rss_total_kib "$sample_proc" || echo 0)
[[ -z "$baseline_kib" ]] && baseline_kib=0

USER_APP=$(current_frontmost_app)
trap '{ cleanup_windows; rm -rf "$RUN_DIR"; restore_focus_to "$USER_APP"; } || true' EXIT INT TERM

case "$terminal" in
  marspot)
    # mcli (single-session) — vim drives the workload itself.
    kill_app marspot || true; kill_app mcli || true
    "$ROOT/bin/drivers/marspot.sh" run-shell-mcli "$WORKER"
    ;;
  iterm)
    "$ROOT/bin/drivers/iterm.sh" run-windows 1 "$WORKER" > "$WIN_IDS_FILE"
    ;;
  terminal)
    "$ROOT/bin/drivers/terminal.sh" run-single "$WORKER" > "$WIN_IDS_FILE"
    ;;
  *)
    echo "vim-jump: unsupported terminal: $terminal" >&2
    exit 2
    ;;
esac
restore_focus_to "$USER_APP"

# Wait for timing.txt — vim should finish in a few seconds.
deadline=$(( $(date +%s) + 120 ))
while [[ $(date +%s) -lt $deadline ]]; do
  [[ -f "$RUN_DIR/timing.txt" ]] && break
  sleep 0.2
done

# Settle so the terminal finishes its last frame, then sample post-RSS.
sleep 0.5
post_kib=$(rss_total_kib "$sample_proc" || echo 0)
[[ -z "$post_kib" ]] && post_kib=0

case "$terminal" in
  marspot) kill_app marspot || true; kill_app mcli || true ;;
esac

# ---- aggregate -------------------------------------------------------

python3 - "$RUN_DIR" "$terminal" "$out_json" "$N_LINES" \
                    "$baseline_kib" "$post_kib" "$RUN_TAG" <<'PY'
import json, os, sys

(run_dir, terminal, out_json, n_lines,
 baseline_kib, post_kib, run_tag) = sys.argv[1:8]
n_lines = int(n_lines)
baseline_kib, post_kib = int(baseline_kib), int(post_kib)

real_ns = None
timing_path = os.path.join(run_dir, "timing.txt")
if os.path.exists(timing_path):
    for line in open(timing_path):
        if line.startswith("real"):
            try:
                s = line.split()[1]
                if "m" in s:
                    mins, rest = s.split("m"); secs = float(rest.rstrip("s"))
                    total = float(mins)*60 + secs
                else:
                    total = float(s)
                real_ns = int(total * 1e9)
            except Exception:
                pass
            break

result = {
    "scenario": "vim-jump",
    "terminal": terminal,
    "metrics": {
        "n_lines": n_lines,
        "wall_ns": real_ns,
        "wall_s": round(real_ns / 1e9, 3) if real_ns else None,
        "rss_baseline_KiB": baseline_kib,
        "rss_post_delta_KiB": post_kib - baseline_kib if post_kib else None,
        "run_tag": run_tag,
    },
    "skipped": [] if real_ns else ["timing missing — vim may have failed"],
}

with open(out_json, "w") as f:
    json.dump(result, f, indent=2)

m = result["metrics"]
print(f"  {terminal:8s} vim-jump ({n_lines:,} lines):")
if m["wall_s"] is not None:
    print(f"    wall time              {m['wall_s']} s")
if m["rss_post_delta_KiB"] is not None:
    print(f"    RSS Δ post             {m['rss_post_delta_KiB']/1024:.0f} MiB  (baseline {baseline_kib/1024:.0f} MiB)")
PY

exit 0

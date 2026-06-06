#!/usr/bin/env bash
# bin/_remote-measure-others-mini.sh — runs on the bench host, driven
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

SCENARIOS=(cat-ascii cat-mixed cat-cjk cat-emoji)
TRIALS=3
# Terminal.app intentionally omitted: requires a one-time Automation
# permission grant on the remote host (System Settings → Privacy &
# Security → Automation), and the bench gate only consults iTerm /
# Warp anyway. Re-add `terminal` here once the host is onboarded.
TERMS=(iterm warp)
MARKER_TIMEOUT_S=600

# Make sure scenarios are present. gen-scenarios.sh writes the .bin
# files this script's cat targets read.
if [[ ! -f bench/scenarios/cat-ascii.bin ]]; then
  echo "==> generating scenarios" >&2
  ./bin/gen-scenarios.sh >&2
fi

build_one_cmd() {
  local marker=$1
  local cmd="rm -f $marker; "
  local s t
  for s in "${SCENARIOS[@]}"; do
    for ((t=1; t<=TRIALS; t++)); do
      cmd+="echo '==SCN== $s' >> $marker; "
      cmd+="/usr/bin/time -p /bin/cat $ROOT/bench/scenarios/$s.bin 2>> $marker; "
    done
  done
  cmd+="echo '==ALL_DONE==' >> $marker; "
  cmd+="exit"
  echo "$cmd"
}

# macOS ships bash 3.2 → no `declare -A`. Use one var per terminal.
WIN_iterm=""
WIN_warp=""
for t in "${TERMS[@]}"; do
  marker="/tmp/measure-${t}-all.txt"
  rm -f "$marker"
  cmd="$(build_one_cmd "$marker")"
  echo "==> launch $t" >&2
  case "$t" in
    iterm) WIN_iterm="$(bin/drivers/iterm.sh run-tabs 1 "$cmd")" ;;
    warp)  bin/drivers/warp.sh run-single "$cmd" ;;
  esac
done

for t in "${TERMS[@]}"; do
  marker="/tmp/measure-${t}-all.txt"
  echo "==> wait $t marker" >&2
  for _ in $(seq 1 "$MARKER_TIMEOUT_S"); do
    [[ -s "$marker" ]] && grep -q '==ALL_DONE==' "$marker" && break
    sleep 1
  done
done

# Close windows by id (skip warp — its driver explicitly refuses to
# quit Warp to avoid killing the user's session; the marker shell
# already exited via the trailing `exit`).
[[ -n "$WIN_iterm" ]] && bin/drivers/iterm.sh close-windows "$WIN_iterm" >/dev/null 2>&1 || true

# Parse markers → JSON on stdout.
python3 - "${SCENARIOS[@]}" <<'PY'
import json, re, os, sys
SCENARIOS = sys.argv[1:]
TERMS = ["iterm", "warp"]
ROOT = os.environ.get("PWD", os.getcwd())
out = {}
for t in TERMS:
    marker = f"/tmp/measure-{t}-all.txt"
    if not os.path.exists(marker):
        continue
    by_scn = {s: [] for s in SCENARIOS}
    current = None
    for line in open(marker, errors="replace"):
        m = re.match(r"^==SCN== (\S+)", line)
        if m:
            current = m.group(1)
            continue
        m = re.match(r"^real\s+(.+)$", line.strip())
        if m and current and current in by_scn:
            s = m.group(1)
            if "m" in s:
                a, b = s.split("m", 1)
                secs = int(a) * 60 + float(b.rstrip("s"))
            else:
                secs = float(s)
            by_scn[current].append(int(secs * 1e9))
    rec = {}
    for s, vs in by_scn.items():
        if not vs:
            continue
        vs.sort()
        med = vs[len(vs) // 2]
        size = os.path.getsize(f"{ROOT}/bench/scenarios/{s}.bin")
        bps = size * 1_000_000_000 // med
        rec[f"{s}_MBps"] = round(bps / 1048576, 1)
    if rec:
        out[t] = rec
print(json.dumps(out, indent=2))
PY

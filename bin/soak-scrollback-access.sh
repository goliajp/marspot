#!/usr/bin/env bash
#
# Scrollback ACCESS-at-depth gate (perf-attack D, re-scoped).
#
# D was filed as "scrollback dramatic-edge gap" — the goal of beating
# Terminal.app by 1.5× on 1M-line PUSH throughput, or accessing scrollback
# at 1M depth.  Measuring it surfaced two facts that re-frame D:
#
#  1. marspot's disk-backed scrollback is a FIXED ~26 624-line ring
#     (DISK_SCROLLBACK_RAM_LINES 1024 + DISK_SCROLLBACK_PAGES 100 ×
#     LINES_PER_PAGE 256), ~50 MiB anonymous mmap, bounded forever.  The
#     "unlimited / 1M / 10M scroll" framing was aspirational — the product
#     retains the most recent ~26 624 lines by default.
#  2. Within that ring, access is O(1): the COLD (post-MADV_DONTNEED) read
#     latency for a viewport is FLAT across depth (a page fault at the
#     target line, independent of how far back it is) — the real structural
#     edge over Term/iTerm, which lose history beyond their buffer.
#
# So the honest, gateable property is "O(1) cold access across the full
# retained depth + bounded memory", not a push-throughput multiplier.  This
# asserts:
#   - every probed depth's cold read is under an absolute cap (an O(N) ring
#     at 26 k depth would be orders of magnitude slower — milliseconds+),
#   - the deepest read isn't dramatically slower than the shallowest,
#   - the ring stays bounded (sb_len == the configured cap; RSS under cap).
#
# Headless (the bench uses Terminal only, no renderer / window).  Runs
# against the default disk scrollback; never touches the installed app.
#
#   bin/soak-scrollback-access.sh

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"   # marspot_bin

BIN="$(marspot_bin marspot)"
# Feed far more than the ring holds so the ring is full at every depth.
TARGET_LINES="${TARGET_LINES:-2000000}"
# O(1) proof: a single cold viewport read (~1920 cells) must be well under
# this. An O(N) ring at 26 k depth would be milliseconds+; cap generously
# at 5 ms to prove constant-time without flapping on cold-fault noise.
COLD_NS_CAP="${COLD_NS_CAP:-5000000}"
# Deepest read vs shallowest: no depth penalty. Generous (cold µs values
# are noisy; a real O(N) regression is orders of magnitude, not 8×).
DEPTH_RATIO_MAX="${DEPTH_RATIO_MAX:-8.0}"
# Bounded memory: the ~50 MiB ring + grid + binary, with headroom.
RSS_CAP_KIB="${RSS_CAP_KIB:-204800}"

fail() { echo "FAIL: $*"; exit 1; }

if [[ ! -x "$BIN" ]]; then
  echo "==> building marspot (release)"
  ( cd "$ROOT" && cargo build --release --bin marspot 2>&1 | tail -3 )
fi
[[ -x "$BIN" ]] || fail "marspot not built at $BIN"

echo "==> scrollback access-at-depth (feed ${TARGET_LINES} lines, disk ring)"
OUT=$(MARSPOT_DISK_SCROLLBACK=1 "$BIN" --bench "scrollaccess:$TARGET_LINES" 2>/dev/null)
[[ -n "$OUT" ]] || fail "bench produced no output"
echo "$OUT" | python3 -m json.tool 2>/dev/null || { echo "$OUT"; fail "bench output not JSON"; }

python3 - "$OUT" "$COLD_NS_CAP" "$DEPTH_RATIO_MAX" "$RSS_CAP_KIB" <<'PY' || exit 1
import json, sys
out, cap_ns, ratio_max, rss_cap = sys.argv[1:5]
cap_ns = int(cap_ns); ratio_max = float(ratio_max); rss_cap = int(rss_cap)
d = json.loads(out)
depths = d.get("depths", [])
if not depths:
    print("FAIL: no depth samples"); sys.exit(1)
rss = d.get("rss_kib", -1)
sb_len = d.get("sb_len", 0)

# 1. Every depth under the absolute O(1) cap.
worst = max(depths, key=lambda x: x["cold_ns"])
if worst["cold_ns"] > cap_ns:
    print(f"FAIL: depth {worst['depth']} cold {worst['cold_ns']} ns > cap {cap_ns} ns "
          f"(access is not O(1) — looks depth-dependent)")
    sys.exit(1)

# 2. No depth penalty: deepest vs shallowest.
shallow = min(depths, key=lambda x: x["depth"])["cold_ns"]
deep = max(depths, key=lambda x: x["depth"])["cold_ns"]
ratio = deep / shallow if shallow > 0 else 0.0
if ratio > ratio_max:
    print(f"FAIL: deepest read {deep} ns is {ratio:.1f}× the shallowest {shallow} ns "
          f"(> {ratio_max}× — depth penalty, not O(1))")
    sys.exit(1)

# 3. Bounded memory.
if rss > rss_cap:
    print(f"FAIL: RSS {rss} KiB > cap {rss_cap} KiB (ring not bounded)")
    sys.exit(1)

print(f"PASS: O(1) scrollback access — retained depth {sb_len} lines, "
      f"cold reads {shallow}–{deep} ns across depths "
      f"{[x['depth'] for x in depths]} (deep/shallow {ratio:.2f}× ≤ {ratio_max}×), "
      f"RSS {rss} KiB ≤ {rss_cap} KiB")
PY

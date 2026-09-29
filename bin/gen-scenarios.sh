#!/usr/bin/env bash
# Generate the bench scenario files used by `bin/measure.sh` and the marspot
# `--bench` modes.  Outputs go in `bench/scenarios/`.
#
# Re-run any time the scenario definitions change.  The files are
# deterministic given the seeds below; they are gitignored — `bin/bench`
# regenerates them on demand to keep the repo small.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/bench/scenarios"
mkdir -p "$OUT"

# ---- cat-ascii: 32 MiB of mixed ASCII text --------------------------------
# Sized so a slow terminal (iTerm2 at ~10 MB/s) finishes in a few seconds
# but a fast one (marspot target) finishes well under a second — gives us
# resolution at both ends.
echo "==> cat-ascii (32 MiB)"
python3 - "$OUT/cat-ascii.bin" 32 <<'PY'
import sys, os
out, mb = sys.argv[1], int(sys.argv[2])
target = mb * 1024 * 1024
chunk = (
    "Lorem ipsum dolor sit amet, consectetur adipiscing elit. "
    "Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.\n"
    "The quick brown fox jumps over the lazy dog. 0123456789!@#$%^&*()\n"
).encode("utf-8")
written = 0
with open(out, "wb") as f:
    while written < target:
        f.write(chunk)
        written += len(chunk)
print(f"  {out}: {os.path.getsize(out):,} bytes")
PY

# ---- cat-mixed: 64 MiB with ANSI colour codes -----------------------------
# Mimics `tail` of a colourised app log.  Each line carries an SGR
# foreground change to exercise the parser's CSI path.
echo "==> cat-mixed (16 MiB)"
python3 - "$OUT/cat-mixed.bin" 16 <<'PY'
import sys, os
out, mb = sys.argv[1], int(sys.argv[2])
target = mb * 1024 * 1024
colours = [31, 32, 33, 34, 35, 36, 37, 91, 92, 93, 94, 95, 96]
written = 0
i = 0
with open(out, "wb") as f:
    while written < target:
        c = colours[i % len(colours)]
        line = f"\x1b[{c}m[INFO]\x1b[0m request {i:08d} processed in {(i*7)%9999:4d}ms\n"
        b = line.encode("utf-8")
        f.write(b)
        written += len(b)
        i += 1
print(f"  {out}: {os.path.getsize(out):,} bytes")
PY

# ---- cat-cjk: 32 MiB of CJK text ------------------------------------------
echo "==> cat-cjk (8 MiB)"
python3 - "$OUT/cat-cjk.bin" 8 <<'PY'
import sys, os
out, mb = sys.argv[1], int(sys.argv[2])
target = mb * 1024 * 1024
chunk = (
    "你好世界 こんにちは世界 안녕하세요 세계\n"
    "床前明月光 疑是地上霜 举头望明月 低头思故乡\n"
    "東京の夜空に星が輝いている 心の中の温かい記憶\n"
).encode("utf-8")
written = 0
with open(out, "wb") as f:
    while written < target:
        f.write(chunk)
        written += len(chunk)
print(f"  {out}: {os.path.getsize(out):,} bytes")
PY

# ---- cat-emoji: 8 MiB of emoji-heavy text ---------------------------------
echo "==> cat-emoji (8 MiB)"
python3 - "$OUT/cat-emoji.bin" 8 <<'PY'
import sys, os
out, mb = sys.argv[1], int(sys.argv[2])
target = mb * 1024 * 1024
chunk = (
    "🚀 ✨ 🎉 🌏 🌙 ⭐ 🔥 💫 🌈 🦄 🐉 🎨 🎭 🎪 🎨 🎯\n"
    "deploy 🚢 done ✅ tests 🧪 passed all 100% 💯 ship 🚀 it\n"
).encode("utf-8")
written = 0
with open(out, "wb") as f:
    while written < target:
        f.write(chunk)
        written += len(chunk)
print(f"  {out}: {os.path.getsize(out):,} bytes")
PY

# ---- scroll-history: 100K-line synthetic shell history --------------------
# Lightweight, deterministic content that populates the scrollback ring
# without exercising the parser much (no escape sequences, no wide chars).
# Used by `--bench scroll`: feed populates ~100K lines of history, then the
# bench drives view_offset downward (toward live) and times each viewport
# repaint.  Sized so disk-backed scrollback's 26 624-line ring sees real
# wraparound, and memory-backed (10K-line ring) sees its full window.
echo "==> scroll-history (100K lines)"
python3 - "$OUT/scroll-history.bin" 100000 <<'PY'
import sys, os
out, n = sys.argv[1], int(sys.argv[2])
with open(out, "wb") as f:
    for i in range(1, n + 1):
        # 80-col-friendly content: "line " + 6-digit zero-padded i + filler
        # to ~70 cols → leaves room before wrap, exercises the per-cell
        # access pattern without forcing wraps mid-line.
        line = f"line {i:06d} {'.' * (60 - len(str(i)))}\n"
        f.write(line.encode("ascii"))
print(f"  {out}: {os.path.getsize(out):,} bytes ({n} lines)")
PY

echo
echo "scenarios in $OUT"
ls -lh "$OUT"

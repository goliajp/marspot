#!/usr/bin/env bash
# Generate the bench scenario files used by `bin/measure.sh` and the mars
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
# but a fast one (mars target) finishes well under a second — gives us
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
echo "==> cat-emoji (2 MiB)"
python3 - "$OUT/cat-emoji.bin" 2 <<'PY'
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

echo
echo "scenarios in $OUT"
ls -lh "$OUT"

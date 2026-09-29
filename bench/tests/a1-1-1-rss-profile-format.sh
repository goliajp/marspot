#!/usr/bin/env bash
# Phase 1.1 — A1 RSS profile dump format contract.
#
# Drives `mars --bench rss-format-dump:5`, a headless mode that
# constructs a minimal Mars (Session driving /bin/sleep 5, no
# renderer), runs the 1 Hz maybe_dump_rss loop for 5 seconds, then
# exits.  Verifies the on-disk TSV has 8 columns and at least 4 rows
# — the format contract that Phase 1.2's 30-min capture and Phase
# 1.3's analyze-rss-dump.py both depend on.
#
# Renderer-bucket columns (atlas / fontcache / mtl) read 0 in this
# mode because the bench Mars holds no MetalRenderer; that's fine —
# real numbers come from the live mars run in Phase 1.2.  Here we
# only assert the format.

set -uo pipefail

cd "$(git rev-parse --show-toplevel)"

bin=./target/release/mars
if [[ ! -x "$bin" ]]; then
    echo "FAIL: $bin missing — run 'cargo build --release' first"
    exit 2
fi

out=$(mktemp /tmp/rss-tsv-XXXX)
trap 'rm -f "$out"' EXIT

# macOS doesn't ship `timeout(1)` by default — use perl's alarm
# wrapper instead so the test never hangs if the bench mode is buggy
# enough to ignore its own deadline.  The bench mode itself exits
# after ~5.5 s; the perl alarm at 8 s is a safety net only.
MARS_PROFILE_RSS="$out" \
    perl -e 'alarm 8; exec @ARGV' "$bin" --bench rss-format-dump:5 \
        >/dev/null 2>&1 || true

if [[ ! -s "$out" ]]; then
    echo "FAIL: no rows written to $out (instrumentation absent or bench mode missing)"
    exit 1
fi

n=$(wc -l < "$out" | tr -d ' ')
cols=$(awk '{print NF}' "$out" | sort -u | head -1)

if [[ $n -ge 4 && $cols == 8 ]]; then
    echo "PASS: $n rows, $cols cols"
    exit 0
fi

echo "FAIL: $n rows, $cols cols (want >= 4 rows, exactly 8 cols)"
echo "--- first 3 rows of $out ---"
head -3 "$out" 2>/dev/null
exit 1

# mars perf attack — execution plan

Linear phased plan to close the backlog identified in [`master`](../perf-attack.md).
Each phase: one feature branch · TDD test landed first · exit criterion
in numbers · rollback path via `git`.

Pre-conditions (apply to every phase):
- Clean machine (no foreign CPU load).  Verify with `uptime` 1-min
  load < 4 before starting, otherwise pause.
- `git checkout develop` from latest.
- `cargo build --release` clean (no warnings change).

Phase ordering is **strict linear** — the predecessor's exit
criterion must be GREEN before starting the next.  Don't fork
parallel branches (they create rebase pain when fixes interact).

---

## Phase 1 — A1 surgical diagnosis (P0, 1 day)

**Goal**: identify the subsystem accounting for ~10 MiB/min RSS growth
during active 9-session soak.

### Phase 1.1 — MARS_PROFILE_RSS instrumentation (4-6 h)

Branch: `feature/perf-A1-rss-profile-instrument`

**TDD test first** — `bench/tests/a1-1-1-rss-profile-format.sh`:

```sh
#!/usr/bin/env bash
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"
out=$(mktemp /tmp/rss-tsv-XXXX)

# mcli with a 5-second cat job, RSS dumping every second
cat > /tmp/dump-test.sh <<EOF
#!/bin/sh
/bin/sleep 5
EOF
chmod +x /tmp/dump-test.sh

MARS_PROFILE_RSS="$out" MARS_SHELL=/tmp/dump-test.sh \
  timeout 6 ./target/release/mcli >/dev/null 2>&1 || true

# Expect ≥ 4 lines (1Hz × 5 sec) of 8-column TSV
n=$(wc -l < "$out" | tr -d ' ')
cols=$(awk '{print NF}' "$out" | sort -u | head -1)
[[ $n -ge 4 ]] && [[ $cols == 8 ]] \
  && echo "PASS" \
  || { echo "FAIL: $n rows, $cols cols"; exit 1; }
```

Run on current `develop` → **expect FAIL** (instrumentation absent).

**Implementation** — new files / edits:

- `src/main.rs`:
  ```rust
  // Mars struct gains:
  profile_rss_path: Option<PathBuf>,
  rss_dump_started_at: Option<Instant>,
  last_rss_dump: Option<Instant>,

  // In main(), after reading MARS_LATENCY:
  let profile_rss_path = std::env::var("MARS_PROFILE_RSS").ok().map(PathBuf::from);

  // After each render call (or once per second):
  fn maybe_dump_rss(&mut self) {
      let path = match &self.profile_rss_path { Some(p) => p, None => return };
      let now = Instant::now();
      let started = *self.rss_dump_started_at.get_or_insert(now);
      if let Some(last) = self.last_rss_dump {
          if now.duration_since(last).as_secs() < 1 { return; }
      }
      self.last_rss_dump = Some(now);
      let elapsed_s = now.duration_since(started).as_secs();
      let total_kib = read_self_rss_kib();
      let grid: usize = self.sessions.iter()
          .map(|s| s.terminal().grid().approx_bytes()).sum();
      let scrollback: usize = self.sessions.iter()
          .map(|s| s.terminal().grid().scrollback_approx_bytes()).sum();
      let atlas = self.renderer.atlas_approx_bytes();
      let fontcache = self.renderer.fontcache_approx_bytes();
      let mtl_buffers = self.renderer.metal_buffers_approx_bytes();
      let other = (total_kib * 1024).saturating_sub(grid + scrollback + atlas + fontcache + mtl_buffers);
      let line = format!("{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                         elapsed_s, total_kib * 1024, grid, scrollback,
                         atlas, fontcache, mtl_buffers, other);
      let _ = OpenOptions::new().create(true).append(true).open(path)
                .and_then(|mut f| f.write_all(line.as_bytes()));
  }

  fn read_self_rss_kib() -> usize {
      use libc::{c_int, mach_task_basic_info, mach_task_self, task_info, ...};
      // mach_task_basic_info on macOS — cheap (~10 µs)
      let mut info: mach_task_basic_info = unsafe { mem::zeroed() };
      let mut count = MACH_TASK_BASIC_INFO_COUNT;
      unsafe {
          task_info(mach_task_self(), MACH_TASK_BASIC_INFO,
                    &mut info as *mut _ as *mut _, &mut count);
      }
      (info.resident_size / 1024) as usize
  }
  ```

- `src/grid.rs`:
  ```rust
  pub fn approx_bytes(&self) -> usize {
      self.cells.capacity() * std::mem::size_of::<Cell>()
        + self.dirty_rows.capacity() * std::mem::size_of::<bool>()
  }

  pub fn scrollback_approx_bytes(&self) -> usize {
      self.scrollback.approx_bytes()  // delegates to Memory or Disk variant
  }
  ```

- `src/scrollback.rs`:
  ```rust
  // Memory: cells.capacity() * size_of::<Cell>()
  // Disk: mmap_len  (resident pages can't be cheaply queried; use full
  //                 reservation as upper bound)
  ```

- `src/glyph_atlas.rs`:
  ```rust
  pub fn approx_bytes(&self) -> usize {
      // Texture is fixed (width * height * 1 byte for R8) — known constant
      self.width as usize * self.height as usize
        + self.cache.capacity() * (size_of::<GlyphKey>() + size_of::<AtlasEntry>())
        + self.shelves.capacity() * size_of::<Shelf>()
  }
  ```

- `src/font_cache.rs`:
  ```rust
  pub fn approx_bytes(&self) -> usize {
      // 8K HashMap<(u32,u8), (usize,CGGlyph)> at high water mark
      self.char_cache.capacity() * 32  // rough HashMap accounting
        + self.fonts.capacity() * 64  // CTFont smart pointer
  }
  ```

- `src/render_metal.rs`:
  ```rust
  pub fn metal_buffers_approx_bytes(&self) -> usize {
      // Sum vertex/index/uniform buffer lengths and per-frame command
      // buffer pool.  Metal Resources don't expose `length` directly via
      // objc2 bindings; use `.length()` on each MTLBuffer ref.
      self.vertex_buf.length() as usize + self.uniform_buf.length() as usize
  }
  ```

**Verify**: re-run TDD test → **expect PASS**.

**Commit + finish**:
```sh
git flow feature finish perf-A1-rss-profile-instrument
```

---

### Phase 1.2 — Capture 30-min --extended dump (1.5 h wall-time)

Branch: none (no code change; just data capture)

```sh
# Verify clean machine
uptime  # 1-min load should be < 4

# 30-min run with instrumentation
cargo build --release
mkdir -p bench/results/a1-leak
out=bench/results/a1-leak/soak-$(date +%H%M%S).json
rss=bench/results/a1-leak/rss-$(date +%H%M%S).tsv

MARS_PROFILE_RSS="$rss" \
  ./bin/scenarios/active-9x-soak.sh mars "$out" --extended \
  > bench/results/a1-leak/soak-$(date +%H%M%S).log 2>&1 &
echo "wait 31 min, do nothing else"
```

While waiting: zero touch.  No `cargo`, no `bench.sh`, no `pgrep`.

**Verify**:
- `wc -l $rss` → ~1800 (1Hz × 30 min)
- `head $rss` → 8-col TSV starting around t=0
- `tail $rss` → t≈1800

**Exit criterion of 1.2**: `$rss` file exists with ≥ 1500 rows.

---

### Phase 1.3 — Identify leak source from dump (1 h)

Branch: `feature/perf-A1-rss-leak-analysis`

**Tool** — `bench/tools/analyze-rss-dump.py`:

```python
#!/usr/bin/env python3
"""Find monotonically growing columns in a MARS_PROFILE_RSS dump.

Reports per-subsystem slope (bytes/sec); flags any with > 80 KiB/sec
(= 5 MiB/min) as a leak candidate.
"""
import sys, statistics
COLS = ['t', 'total', 'grid', 'scrollback', 'atlas', 'fontcache', 'mtl', 'other']
rows = [list(map(int, l.split())) for l in open(sys.argv[1]) if l.strip()]
if len(rows) < 60:
    print("WARN: fewer than 60 samples; results noisy", file=sys.stderr)

# Linear regression slope per non-time column
ts = [r[0] for r in rows]
n = len(rows)
mean_t = sum(ts) / n
print(f"{'subsystem':<12} {'slope (KiB/s)':>14} {'first':>10} {'last':>10} {'flag':>6}")
for ci, name in enumerate(COLS[1:], start=1):
    ys = [r[ci] for r in rows]
    mean_y = sum(ys) / n
    num = sum((ts[i] - mean_t) * (ys[i] - mean_y) for i in range(n))
    den = sum((ts[i] - mean_t) ** 2 for i in range(n))
    slope = (num / den) / 1024  # bytes/sec → KiB/sec
    flag = "LEAK" if slope > 80 else ("watch" if slope > 8 else "")
    print(f"{name:<12} {slope:>14.2f} {ys[0]/1024:>10.0f} {ys[-1]/1024:>10.0f} {flag:>6}")
```

**Run**:
```sh
python3 bench/tools/analyze-rss-dump.py bench/results/a1-leak/rss-*.tsv
```

**Exit criterion of 1.3**: ≥ 1 subsystem flagged `LEAK`; `other` is
either flat or itself flagged (latter = heap fragmentation case).

**Commit** the analysis tool + paste the analyzer output into
`docs/perf-attack/A1-soak-rss-drift.md` progress log.

---

## Phase 2 — A1 fix (P0, 1-3 days, branches by 1.3)

Branch name varies by leak source: `feature/perf-A1-fix-<subsystem>`

### Decision tree

| 1.3 result | Phase 2 plan | Effort |
|---|---|---|
| `mtl` grows | `feature/perf-A1-mtl-autorelease` | 1 day |
| `grid` grows | `feature/perf-A1-grid-bounds` | 2 days |
| `scrollback` grows | `feature/perf-A1-ring-bound` | 1 day |
| `atlas` grows | `feature/perf-A1-atlas-bound` | 1 day |
| `fontcache` grows | `feature/perf-A1-fontcache-bound` | ½ day |
| `other` grows alone | `feature/perf-A1-allocator` (mimalloc) | ½ day |

### Sub-plan: mtl-buffers leak (most likely candidate per static analysis)

**Hypothesis**: command buffers / drawables not released; objc
autorelease pool drain timing under sustained renders.

**TDD test** — `bench/tests/a1-fix-soak-drift.sh`:
```sh
# Run 5-min soak; assert q4/q1 ≤ 1.30.  Assumes A1 instrumentation
# is in place from Phase 1.
out=$(mktemp /tmp/a1-fix.json)
./bin/scenarios/active-9x-soak.sh mars "$out" >/dev/null 2>&1
drift=$(python3 -c "import json; print(json.load(open('$out'))['metrics']['rss_drift_ratio_q4_over_q1'])")
python3 -c "exit(0 if $drift <= 1.30 else 1)" \
  && echo "PASS: drift $drift ≤ 1.30" \
  || { echo "FAIL: drift $drift > 1.30"; exit 1; }
```

Run pre-fix → expect FAIL (drift ~1.81).

**Implementation** (if mtl is leak):
- `src/render_metal.rs` — wrap each render frame in
  `objc2::rc::autoreleasepool(|| { ... })` so command buffers /
  drawables release at frame boundary instead of accumulating
  until the main runloop drains
- Verify `metal_buffers_approx_bytes()` plateaus

**Verify**: re-run a1-fix-soak-drift.sh → PASS.  Then re-run --extended:
- exit `drift ≤ 1.10×` AND `(max - first) ≤ 30 MiB`

### Sub-plan: scrollback ring growth

If `scrollback` column grows past 9 × ~100 MiB cap, the ring isn't
bounding correctly.  Fix the bound; verify capacity ≤ documented cap.

### Sub-plan: heap fragmentation (other grows)

```toml
# Cargo.toml
[dependencies]
mimalloc = "0.1"
```

```rust
// src/main.rs main() top:
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

Re-run --extended; expect `other` slope drops dramatically.

### Phase 2 exit criteria

- `bench/tests/a1-fix-soak-drift.sh`: passes 3 consecutive runs
- `bin/scenarios/active-9x-soak.sh mars … --extended`:
  drift ≤ 1.10× AND max - first ≤ 30 MiB
- `bench/baseline.json` `multi_session_thresholds.active-9x-soak`
  added with `rss_drift_max: 1.20` (10 % safety over fix's measurement)
- A1's progress log notes the root cause and mechanism

---

## Phase 3 — C bucket cleanup verification (½ day)

Branch: `feature/perf-C-verify-after-A1`

After A1 fix, re-measure C1-C4 with the new code.

**Test** — `bench/tests/c-bucket-bounds.sh`:
```sh
# Run all C scenarios; assert bounds
out=$(mktemp -d)
./bin/scenarios/idle-9x.sh mars "$out/c1.json" >/dev/null
./bin/scenarios/vim-jump.sh mars "$out/c2.json" >/dev/null
./bin/scenarios/htop-60s.sh mars "$out/c3.json" >/dev/null
./bin/scenarios/active-9x-soak.sh mars "$out/c4.json" >/dev/null

# C1: idle-9x first sample ≤ 30 MiB
# C2: vim-jump post ≤ 25 MiB
# C3: htop-60s mean ≤ 30 MiB
# C4: active-soak max ≤ 100 MiB
python3 - <<PY
import json
ok = True
for k, target in [('c1', 30), ('c2', 25), ('c3', 30), ('c4', 100)]:
    j = json.load(open('$out/{}.json'.format(k)))
    # extract per-scenario MiB
    ...
print("PASS" if ok else "FAIL")
PY
```

**Possible outcomes**:
1. All pass → C is automatically resolved by A1 fix.  Mark all C items `done`.
2. Some fail → identify which subsystem still bloats.  Fix individually
   (probably 1-2 specific items, e.g. atlas footprint per cell).

**Exit criterion**: all 4 C bounds enforced via above test, and
encoded as separate gate fields in `bench/baseline.json`.

---

## Phase 4 — B3 + B4 CJK / emoji raster optimisation (1 day)

Branch: `feature/perf-B-raster-pool`

### Phase 4.1 — TDD tests

`bench/tests/b3-cjk-throughput.sh`:
```sh
# Asserts mars cat-cjk live ≥ 42 MB/s (= 1.0× Term)
mb=$(./bin/measure.sh cat-cjk mars 2>&1 | grep -oE 'cat-cjk.*[0-9.]+ MB/s' | grep -oE '[0-9.]+ MB/s' | head -1 | awk '{print $1}')
python3 -c "exit(0 if float('$mb') >= 42 else 1)" \
  && echo "PASS $mb ≥ 42 MB/s" \
  || { echo "FAIL $mb < 42"; exit 1; }
```

Pre-fix: FAIL (current 36.4).

### Phase 4.2 — Implementation

`src/glyph_atlas.rs` additions:

```rust
/// Pre-allocated buffers for repeated glyph rasterisation.  A single
/// `RasterPool` is owned by `GlyphAtlas` and reused across every
/// rasterise_glyph call; eliminates per-glyph CGBitmapContext create
/// + property setting + Vec::new() allocation cost (perf-attack B).
struct RasterPool {
    ctx_1cell: CGContext,
    ctx_2cell: CGContext,
    bytes_1cell: Vec<u8>,
    bytes_2cell: Vec<u8>,
    metrics: SlotMetrics,
}

impl RasterPool {
    fn new(metrics: SlotMetrics) -> Option<Self> {
        let ctx_1cell = make_alpha_only_ctx(metrics.cell_w, metrics.cell_h)?;
        configure_ctx(&ctx_1cell);
        let ctx_2cell = make_alpha_only_ctx(metrics.cell_w * 2, metrics.cell_h)?;
        configure_ctx(&ctx_2cell);
        let len_1c = (metrics.cell_w as usize) * (metrics.cell_h as usize);
        let len_2c = len_1c * 2;
        Some(Self {
            ctx_1cell, ctx_2cell,
            bytes_1cell: vec![0u8; len_1c],
            bytes_2cell: vec![0u8; len_2c],
            metrics,
        })
    }

    fn rasterise(&mut self, font: &CTFont, glyph: CGGlyph, n_cells: u16, bbox_origin_x: f64) -> &[u8] {
        let (ctx, bytes) = if n_cells == 1 {
            (&self.ctx_1cell, &mut self.bytes_1cell)
        } else {
            (&self.ctx_2cell, &mut self.bytes_2cell)
        };
        bytes.fill(0);
        let baseline_canvas_y = self.metrics.cell_h as f64 - self.metrics.baseline_from_top as f64;
        let origin = CGPoint::new(-bbox_origin_x, baseline_canvas_y);
        font.draw_glyphs(&[glyph], &[origin], ctx);
        bytes
    }
}
```

`rasterise_glyph` rewritten to use `RasterPool::rasterise`:

```rust
fn rasterise_glyph(
    pool: &mut RasterPool,
    font: &CTFont,
    glyph: CGGlyph,
    metrics: SlotMetrics,
) -> Option<Raster> {
    let bbox = font.get_bounding_rects_for_glyphs(...);
    if bbox.size.width <= 0.0 || bbox.size.height <= 0.0 { return None; }
    let n_cells: u16 = if bbox.size.width > metrics.cell_w as f64 * 1.5 { 2 } else { 1 };
    let bytes = pool.rasterise(font, glyph, n_cells, bbox.origin.x);
    Some(Raster {
        bytes: bytes.to_vec(),  // copy out for atlas upload; see note
        px_w: metrics.cell_w * n_cells as u32,
        px_h: metrics.cell_h,
        n_cells,
    })
}
```

Note: `bytes.to_vec()` copies once.  If profiling shows this copy
dominates, the upload path can be changed to take `&[u8]` directly,
saving the allocation entirely.

### Phase 4.3 — Verify

```sh
bench/tests/b3-cjk-throughput.sh    # PASS ≥ 42 MB/s
bench/tests/b4-emoji-throughput.sh  # PASS ≥ 50 MB/s
bench/tests/e2-warmup.sh            # regression check
./bin/bench.sh                      # GATE PASSED
```

**Phase 4 exit criterion**: B3 + B4 tests pass on 3 consecutive runs.
`bench/baseline.json` cat-cjk + cat-emoji `mars_live_MBps_min` updated
to reflect new floor.

---

## Phase 5 — D rescope to scrollback access latency (1 day)

Branch: `feature/perf-D-access-latency-scenario`

### Phase 5.1 — New scenario

`bench/scenarios/scroll-access-1m.spec.md` — describes the workload.

`bin/scenarios/scroll-access-1m.sh`:
```sh
# For mars: push 1M lines, then issue scroll-back-to-line-N for
# N in {1, 100K, 500K, 999_999}; measure access latency per N.
# For Term/iTerm2/Warp: same workload, expect skip if history
# capped below N.
```

`src/main.rs` `--bench` mode gains `scroll-access-1m`.

### Phase 5.2 — Drivers + gate

Each terminal driver runs the scenario; mars-only emits per-N
latency.  Term/iTerm2 fail with `unsupported (history capped at N)`.

`bench/baseline.json` adds:
```json
"scroll_access_1m_us_max": {
  "_comment": "p99 access latency for mars at 1M-line depth.  Structural advantage gate: this query is meaningless for terminals with cap'd history.",
  "view_offset_999_999_us_max": 100
}
```

**Exit criterion**: mars accesses depth-1M scrollback in ≤ 100 µs;
gate fails if latency exceeds.  Term/iTerm2 explicitly skipped (not
"failed").

---

## Phase 6 — E3 vim-jump cross-term wall-time (½ day)

Branch: `feature/perf-E3-vim-jump-cross-term`

Each driver (`bin/drivers/{iterm,terminal,warp}.sh`) gains a path
that wraps the vim command with `time -p`, writes wall-time to
marker.  `bin/scenarios/vim-jump.sh` parses wall-time from marker
for every terminal.

Test: `bench/tests/e3-vim-jump-cross-term.sh` — synthetic markers
for each terminal verify wall-time field is populated.

**Exit criterion**: `bench-run.sh` snapshot's `vim-jump.{iterm,terminal,warp}.metrics.wall_time_s` is non-null.

---

## Total timeline (focused work)

| Phase | Effort | Dependency |
|---|---|---|
| 1.1 instrument | 4-6 h | clean machine |
| 1.2 capture | 30 m + 30 m wall | 1.1 done |
| 1.3 analyse | 1 h | 1.2 dump in hand |
| 2 fix | 1-3 days | 1.3 root cause known |
| 3 C verify | ½ day | 2 done |
| 4 B3+B4 raster pool | 1 day | independent of 2 (could parallel, but linear keeps clean) |
| 5 D rescope | 1 day | independent |
| 6 E3 driver gap | ½ day | independent |

**Critical path: A1 (Phase 1+2)** = 2-4 days.  Everything else can
follow once A1 is closed and architectural commitment is restored.

---

## Rollback strategy

Every phase: feature branch, FF-merge into develop on success only.
On failure:
- Phase 1: discard branch (`git branch -D feature/perf-A1-rss-profile-instrument`)
- Phase 2: same; the instrumentation stays in place from Phase 1
- Phase 3-6: same per-phase

baseline.json is updated only via `--update-baseline` post-fix; if a
fix lands but the F-floor needs adjustment (rare), do a separate
recalibration commit explaining the bump.

`develop` always reflects last-known-green state.  `git revert <sha>`
on develop pulls a phase out cleanly if needed.

---

## What "ready to ship" looks like

- `bin/bench.sh --full` passes 13/13 + multi-session 4/4 + soak 4/4
- `bin/scenarios/active-9x-soak.sh mars … --extended` drift ≤ 1.10×
- `bench/tests/b3-cjk-throughput.sh` PASS
- `bench/tests/b4-emoji-throughput.sh` PASS
- New "scroll-access-1m" scenario gated mars-only
- All 7 E items either done or retracted
- All A/B/C/D bucket items either done or rescoped explicitly

That state is shippable as the perf-foundation merge.  Subsequent
work focuses on user-visible features (UI, tmux integration, etc.)
on top of a stable perf platform.

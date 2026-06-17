# Pane upgrade — implementation plan (audited)

A living design.  Replaces the in-process anonymous-mmap scrollback
(26 k-line cap, lost on execv) with a **per-session persistent
file**, layers a **full-history search engine** on top of it, and
introduces a **pane toolbar framework** that the search UI is the
first inhabitant of.  basic-level feature — lives in `marspot-term`,
`marspot-session`, `marspot-core`.  No cc plugin involvement.

Audit pass v2 (2026-06-18) — every open question resolved to a
decision; every step has a Definition-of-Done; the full commit
sequence and rollback strategy are documented; no mid-execution
choices remain.

Tracking memory: [[project-scrollback-persistent]] in
`~/.claude-profile-3`.

---

## §0  Product principle (overrides everything)

**性能必须极佳;功能不要都可以.**  This is marspot's product
tone, captured in memory `feedback-perf-over-feature`.  Apply to
every decision in this doc as follows:

- **Every perf budget in this doc is a hard ceiling**, not an
  aspiration.  A step that cannot meet its budget is **dropped**,
  not "perf-relaxed for shipping convenience".
- When perf and feature collide, the feature is what gives way.
  Concrete examples in this rollout:
  - C4 highlight overlay: budget +50 µs to render p99.  If
    measured at +200 µs even after optimisation, **drop the
    visual highlight entirely** (search still navigates to the
    line; just no yellow bar).
  - B4 live-grid merge: budget zero added latency on the typing
    path.  If we can't keep it off the hot path, **drop the
    live-grid hits**.
  - Search worker thread: must NOT cause priority-inversion with
    the PTY pump.  If the worker steals from PTY scheduling, the
    feature is dropped and search becomes async-poll-based.
- Hot paths (`Terminal::feed` per byte, `Grid::push_line` per
  line, render per frame) are sacred.  Storage / search /
  highlight code added in this rollout **never touches them
  with synchronisation primitives** beyond a single atomic load.
- The FLIP commit (§8 #14) ships ONLY if every perf gate in §12
  passes; partial-feature with full perf is acceptable, full-
  feature with even one perf regression is not.

This principle is also why §13 (out-of-scope) is unapologetic.

---

## §0.5  Operating principles for this rollout

These constrain everything below.  Re-read before authoring any
commit.

**P1. No mid-execution decisions.** Every choice that affects code
is resolved in this document before A1 starts.  If a buried
ambiguity surfaces during implementation, **stop and revise the
document, then continue** — never paper over with an ad-hoc choice.

**P2. No mid-rollout install.** `bin/install-local.sh` is NOT run
between A1 and the final FLIP commit.  Develop accumulates
commits; the running app stays on the pre-A1 binary the entire
time.  Mini bench (`bin/bench-remote.sh`) still runs after every
commit to catch perf regressions in code that is staged but not
loaded.

**P3. Atomic flip.** The FLIP commit is the single moment behaviour
changes for the user.  Before it, the feature is unreachable; after
it, it is the default.  No env-flag double-life that lingers.

**P4. Full revert preserves baseline.** At any point during the
rollout, `git revert <A1..HEAD>` restores the user-facing baseline.
Every intermediate commit must keep this invariant: nothing
permanently mutates user state (state.bin, persistent files, etc.)
unless `MARSPOT_FILE_SCROLLBACK=1` is explicitly set during
development testing.

**P5. Each intermediate commit is buildable, tested, bench-clean.**
`cargo build` + `cargo nextest run --lib` + `bin/bench-remote.sh`
all green on every commit.  Never an "in flight" partial.

**P6. Each step has a written Definition of Done.**  Listed in
§12.  A step is not "done" until every DoD bullet is verified.

**P7. Perf gate failure = drop, not relax.**  Per §0 product
principle.  If any step's perf budget can't be met after a
reasonable optimisation pass (≤ 2 days of work), the step's
feature is removed from this rollout and §13 is updated to
mark it out-of-scope.  We do NOT ship a slow feature.

---

## §1  Decisions (was: open questions)

The previous draft had Q1..Q9 punted.  All resolved here.

| # | decision | rationale |
|---|---|---|
| D1 | File size cap = **none** in v1 | user wants unbounded depth; disk space is the user's call to manage |
| D2 | Sidecar text mirror = **no** | linear scan of `.bin` at 5 GB/s memcpy is < 50 ms for 100 MB.  Add later only if profiling forces it |
| D3 | line_idx stability = **forever** | no eviction in v1 → no shift; line_idx is permanent |
| D4 | L2 result cache = **no** | regenerate on re-open; staleness bugs are worse than re-scan cost |
| D5 | Per-line timestamps = **no** | reserved 0-bit in file header for D-phase |
| D6 | Cross-pane search = **no, not even Phase D** | user clarified: future is L2-level archive search, not cross-pane |
| D7 | Search bar placement = **floating OVERLAY** at top-right of pane | Chrome / VS Code convention |
| D8 | fsync = **never** in v1 | flush BufWriter on graceful exit only; crash tolerates losing last ~30 s |
| D9 | Fuzzy / regex = **dropped from v1** | "fuzzy 可能太复杂,先解决换行加空格就非常好" — literal substring only |
| D10 | cc plugin reuse = **out of scope** | wire layer is non-cc-specific so it stays unblocked for future |
| D11 | Search-text normalisation = **applied by default, no opt-in** | DECAWM wrap merge + cc-style hard-wrap-with-hanging-indent merge.  Same heuristic as `grid_links::is_cc_hard_wrap_continuation` |
| D12 | Eviction = **none in v1** | disk grows unbounded; log at 1 GiB watermark (warn only); add eviction later if user requests |
| D13 | Wire format for new MsgTypes = **hand-rolled** matching existing `shell_proto::encode_*` style — NO new bincode dependency |
| D14 | New MsgType IDs = **50, 51, 52, 53** (next available in the 30-49 window-state block extended by 1, sticking with the L2↔L3 cluster) |
| D15 | Concurrent queries on L3 = **last-write-wins** — receiving a new SearchScrollback cancels the in-flight one |
| D16 | Worker thread for search = **yes** — L3 main thread must keep pumping PTY during long scans |
| D17 | Toolbar tools framework = **PaneTool trait, SlotZone enum, all UI integration in `src/render_metal.rs` + `src/bin/marspot-core.rs`** (no new bin / no new crate) |
| D18 | Render approach for search bar / list / highlight = **all glyph instances on the same Metal pass** as the title strip — no new pipeline state |
| D19 | Cmd+F = **macOS Cmd** (not Ctrl).  Documented separately because user typed "Ctrl+F" |
| D20 | Search state across L3 self-execv = **dropped** (L2 closes search bar; new L3 starts fresh).  Rationale: state survival across execv is high-cost, low-value for a transient UI |

---

## §2  Architecture contracts (pre-existing things we honour)

These exist and we MUST NOT break them.

**C1. `Scrollback` enum dispatched via `match`** — perf-critical
(see crate docs).  Adding `File` variant means extending the enum
with another arm; not switching to trait-object dispatch.

**C2. `Cell` encoding = `CELL_BYTES = 13` (4 ch + 9 attrs)** — set
in `crates/marspot-term/src/terminal.rs:884`.  All new file formats
use this exact same byte layout for cells.  If `CELL_BYTES`
changes, the file header's `cell_abi` field gates compat.

**C3. `MsgType` repr(u32) and silently-skip rule** — unknown
msg_type at decode time returns `None` and the receiver drops the
frame.  No errors; no breakage.

**C4. wire frames use `Frame::write_to(stream)` / `Frame::read_from`** —
`{ msg_type: u32, payload_len: u32, payload: Vec<u8> }`.  We add
new MsgType values, encode/decode helpers per the
`shell_proto::encode_pane_badge` style.

**C5. `Terminal::new(cols, rows)` initialises the disk-backed
scrollback by default** — no env required.  We extend this to
optionally use `File` (controlled by a per-Terminal opt-in
parameter, not env at this layer — env wires up only at the L3
binary main).

**C6. RFC-003 §6 Amendment 16 self-execv** — L3 hands off PTY +
listener + L2 control fds via the handoff manifest.  Persistent
scrollback file is path-based reopen, NOT fd-handoff — keeps the
manifest unchanged.

**C7. `paths::sessions_dir() / session_dir(id)`** — the canonical
per-session storage root.  Add `scrollback_bin_path(id)` and
`scrollback_idx_path(id)` next to existing `state_bin_path(id)`.

**C8. `pane_badges` (HashMap on L2 CoreApp)** — used today for cc
detection.  Search heuristic ("treat cc-hard-wrap as soft wrap")
applies *unconditionally* per D11; no pane_badges check needed.

**C9. Mini bench gate** — every commit must pass `bin/bench-remote.sh`
11/11.  Specifically `parse cat-*`, `render p99`, `scroll p99`,
`scroll-cold p99`, `rss`, `size` — the storage rewrite must not
regress any of these.

---

## §3  Storage layer — `scrollback.bin` + `scrollback.idx`

### §3.1  Files

Per L3 session, in `paths::session_dir(id)`:

- `scrollback.bin` — append-only data file, line records
- `scrollback.idx` — append-only sidecar, `[u64 LE byte_offset]` array

`paths.rs` additions (new pub fns):

```rust
pub fn scrollback_bin_path(id: u64) -> PathBuf {
    session_dir(id).join("scrollback.bin")
}
pub fn scrollback_idx_path(id: u64) -> PathBuf {
    session_dir(id).join("scrollback.idx")
}
```

### §3.2  `scrollback.bin` byte layout

Header (32 bytes, fixed):

```text
offset  size  field          value
0       4     magic          0x5350_5301  ('S','S','P',0x01) LE
4       4     version        u32 LE, current = 1, min_compat = 1
8       4     cell_abi       u32 LE = CELL_BYTES (= 13 in this build)
12      4     header_flags   u32 LE, all zero in v1, reserved
16      8     created_ns     u64 LE = SystemTime::UNIX_EPOCH nanos at create
24      8     reserved       0
```

After header, one or more line records appended in arrival order:

```text
offset  size       field
+0      4          rec_len u32 LE  = 1 + 2 + cols * CELL_BYTES
+4      1          wrapped u8      (0 = hard newline, 1 = DECAWM continuation of previous line)
+5      2          cols    u16 LE  (cell count in this line)
+7      cols*13    cells           (each = 4 ch + 9 attrs, same as serialize_snapshot)
```

`rec_len` is the byte count **after the rec_len field itself**.
Reader validates `pos + 4 + rec_len <= file_len` before parsing
cells.  A truncated trailing record (crash mid-write) is
discardable by this check.

### §3.3  `scrollback.idx` byte layout

```text
[ u64 LE entry_0 ]
[ u64 LE entry_1 ]
...
[ u64 LE entry_N ]    ← sentinel: byte_offset of the EOF
```

Where `entry_k` = byte offset in `.bin` of the start of the
`rec_len` field for line k.  Invariants:

- `entry_0 = 32` (header bytes count)
- `entry_k+1 = entry_k + 4 (rec_len) + 1 (wrapped) + 2 (cols) + cols * CELL_BYTES`
- `len(idx) = N + 1` where N = lines in `.bin`
- The sentinel lets `len(idx) - 1` work as the line-count getter

### §3.4  Lifecycle

| event | code path | behaviour |
|---|---|---|
| `Scrollback::file(id, cols)` constructor | A1 | open `.bin`+`.idx` r/w append; create if missing with header.  Validate magic / version / cell_abi if existing.  Mmap idx.  Load last 1024 line offsets and read those cells into the RAM ring. |
| `push_line(&mut self, line: &[Cell], wrapped: bool)` | A1 | append rec to `.bin` BufWriter (64 KiB); append `bin_tail_offset` to `.idx` BufWriter (4 KiB); push into RAM ring; increment line count; remap idx if mmap window crossed |
| `cell_at(idx, col)` | A2 | tier 1 RAM ring → tier 2 mmap window (last 1 MiB of `.bin`) → tier 3 `pread()` |
| `line_to_vec(idx)` | A2 | same tier sequence, wraps cell_at across cols |
| `scrollback_wrapped(idx)` | A2 | tier-1 RAM ring stores its own flag; tier-2/3 reads byte at `entry[idx] + 4` |
| graceful drop | A1 | flush BufWriters (writes to OS page cache; no fsync) |
| crash | n/a | next open trims trailing record if `entry_last + 4 + rec_len > file_len` |
| cold start (file exists) | A3 | open; if header bad: log + rename to `.corrupt.<ns>` and start fresh; if good: rebuild idx by linear scan IFF `len(idx)` doesn't match expected |
| self-execv | A3 | new L3 image opens by path; same file; no fd handoff |
| Terminal::new(cols, rows) | A3 | branches: env `MARSPOT_FILE_SCROLLBACK=1` → File variant; else → existing Disk variant (default until FLIP) |
| FLIP commit | F1 | switch default to File; mark Disk variant `#[deprecated]`; final commit removes Disk in F2 |

### §3.5  RAM ring + mmap window sizes

| tier | size | rationale |
|---|---|---|
| RAM ring (most-recent lines, owned Vec<Cell> + wrapped flag) | **1024 lines** | matches existing `DISK_SCROLLBACK_RAM_LINES` budget; ≥ 99 % hit rate for non-search workloads |
| mmap window of `.bin` tail | **1 MiB**, page-aligned, slide forward on overflow | covers ~10 k recent lines at avg 100 B/line |
| mmap of full `.idx` | always entire file | u64 per line × N lines × small N: even 1 M lines = 8 MiB which fits comfortably; mmap'd kernel-paged read-only |
| LRU page cache for cold reads | **not implemented in v1** | kernel page cache + mmap window covers it; defer until profile demands |

### §3.6  Hot-path cost budget

Steady-state, per `push_line`, on M-series.  Step-1 micro-bench
gates these on mini.

| step | budget | actual measured at |
|---|---|---|
| serialise 80 cells to scratch buf | < 300 ns | A1 step |
| RAM-ring tail copy | < 100 ns | A1 step |
| BufWriter `.bin` write | < 50 ns (RAM only) | A1 step |
| BufWriter `.idx` write | < 30 ns (RAM only) | A1 step |
| **total per push_line** | **< 500 ns** | A1 step |

Compare to parser cost of feeding 80 chars = 5-20 µs.  Storage
adds < 3 % to a typical parse round.

### §3.7  Migration

The FLIP commit (F1) switches Terminal::new's default to File.  At
that moment:

1. Running L3 (still on Disk variant) is unaffected — the FLIP
   commit doesn't install-local automatically; user must
   `bin/install-local.sh` to load the new binary.
2. On the install, L3 self-execvs.  Resume code (`try_resume_handoff`)
   reads the v2 snapshot which carries up to 20 k scrollback lines.
3. New L3 image opens scrollback.bin (fresh, just created), THEN
   apply_snapshot replays the snapshot scrollback into it via
   push_to_file as part of `push_historic_scrollback_line`.  This
   one-time write seeds the file.
4. After this install, push_line goes straight into the file; the
   in-memory ring carries the most-recent 1024 for read speed.

### §3.8  v2 snapshot scrollback section disposition

Once F1 lands:

- `SNAPSHOT_SCROLLBACK_LINE_CAP` stays at 20 k as backstop.
- Reader (`apply_snapshot`) consumes the v2 scrollback section
  ONCE per resume — feeds it into File scrollback via
  `push_historic_scrollback_line`.
- Subsequent execvs need not include the section (file already
  has it).  We leave the serialiser unchanged: it still writes the
  20 k lines.  Cost: ~26 MB state.bin × 8 panes ≈ 200 MB write per
  install — acceptable; tradeoff is "no risk if file misses the
  resume read for any reason".

F-phase decides whether to demote the section to 256 lines (true
backstop).  For now it stays at 20 k.

### §3.9  Failure modes (storage)

| condition | behaviour |
|---|---|
| `.bin` magic mismatch | rename `scrollback.bin` → `scrollback.bin.corrupt.<ns>`; same for `.idx`; create fresh; log error |
| `.bin` ok, `.idx` missing | rebuild `.idx` by linear-scanning `.bin`; one-time cost; log info |
| `.bin` ok, `.idx` length wrong | rebuild `.idx`; log warn |
| trailing partial rec in `.bin` (crash) | truncate `.bin` to last valid record end; rebuild `.idx`; log info |
| disk full on append | best-effort: drop oldest line in RAM ring; do not panic; log error; user sees scrollback freeze at oldest content + UI status |
| permission denied opening session dir | log error; fall back to Disk variant for this session; user sees "scrollback may not persist" status |
| concurrent access from a sibling L3 (impossible by design but defensive) | `flock` advisory on the `.bin` at open; second opener gets EAGAIN; log error and fall back to Disk |

---

## §4  Search engine

### §4.1  Logical-line + normalisation pipeline

A **logical line** is the unit the engine searches.  Built by an
iterator in `marspot-term::scrollback_search`:

```rust
pub struct LogicalLineIter<'a> { /* private */ }

pub struct LogicalLine {
    pub start_phys_idx: u64,   // first physical line in .bin (inclusive)
    pub end_phys_idx:   u64,   // last physical line (inclusive)
    pub raw_text:       String,// chars(per cell) concatenated, '\n' joins
    pub norm_text:      String,// normalised: hanging-indent stripped + soft-wrap-joined
    /// For each char position in norm_text, where in (phys_line, col) is it?
    pub norm_to_phys: Vec<(u64, u16)>,
}
```

Logical-line iterator rule (newest-first):

1. Start at idx[N-1] (last appended).
2. Walk backwards while `wrapped == true` on the current row →
   group continuation rows together.
3. Apply cc-hard-wrap heuristic AFTER soft-wrap grouping
   (D11): if the would-be-next-older line ends with a URL/path-
   class character at flush-right AND the current line begins with
   1..=4 leading spaces and a URL/path-class char, merge anyway
   (same predicate as `grid_links::is_cc_hard_wrap_continuation`).
   The merged line's `raw_text` keeps the `\n + leading spaces`;
   `norm_text` strips them.

`raw_text` and `norm_text` are both stored so the search can
report char positions that map BACK to physical row + col via
`norm_to_phys` (entries built during normalisation).

Why both raw and norm: literal substring queries match against
norm_text (user typed "https://example.com/path/to/file" without
the `\n  `); highlighting uses the phys mapping to draw bars on
the actual physical row positions.

### §4.2  Substring algorithm

```rust
pub struct SearchOpts {
    pub case_sensitive: bool,
    pub max_total: u32,  // first batch cap; SearchMore extends
}

pub fn search_scrollback(
    bin_path: &Path,
    idx_path: &Path,
    query: &str,
    opts: SearchOpts,
) -> Box<dyn Iterator<Item = SearchHit>>
```

Iterator semantics:

- Newest-first (high logical_line_idx → low).
- Yields one `SearchHit` per match instance — multiple hits per
  line are emitted as separate items, in column order within the
  line.
- Lazy: stops iterating when consumer drops the iterator (used
  for cancellation).

Case folding for D11 case-insensitive:

- Lowercase both query and norm_text once at iterator init.
- ASCII fast path: simple `b'A'..=b'Z' XOR 0x20` per byte.
- Non-ASCII: Unicode lowercase via `char::to_lowercase()` — slower
  but tolerable since case-insensitive is opt-in.

Snippet (preview text shown in result list):

- `[..mark..]` = the matched run
- Snippet length = up to 80 chars: match + `min(40, before)` chars
  before and `min(40 - before_taken, after_avail)` chars after,
  clamped to the logical line boundary
- Snippet uses `norm_text` not raw

### §4.3  `SearchHit` shape

```rust
pub struct SearchHit {
    pub logical_line_idx: u64,
    pub char_offset:      u32,   // chars into norm_text
    pub char_len:         u32,
    pub snippet:          String,// ≤ 80 chars, UTF-8, no markup
    pub snippet_match_start: u16,// char offset of match within snippet
    pub snippet_match_end:   u16,
    pub physical_rows: Vec<PhysicalSpan>,
}

pub struct PhysicalSpan {
    pub phys_row_idx: u64,     // line idx in .bin (= scrollback line idx)
    pub col_start:    u16,
    pub col_end_inclusive: u16,
}
```

`physical_rows` has 1 entry for a single-row match, 2 + entries for
a match crossing wrap boundaries.  L2 uses it for highlight render.

### §4.4  Cancellation + worker thread (D16)

L3 main thread spawns a search worker:

```rust
struct SearchWorker {
    handle: JoinHandle<()>,
    cancel: Arc<AtomicBool>,
    query_id: u32,
}
```

Worker iterates the engine; checks `cancel` between each emitted
hit.  Reports batches via mpsc back into main loop's event channel
(or directly writes a `SearchResults` frame on the UDS — see §5).

Last-write-wins (D15): receiving `SearchScrollback` while a worker
is running:

1. Set the old worker's `cancel` to true.
2. Spawn a new worker.
3. Old worker exits at next `cancel.load(Acquire)` check.

### §4.5  Live-grid merge (B4)

Visible grid content not yet pushed to scrollback is also searched.
Implementation:

- Before iterating scrollback file, the engine reads the live grid
  (`session.terminal().grid()` cells), builds logical lines, and
  emits hits at synthetic line indices `u64::MAX - row_offset`.
- L2 recognises hits with `logical_line_idx >= u64::MAX - rows`
  as live-grid hits — view_offset for these is `rows - 1 - (u64::MAX - hit.idx)`.

### §4.6  Performance budget

| scrollback size | first 64 hits target | re-measured at |
|---|---|---|
| 1 MB (~10 k lines) | < 20 ms | B1 step |
| 10 MB (~100 k lines) | < 100 ms | B3 step |
| 100 MB (~1 M lines) | < 600 ms | post-D-phase if user reports breach |

If the 100 MB target is breached after B3, decision point:
implement D2 (sidecar text mirror).

---

## §5  Wire protocol

### §5.1  New MsgType values (D14)

| name | id | direction |
|---|---|---|
| `SearchScrollback` | 50 | L2 → L3 |
| `SearchResults`    | 51 | L3 → L2 |
| `SearchMore`       | 52 | L2 → L3 |
| `SearchCancel`     | 53 | L2 → L3 |

Added to `MsgType` enum with `try_from_u32` arms.

### §5.2  `SearchScrollback` payload (L2 → L3)

Hand-rolled encoding per D13:

```text
[ query_id        u32 LE ]
[ case_sensitive  u8     ]
[ max_total       u32 LE ]
[ query_byte_len  u32 LE ]
[ query           utf8 bytes ]
```

Encoder/decoder:

```rust
pub fn encode_search_scrollback(query_id: u32, case_sensitive: bool, max_total: u32, query: &str) -> Vec<u8>
pub fn decode_search_scrollback(buf: &[u8]) -> Result<(u32, bool, u32, String), DecodeErr>
```

### §5.3  `SearchResults` payload (L3 → L2)

```text
[ query_id     u32 LE ]
[ has_more     u8     ]
[ total_seen   u32 LE ]
[ hit_count    u32 LE ]
[ for each hit:
    [ logical_line_idx        u64 LE ]
    [ char_offset             u32 LE ]
    [ char_len                u32 LE ]
    [ snippet_match_start     u16 LE ]
    [ snippet_match_end       u16 LE ]
    [ snippet_byte_len        u32 LE ]
    [ snippet                 utf8 bytes ]
    [ phys_span_count         u16 LE ]
    [ for each span:
        [ phys_row_idx       u64 LE ]
        [ col_start          u16 LE ]
        [ col_end_inclusive  u16 LE ]
    ]
]
```

### §5.4  `SearchMore` payload (L2 → L3)

```text
[ query_id     u32 LE ]
[ count        u32 LE ]
[ direction    u8     ]   // 0 = older (more results going back in time), 1 = newer
```

In v1, direction is always 0 (newer hits are only the live grid,
already returned in the initial response).  Direction=1 reserved
for D-phase.

### §5.5  `SearchCancel` payload (L2 → L3)

```text
[ query_id     u32 LE ]
```

### §5.6  query_id allocation

L2 holds `next_query_id: u32` per CoreApp.  Each new
SearchScrollback uses `next_query_id += 1`.  Wrap at u32::MAX is
acceptable (~4 B queries before reuse).

### §5.7  Forward-compat behaviour

`MsgType::try_from_u32` returns `None` for unknown ids; receivers
log at TRACE and drop the frame.  This means:

- Old L3 receiving new SearchScrollback → silent skip.  L2 should
  detect "no SearchResults within timeout" and fall back to "search
  unavailable" UI state.  We add the timeout in C3 step.
- Old L2 receiving SearchResults → silent skip.  No issue: L2
  never asks for results unless it understands them.

---

## §6  UI — PaneTool framework + search bar / list / highlight

### §6.1  `PaneTool` trait + `ToolSlot` enum (D17)

In `crates/marspot-term/src/render.rs` (new module added in C1):

```rust
pub enum ToolSlot {
    TopFixed,     // claims N rows below title strip; shrinks grid
    BottomFixed,  // claims N rows at pane bottom; shrinks grid
    Overlay,      // free-positioned, does not affect grid layout
}

pub enum InputDisposition {
    Handled,            // tool consumed the event; do not propagate
    Pass,               // tool did not handle; propagate to next handler
    HandledRequestRedraw,
}

pub trait PaneTool: Send {
    fn slot(&self) -> ToolSlot;
    /// Rows tall (for fixed slots).  Ignored for Overlay.
    fn fixed_height_rows(&self) -> u16 { 0 }
    /// Hit-test (overlay only): is (x_phys, y_phys) inside this tool?
    fn hit_test(&self, _x: f64, _y: f64) -> bool { false }
    /// Key event while tool has focus.  Default: Pass.
    fn on_key(&mut self, _ev: &MarspotKeyEvent, _mods: Modifiers) -> InputDisposition { InputDisposition::Pass }
    /// Mouse event.  Default: Pass.
    fn on_mouse(&mut self, _kind: MouseEventKind, _x: f64, _y: f64) -> InputDisposition { InputDisposition::Pass }
}
```

**Per-pane state**: `Pane` gains a `tools: Vec<Box<dyn PaneTool>>`
slot; default empty.  Search adds itself when user presses Cmd+F.

### §6.2  Layout integration

In `src/render_metal.rs::build_instances`, the per-pane block
currently computes:

```
inner_y_top = rect.y_top + title_h + padding
grid_inner_h = rect.h - title_h - 2*padding
```

After C1:

```
top_fixed_h = sum(tool.fixed_height_rows * cell_h for tools in TopFixed slot)
bot_fixed_h = sum(tool.fixed_height_rows * cell_h for tools in BottomFixed slot)
inner_y_top = rect.y_top + title_h + padding + top_fixed_h
grid_inner_h = rect.h - title_h - 2*padding - top_fixed_h - bot_fixed_h
```

Render order per pane:

1. Title strip (existing path)
2. Top-fixed tools (each render their cells/glyphs into their stripe)
3. Grid cells + glyphs (existing path, on smaller inner rect)
4. Bottom-fixed tools
5. Overlay tools (after grid, so they sit on top visually)
6. Highlight overlay (drawn last to win z-order)

### §6.3  Input routing

In `src/bin/marspot-core.rs::key()`, before the existing dispatch:

```rust
// Tool-layer interception: any tool in slot() != Overlay can claim
// the key.  Overlay tools claim only when "focused" (the user has
// the search bar active).
for tool in self.panes[self.focused_idx].tools_mut() {
    match tool.on_key(&event, modifiers) {
        InputDisposition::Handled => return,
        InputDisposition::HandledRequestRedraw => { self.needs_render = true; return; }
        InputDisposition::Pass => continue,
    }
}
// ... existing key handling continues ...
```

Same pattern for `mouse_down/drag/up`.

### §6.4  Search bar tool

`crates/marspot-term/src/tools/search_bar.rs` (new file under
existing `marspot-term` crate; or under `src/` of `marspot` —
**decision: under `src/`** because the tool integrates with L2's
search-state machinery and isn't reusable in `mcli`).

State:

```rust
pub struct SearchBar {
    pub query: String,
    pub case_sensitive: bool,
    pub query_id: u32,
    pub focused: bool,       // user actively typing
    pub counter: Option<(u32, u32)>, // (current, total)
    /// Where in `query` is the text-cursor.
    pub cursor: usize,
    /// IME composition (preedit). Empty when not composing.
    pub preedit: String,
}
```

Rendering — `Overlay` slot:

- Anchor: top-right of grid inner rect
- Size: 30 cols × 3 rows (in cell units), padded
- Contents (rows top to bottom):
  - row 0: `[Aa]` toggle + counter `1 of 23`
  - row 1: query text (with cursor) — uses standard glyph atlas
  - row 2: hint text `Esc: close   Cmd-G: next   ↑↓ select`

Visual primitives: re-use the same glyph emit code as title strip;
draw a 1-cell border using box-drawing chars (same rendering as
selection rectangle).

### §6.5  Result list tool

`src/tools/search_list.rs` (new file).

State:

```rust
pub struct SearchList {
    pub query_id: u32,
    pub hits: VecDeque<SearchHit>,
    pub focused: usize,                 // hits[focused] is selected
    pub visible_top: usize,              // hits[visible_top] is top of viewport
    pub exhausted_older: bool,
    pub pending_more: bool,              // SearchMore in-flight
}
```

Rendering — `Overlay` slot:

- Anchor: below search bar (top-right of grid, below SearchBar rect)
- Size: 50 cols × 12 rows
  - 1 row title `Search results · 1 of 23`
  - 10 rows of hit lines
  - 1 row footer `(scrolling loads more)` when not exhausted

Each hit row:

```
[focus?▶ ] L<line_idx>: ...snippet with **match** highlighted...
```

The `**...**` is rendered using SGR bold reverse for the match
chars within the snippet — no new glyph state.

Load policy:

```rust
fn on_scroll(&mut self, delta: i32) {
    self.visible_top = (self.visible_top as i32 + delta).clamp(0, max);
    if !self.exhausted_older
        && !self.pending_more
        && self.visible_top + VIEWPORT > self.hits.len() - LOAD_THRESHOLD {
        emit(MsgType::SearchMore, ...);
        self.pending_more = true;
    }
}
```

Constants: `VIEWPORT = 10`, `LOAD_THRESHOLD = 8`, `MAX_HITS = 256`.
On overflow trim oldest from head.

### §6.6  Highlight overlay

State lives on `Pane`:

```rust
pub struct ActiveHighlight {
    pub query_id: u32,
    pub spans: Vec<PhysicalSpan>, // copied from focused hit
}
```

Render hook in `build_instances`, in the per-cell BG fill loop:

```rust
let bg = resolve_attrs(cell.attrs).1;
let bg = match &pane.active_highlight {
    Some(hl) if cell_in_any_span(view_row, col, hl.spans) => HIGHLIGHT_BG,
    _ => bg,
};
// existing fill code ...
```

`HIGHLIGHT_BG` = bright yellow with reverse foreground (same
visual language as text selection).

When user navigates `Cmd-G` / clicks a hit, the highlight updates;
`needs_render = true` triggers redraw.

### §6.7  Keybindings (D19)

| key combo | handled by | action |
|---|---|---|
| Cmd+F | CoreApp::key (before tool loop, intercept early) | spawn SearchBar tool if not already; focus it |
| Esc | SearchBar.on_key when focused | clear highlight; remove SearchBar + SearchList tools |
| Cmd+G | SearchBar.on_key OR (when not focused) CoreApp::key | move focused result +1 |
| Shift+Cmd+G | same | move focused result -1 |
| Enter | SearchBar.on_key when focused | jump to focused hit; do NOT close bar |
| ↑ / ↓ | SearchBar.on_key when focused | move focused result |
| printable / Backspace | SearchBar.on_key when focused | edit query; debounce 100 ms; emit new SearchScrollback (cancel old) |
| Cmd+A / Cmd+C / Cmd+V | SearchBar.on_key when focused | text-edit on query |
| Cmd+Q | unchanged — quits app | |

Click on a hit row in SearchList: `SearchList::on_mouse` triggers
the same "jump to hit + highlight" as Enter.

---

## §7  Test inventory (all must pass before FLIP)

### §7.1  marspot-term unit tests

`scrollback::file::tests`:

- `file_create_then_roundtrip_one_line` — create empty file, push 1 line, reopen, read it back.
- `file_create_then_roundtrip_many_lines` — push 5000 lines, reopen, scrollback_len = 5000, sample lines match.
- `file_header_magic_mismatch_renamed` — pre-corrupt the file with bad magic; constructor renames + creates fresh.
- `file_idx_rebuild_on_missing` — delete the `.idx` file; constructor rebuilds it; reads succeed.
- `file_idx_rebuild_on_length_mismatch` — truncate `.idx` to half; constructor rebuilds.
- `file_trailing_partial_record_truncated` — write a record then truncate it mid-way; constructor trims; first read past truncation returns None.
- `file_push_line_hot_path_alloc_count` — assert per-push allocations = 0 (use a counting global allocator harness).
- `file_disk_full_fallback` — mock writer that returns ENOSPC; push_line returns Err and the line goes to RAM ring only.
- `file_wrapped_flag_preserved` — push with wrapped=true; read back wrapped().

`scrollback_search::tests`:

- `search_ascii_substring_finds_three_hits_in_one_line` — text "foo bar foo baz foo"; query "foo"; returns 3 hits with correct col positions.
- `search_case_insensitive_matches_mixed_case` — text "Hello WORLD"; query "world" case_sensitive=false; one hit.
- `search_cjk_no_case_fold_clean_match` — text "中文 你好"; query "你好"; one hit; char offsets correct.
- `search_decawm_wrap_treats_two_rows_as_one_logical_line` — feed text overflowing row; query spanning the wrap returns 1 hit with 2 phys_rows.
- `search_cc_hard_wrap_with_indent_treated_as_continuation` — text "https://example.com/path/\n  to/file"; query "path/to/file" returns 1 hit.
- `search_snippet_clipped_to_line_boundary` — short line < 80 chars; snippet bounds don't extend past line edges.
- `search_snippet_centered_on_match` — long line; query in middle; snippet is match + ~40 chars before + ~40 after.
- `search_no_results_returns_empty_iter` — query "xyzzy" on text "hello"; iter yields nothing.
- `search_cancellation_via_drop_iter` — start a 10k-hit iteration; drop after 1; no panic, no leak.

`shell_proto::tests` (new search frame encoding):

- `encode_decode_roundtrip_search_scrollback_ascii_query`
- `encode_decode_roundtrip_search_scrollback_cjk_query`
- `encode_decode_roundtrip_search_results_empty`
- `encode_decode_roundtrip_search_results_with_two_hits_two_spans`
- `encode_decode_roundtrip_search_more`
- `encode_decode_roundtrip_search_cancel`
- `try_from_u32_returns_none_for_unknown_msg_id_50` (will be obsolete after type added; remove)
- `decode_truncated_payload_returns_err`

`tools::search_bar::tests`:

- `search_bar_typing_updates_query`
- `search_bar_backspace_at_start_no_underflow`
- `search_bar_case_toggle_flips_state_and_requests_redraw`
- `search_bar_cursor_moves_with_arrow_keys`

`tools::search_list::tests`:

- `search_list_scroll_past_threshold_emits_pending_more`
- `search_list_caps_hits_at_256_trims_head`
- `search_list_focused_navigation_wraps_or_clamps`

### §7.2  marspot-session integration tests

Add to `crates/marspot-session/src/main.rs` or new `tests/` integration file:

- `l3_search_returns_results_within_50ms_on_1mb_scrollback` — set up an L3 with synthetic 10 k lines; send a SearchScrollback frame; assert SearchResults frame arrives < 50 ms with valid hits.
- `l3_cancel_drops_inflight_search` — start search, send Cancel immediately; assert no more SearchResults arrive after cancel.
- `l3_last_write_wins_new_query_supersedes_old` — start search A, then search B before A finishes; assert only B's results arrive.

### §7.3  marspot core (L2) integration tests

Add to `src/bin/marspot-core.rs` test module:

- `core_search_initial_query_request_emitted`
- `core_search_results_populate_search_list_in_order`
- `core_search_focused_navigation_updates_highlight`
- `core_search_jump_forwards_correct_view_offset`
- `core_pane_tool_layout_shrinks_grid_on_top_fixed`
- `core_pane_tool_layout_leaves_grid_alone_on_overlay`

### §7.4  Visual / E2E (pre-FLIP smoke)

These run AFTER all unit + integration pass.  Done as part of the
FLIP commit's verification — manual, with a written checklist:

1. Build clean: `cargo build --release` → finishes without errors.
2. Run mini bench: `bin/bench-remote.sh` 11/11 PASS with
   tolerances of ≤ 5 % on any metric vs. the previous baseline.
3. Run `bin/test.sh` (full nextest) → 100 % green.
4. `MARSPOT_FILE_SCROLLBACK=1 bin/run.sh` in dev sandbox:
   - feed 1000 lines of test text
   - Cmd+F, search, see results stream in
   - click a hit, view jumps + highlight appears
   - Esc closes search
   - scroll-up still works
   - kill and relaunch — scrollback persists
5. Install: `bin/install-local.sh`
   - 8 existing panes self-execv; scrollback survives (via snapshot
     v2 backstop)
6. Real-use soak (≥ 30 min) of typing + scrolling + searching
   without any visible glitch.

Only after #1-#6 all pass does the FLIP commit get pushed.

---

## §8  State transition map — commit-by-commit

Each row = one commit to develop.  "User state" = what the running
app does.  "Repo state" = what's in code.

| # | step | DoD ref | user state | repo state | rollback |
|---|---|---|---|---|---|
| 1 | A1 | §12.A1 | unchanged (file constructor not wired) | new module `marspot-term::scrollback::file`; `Scrollback::File` variant; constructor + push_line + cell_at + tests | revert commit |
| 2 | A2 | §12.A2 | unchanged | reader tiering (RAM ring → mmap → pread); LRU not needed; tests | revert |
| 3 | A3 | §12.A3 | unchanged unless user exports `MARSPOT_FILE_SCROLLBACK=1` | Terminal::new branches on env; opt-in only | revert |
| 4 | A4 | §12.A4 | unchanged | snapshot v2 applies into File scrollback when opt-in active | revert |
| 5 | B1 | §12.B1 | unchanged (engine not called) | `scrollback_search` module + algorithm + tests | revert |
| 6 | B2 | §12.B2 | unchanged (no handler) | MsgType 50-53 added; encode/decode helpers + tests | revert |
| 7 | B3 | §12.B3 | unchanged unless `MARSPOT_FILE_SCROLLBACK=1` AND L2 sends search frames (which it doesn't yet) | L3 handler + worker thread + integration tests | revert |
| 8 | B4 | §12.B4 | unchanged | live-grid scan inside worker | revert |
| 9 | C1 | §12.C1 | unchanged (no tool occupied) | PaneTool/ToolSlot framework; layout math; render hook | revert |
| 10 | C2 | §12.C2 | unchanged (Cmd+F not bound) | SearchBar tool implementation + tests | revert |
| 11 | C3 | §12.C3 | unchanged | SearchList tool + virtual scroll + tests | revert |
| 12 | C4 | §12.C4 | unchanged | highlight overlay path in build_instances | revert |
| 13 | C5 | §12.C5 | unchanged (Cmd+F intercept active only when env set) | full key routing with env gate | revert |
| 14 | **FLIP** | §12.F1 | **feature live on next install** | env gate removed; Terminal::new default = File; Disk variant marked `#[deprecated]` | revert (back to env-gated) |
| 15 | F2 | §12.F2 | unchanged | remove Disk variant (cleanup); update bench harnesses if any depended on it | revert |

Between commits 1-13, the user can `bin/install-local.sh` freely
and see zero behavioural change (env gate keeps everything dormant).

The FLIP (commit 14) is the moment behaviour changes.  We do
`bin/install-local.sh` immediately after, watch for issues, and if
anything breaks revert just commit 14 to fall back to env-gated.

Commit 15 cleans up Disk.  We do that ~1 week later if everything
is stable.

---

## §9  Rollback strategy

### §9.1  Per-commit revert

Each commit is self-contained.  Within the 1-13 sequence:

```bash
git revert <commit-N>
```

Removes that commit's changes.  Subsequent commits don't depend on
N's behaviour change (only on its code surface — which the revert
also unwinds).  Reverting in reverse order (N then N-1 then...) is
recommended to keep diffs small.

### §9.2  Full revert

If we need to abandon the whole rollout post-FLIP:

```bash
git revert <FLIP_COMMIT>          # back to env-gated; users unaffected unless they set env
# Then optionally:
git revert <commit-15>..<commit-1>  # remove all code
```

The env-gated state (between FLIP revert and full revert) is a
SAFE landing zone — code exists, default behaviour is unchanged.

### §9.3  Data rollback

Once File scrollback is live (post-FLIP), `scrollback.bin` and
`scrollback.idx` files accumulate on disk.  Reverting FLIP does
NOT delete them.  They stay benign — the Disk variant doesn't
read them.

If a user manually wants to wipe: `rm -rf
~/Library/Caches/marspot/sessions/*/scrollback.*`.  Document this
in the release notes.

### §9.4  Bench regression rollback

If mini bench regresses on any commit:

1. Stop the rollout at that commit.
2. Profile (samply or sample).
3. If fixable: amend or follow-up commit.
4. If structural: revert + amend the design doc + start the step over.

NO commit lands with bench regression — even an intermediate one.

---

## §10  Failure modes (search runtime)

| condition | behaviour |
|---|---|
| empty query | clear highlight; clear results; do nothing |
| query > 256 chars | accept; UI input doesn't grow further (clip) |
| 100k+ matches | stream first 64; counter says "23 of many"; SearchMore lazy-loads |
| no matches | result list shows "0 results" |
| L3 crash during search | L2 reader thread sees EOF on UDS; existing reconnect path runs; L2 closes search bar before reconnect |
| L3 self-execv during search | search bar closes (D20); user re-opens manually |
| pane close mid-search | L2 sends SearchCancel; tool removed with pane |
| live grid scrolls a row into scrollback mid-search | iterator's snapshot of `.bin` length is at iterator init; new lines are NOT included in current results; on next user-initiated search they're included |
| highlight overlay vs selection overlay collision | highlight wins (drawn on top); selection survives; user-initiated copy still gets selection text |
| Cmd-F while a PaneSession (cc plugin) has the keyboard | search bar opens anyway (Cmd-F intercept is at the top level); PaneSession sees no key event for F |
| disk full on search worker temp file | n/a — search doesn't write temp files in v1 |
| query contains regex metachars | treated literally — D9 confirmed |

---

## §11  Definitions / Glossary

- **Logical line** — sequence of physical rows joined by DECAWM
  wrap flag OR cc-hard-wrap-with-indent heuristic (D11).
- **Physical row** — one row in the grid or one record in
  `scrollback.bin`.
- **norm_text** — search-searchable text of a logical line:
  hanging-indent stripped, wraps merged, raw `\n` removed.
- **raw_text** — verbatim text including the wrap-induced `\n`
  and any leading spaces (used only for char-position bookkeeping).
- **logical_line_idx** — append-order index into scrollback for a
  logical line; for live-grid hits, `u64::MAX - row_offset`.
- **query_id** — monotonic u32 on L2, stamped on every wire frame
  related to a query; used to discard stale results.
- **ToolSlot** — fixed top / fixed bottom / overlay placement.
- **PaneTool** — render-and-input widget plugged into a ToolSlot.
- **FLIP** — single commit where the feature becomes the default.
- **Mini bench** — `bin/bench-remote.sh` 11-check gate; truth source.

---

## §12  Definition of Done — per step

A step is "done" only when ALL of its DoD bullets verify true.
Skipping any bullet is forbidden by P5.

### §12.A1  Scrollback::File scaffold

- [ ] `crates/marspot-term/src/scrollback.rs` has `Scrollback::File(FileScrollback)` variant; all match arms handle it (push_line, len, capacity, cell_at, line_to_vec, clear).
- [ ] `FileScrollback::new(bin_path, idx_path, cols, ram_capacity)` constructor handles: create-if-missing, magic/version/cell_abi validate, idx rebuild on length mismatch, trailing partial record trim.
- [ ] Hot path `push_line` adds < 500 ns vs. Memory variant on mini parse bench (measured via a focused micro-bench; see `bin/bench.sh --release` extended).
- [ ] All 9 file unit tests in §7.1 pass.
- [ ] No new external crate dependencies.
- [ ] `cargo nextest run --lib` 100 % green.
- [ ] `bin/bench-remote.sh` 11/11 PASS, no metric regresses > 2 %.

### §12.A2  Reader tiering

- [ ] `cell_at` / `line_to_vec` / `scrollback_wrapped` all consult RAM ring → mmap window → pread fallback.
- [ ] mmap window auto-slides on file growth (test: write past current window end; next read maps the new tail).
- [ ] Cold read p99 < 100 µs on synthetic 50 k-line file.
- [ ] `bin/bench-remote.sh` 11/11; scroll-cold p99 ≤ baseline + 5 %.
- [ ] All 9 reader-tier related sub-tests pass.

### §12.A3  Terminal::new opt-in

- [ ] `marspot_term::terminal::Terminal::new(cols, rows)` checks env `MARSPOT_FILE_SCROLLBACK`; "1" → File variant, else → unchanged Disk.
- [ ] File path resolves via new `paths::scrollback_bin_path(id) / scrollback_idx_path(id)`.  Needs `MARSPOT_SESSION_ID` to be present (it is, in marspot-session main).
- [ ] Cold-start with file: file exists → existing scrollback loaded; file missing → created empty.
- [ ] Self-execv with file: new image opens by path; scrollback continues.
- [ ] One smoke integration test: set env, create 100 lines, exit, reopen, assert 100 lines present.

### §12.A4  Snapshot v2 + File integration

- [ ] `apply_snapshot` v2 scrollback section feeds `push_historic_scrollback_line`, which under env=1 writes through to the file.
- [ ] Test: serialize v2 snapshot containing scrollback; create fresh Terminal with env=1; apply_snapshot; reopen; assert scrollback persisted.
- [ ] No regression in existing 12 snapshot tests.

### §12.B1  Search algorithm

- [ ] `marspot_term::scrollback_search::search_scrollback` returns boxed iterator with cancellation via drop.
- [ ] All 9 search algorithm unit tests pass.
- [ ] Performance: 1 MB file first 64 hits < 20 ms (asserted in a test using `Instant::now`).

### §12.B2  Wire types

- [ ] `MsgType` extended with 50-53; `try_from_u32` arms added.
- [ ] `shell_proto::encode_search_*` / `decode_search_*` helpers + 8 encode/decode tests pass.
- [ ] `decode_frame` in marspot-core grows arms for the new types — but they return `None` for now (no handler yet) so old behaviour preserved.

### §12.B3  L3 handler

- [ ] L3 main loop handles `MsgType::SearchScrollback`: spawn worker thread; wire SearchResults frames back through poke.
- [ ] `SearchCancel` sets worker cancel flag; worker exits within 1 ms of next iteration.
- [ ] `SearchScrollback` arriving with new query_id auto-cancels old worker.
- [ ] All 3 marspot-session integration tests pass.
- [ ] `bin/bench-remote.sh` 11/11.

### §12.B4  Live-grid merge

- [ ] Worker scans live grid before scrollback file.
- [ ] Hits in live grid have `logical_line_idx >= u64::MAX - rows`.
- [ ] Test: feed query matching only live grid; assert hit returned with correct synthetic index.

### §12.C1  PaneTool framework

- [ ] `PaneTool` trait + `ToolSlot` enum + `InputDisposition` enum live in `crates/marspot-term/src/render.rs`.
- [ ] `Pane` has `tools: Vec<Box<dyn PaneTool>>`; default empty.
- [ ] `build_instances` per-pane block computes `top_fixed_h`, `bot_fixed_h`, draws title strip + fixed tools + grid + overlay tools + highlight in that order.
- [ ] Empty `tools` → byte-identical render to pre-C1 (assert via existing render unit tests).
- [ ] 2 new tests for layout shrink on fixed tool: TopFixed and BottomFixed.

### §12.C2  Search bar

- [ ] `src/tools/search_bar.rs` implements `PaneTool` with `slot = Overlay`.
- [ ] Tool emits `MsgType::SearchScrollback` (via CoreApp) when query changes (debounced 100 ms).
- [ ] Rendering occupies a 30 × 3 cell area at top-right of pane.
- [ ] All 4 SearchBar unit tests pass.

### §12.C3  Result list

- [ ] `src/tools/search_list.rs` implements `PaneTool` overlay.
- [ ] Virtual list scrolls; loads SearchMore when threshold crossed.
- [ ] Hit clicks call back into CoreApp to set highlight + scroll pane.
- [ ] All 3 SearchList unit tests pass.

### §12.C4  Highlight overlay

- [ ] `Pane::active_highlight: Option<ActiveHighlight>` slot.
- [ ] `build_instances` BG fill loop consults highlight per cell.
- [ ] Multi-row spans render correctly (test with synthetic wrap).
- [ ] `bin/bench-remote.sh` 11/11; render p99 increase from C4 ≤ 50 µs.

### §12.C5  Keybindings + final integration

- [ ] Cmd+F intercept (gated on env=1) spawns SearchBar + SearchList tools.
- [ ] All other keybindings per §6.7 work.
- [ ] All marspot-core integration tests pass.
- [ ] `bin/test.sh` full nextest green.
- [ ] `bin/bench-remote.sh` 11/11.
- [ ] Visual e2e checklist §7.4 #4 passes (env-gated build, dev sandbox).

### §12.F1  FLIP

- [ ] Env gate removed; Terminal::new default = File; Disk marked `#[deprecated]`.
- [ ] Cmd+F intercept always active (no env check).
- [ ] All tests pass.
- [ ] Visual e2e checklist §7.4 #1-#6 all pass including the install-local + real-use soak.
- [ ] Bump version-vector.toml: shell, core, session all minor-bump.
- [ ] Commit message includes the full rollout summary referencing the design doc.

### §12.F2  Cleanup (T+1 week)

- [ ] If no rollback by then: remove `Scrollback::Disk` variant.
- [ ] Remove env handling for `MARSPOT_FILE_SCROLLBACK` (no longer used).
- [ ] Update bench harnesses or tests that used Disk explicitly.

---

## §13  Out-of-scope (now)

For absolute clarity:

- **Cross-pane search** — not v1, not Phase D, not on the roadmap.
- **L2-level archive search** ("search history even when no pane
  open") — future separate project; this design does not block it.
- **Fuzzy / regex** — dropped per D9.
- **Search bar moving / pinning** — overlay default per D7; pin is
  a future tool option, not v1.
- **Per-line timestamps** — D5 reserved header flag, no impl.
- **Indexed / inverted search** — D2 deferred.
- **`scrollback.bin` eviction / truncation** — D12 no v1.
- **Searching across multiple sessions on disk** — out.

---

## §14  Related docs / memory

- `docs/architecture.md` — current layered model
- `docs/per-session-l3.md` — L3 process model + shm wire
- `docs/rfc-003-l3-pty.md` — wire amendments
- `docs/silent-update.md` — execv contract for L3
- memory `project-scrollback-persistent` — original sketch
  (superseded by this doc)
- memory `feedback-wire-upgrade-silent-lossless` — protocol rule
- memory `feedback-ceiling-first` — perf is the priority
- memory `feedback-e2e-required-before-claiming-done` — DoD
  enforcement
- memory `feedback-linear-no-defer` — operating principles P1-P6

---

## §15  Sign-off checklist (before A1 starts)

User reviews and confirms:

- [ ] §0 product principle ("性能必须极佳;功能不要都可以") is the
  unbreakable axis for the whole rollout
- [ ] Goals + non-goals in §0.5/§1 match user intent
- [ ] All decisions D1-D20 are correct
- [ ] Commit sequence in §8 is acceptable
- [ ] Rollback strategy in §9 is sufficient
- [ ] DoD checklists in §12 are appropriate gates
- [ ] Every perf budget in §3.6 / §4.6 / §12 is a hard ceiling I'm
  willing to drop features to defend
- [ ] No mid-execution decisions remain (search this doc for
  "decide later" / "TBD" / "?" — none should remain)
- [ ] Scope is correct (§13 cuts match user intent)

Once these are all green, implementation begins at A1.  No
mid-execution alteration of any of the above; only stop-revise-
resume cycles per P1.

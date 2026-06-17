# Pane upgrade — persistent scrollback + search + toolbar framework

A living design.  Replaces the in-process anonymous-mmap scrollback
(26 k-line cap, lost on execv) with a **per-session persistent
file**, layers a **full-history search engine** on top of it, and
introduces a **pane toolbar framework** that the search UI is the
first user of.  basic-level feature — lives in `marspot-term`,
`marspot-session`, `marspot-core`.  No cc plugin involvement.

Status: design (no code yet).  Tracking memory:
[[project-scrollback-persistent]] in `~/.claude-profile-3` for
cross-session continuity.

---

## 0.  Goals & non-goals

### Goals

1. **Unbounded scrollback depth** per session.  No hard cap; only
   policy-bounded (file-size threshold) eviction at the head.
2. **Daily hot-path stays at zero overhead.**  parser → grid
   push_line adds at most one memory write + one buffered file
   append.  No fsync, no syscall-per-cell, no allocations.
3. **Silent update preserves full scrollback.**  L3 self-execv
   reopens the same file by path — no replay through snapshot,
   no fd handoff, no quirky edge cases.
4. **Crash-resilient.**  Hard kill of L3 (panic, OOM) leaves the
   scrollback file in a recoverable state on next attach.  Worst
   case: the trailing in-flight write is truncated; everything
   before it survives.
5. **Full-history search.**  Cmd-F in a focused pane opens an
   inline search bar; query runs over the entire scrollback +
   visible grid.  Results stream back as they're found.
6. **Result-list UI** lets the user pick a match; clicking jumps
   the pane to that scrollback position and highlights the run in
   the body.  List is a **bidirectional virtual list** with lazy
   loading — opening at a deep match doesn't pre-fetch unrelated
   results.
7. **Wrap-aware.**  A URL that DECAWM-wrapped across two physical
   rows is one logical match, not two; highlight spans both rows.
8. **Pluggable toolbar framework.**  Search bar is the first
   inhabitant of a pane-toolbar abstraction that future features
   (status pills, mini-buffers, etc.) reuse.

### Non-goals (for v1)

- Cross-session search (each pane searches only its own history).
- Regex / glob / fuzzy search.  Reserved hooks only; ship with
  literal substring + case-toggle.
- Indexed search (inverted index, n-gram).  Linear scan is fast
  enough at the sizes we expect; defer until profiling forces it.
- L2-side scrollback ownership.  L3 stays the source of truth; L2
  is a thin client that requests pages + search.

---

## 1.  Layered architecture

```
            ┌─────────────────────── L2 (marspot-core) ─────────────┐
            │                                                       │
            │  PaneSearchState   ▶  controls floating UI + keys     │
            │  ToolbarLayer      ▶  fixed-strip + overlay layout    │
            │  Highlight overlay ▶  render hook on mirror grid      │
            │                                                       │
            │       │ wire frames                                   │
            ▼       ▼                                               │
            SearchScrollback / SearchResults / SearchCancel /       │
            SearchMore / GetScrollbackPage (existing)               │
            ▲                                                       │
            │ wire frames                                           │
            ┌─────────────────────── L3 (marspot-session) ──────────┘
            │
            │  SearchEngine      ▶  scans persistent file + RAM ring
            │  ScrollbackFile    ▶  append-only data + sidecar idx
            │  Terminal::grid    ▶  live grid (unchanged)
            │
            ▼
            ┌─────────────────────── disk (per session) ────────────┐
            │   sessions/<id>/scrollback.bin                        │
            │   sessions/<id>/scrollback.idx                        │
            └───────────────────────────────────────────────────────┘
```

L1 (marspot-shell) is uninvolved.  Search is between L2 (UI) and L3
(storage + engine), as it should be per RFC-003 §6.

---

## 2.  Storage layer — persistent scrollback

### File layout

Two files per session, in `~/Library/Caches/marspot/sessions/<id>/`:

#### `scrollback.bin`

Append-only.  Format:

```text
[magic    u32]   = 0x5350_5301 ('SSP\x01' little-endian)
[version  u32]   = current = 1, min_compat = 1
[cell_abi u32]   = stamp of Cell layout version — bumped on any
                   change to the parser-emitted Cell encoding so
                   a v1-image reading a v2-file rejects cleanly
                   instead of mis-interpreting bytes
[reserved u32]   = 0 for v1, future flags

repeat per logical line, append order = arrival order:
  [len      u32 LE]      total bytes in this record, excluding
                         this length prefix — lets a scanner that
                         hit a torn tail (crash mid-write) seek
                         past the truncated entry on recovery
  [wrapped  u8]          DECAWM continuation flag (does this row
                         continue the previous?)
  [cols     u16 LE]      width of this row in cells
  [cells]   cols × CELL_BYTES   same encoding as
                                `serialize_snapshot` cell payload
```

Append rule: each record is written `len`-then-body so any reader
can validate "have we got the full record?" before parsing.

#### `scrollback.idx`

Dense `[u64 LE byte_offset]` array — index N points to the start of
the Nth `[len]` prefix in `scrollback.bin`.  Initial entry [0] = 0.

Idx file always has `len(idx) = lines_in_bin + 1`.  Last entry =
EOF of `.bin` (sentinel for "scrollback length is N").

#### Why sidecar idx instead of in-file index?

- O(1) line_idx → byte_offset without scanning
- Cheap to rebuild from `.bin` on idx-corruption (every record is
  self-delimiting)
- Append-side is two sequential writes (data, then a u64 tail),
  both buffered

### Hot path — push_line

`Scrollback::File::push_line(&mut self, line: &[Cell], wrapped: bool)`:

1. Serialise to **thread-local pre-allocated scratch buffer**
   (reuse — zero alloc).
2. Memcpy scratch → in-memory ring tail (current LRU slot).
3. Write scratch → `BufWriter<File>` for `.bin`.
4. Write current `.bin` tail offset → `BufWriter<File>` for `.idx`.

No flush.  No fsync.  Kernel decides when to write pages to disk
(typically ~30 s on macOS unless memory-pressure'd).  A crash
might lose the trailing N seconds of scrollback; that's acceptable
since the parser itself still has the bytes coming.

Cost budget (M-series, target):

| step | cost |
|---|---|
| serialise 80 cells to scratch | ~200 ns |
| ring-tail memcpy 1 KiB | ~50 ns |
| BufWriter write (in-RAM buffer) | ~20 ns |
| **total per push_line** | **< 300 ns** |

Compare to parser cost of feeding 80 chars: typically 5-20 µs.
Storage adds < 5 %.

### Read path — `cell_at(idx, col)`

Tiered:

```text
1. RAM ring (most-recent N = 1024 lines)
     in-memory Vec<Cell>; O(1) lookup
     hit rate ≥ 99 % for any user not actively deep-scrolling

2. mmap window (next ~10 k lines from the bin's tail)
     mmap the tail page-aligned region read-only; kernel pages
     in on first touch, swaps out under pressure
     cost = first hit ~10 µs (page fault), warm ~50 ns

3. lseek + pread (deep history)
     uncached file read; ~50-100 µs per page (NVMe)
     happens only when user scrolls many MB into the past or
     a search hit lands deep
```

Lookup algorithm:

```
let bin_offset = idx[line_idx];        // O(1) from idx mmap
let record_len = u32_le(bin_at(bin_offset));
// `cells_offset = bin_offset + 4 (len) + 1 (wrapped) + 2 (cols)`
let cell = decode(bin_at(cells_offset + col * CELL_BYTES));
```

Idx file is small (8 bytes × line count) — mmap it entirely.

### Eviction policy

When `.bin` exceeds `SCROLLBACK_FILE_SOFT_CAP` (configurable, default
200 MB ≈ 4 M lines of 80-col text):

- Truncate the oldest M lines (head eviction)
- Rewrite `.idx` (shift offsets, drop M entries)
- Atomic via rename of new files; old fd stays valid until next
  reopen

Eviction runs **off the hot path** in a background thread fired by
a watermark check on every Nth push_line.

### Lifecycle

| event | action |
|---|---|
| L3 cold start (no file) | create empty `.bin` + `.idx` with headers |
| L3 cold start (file exists) | validate magic+version+cell_abi; mmap idx; load last 1024 lines into RAM ring |
| L3 push_line | append to `.bin` + `.idx` (hot path) |
| L3 self-execv | new image reopens by path; nothing to hand off |
| L3 graceful exit | flush BufWriters, fsync (optional) |
| L3 crash | next attach validates `.bin` length matches idx tail; trims trailing partial record |
| pane closed by user | file kept on disk; reopened if pane is restored from registry, else garbage-collected by a sessions-dir reaper |

### snapshot v2 demotion

Once persistent scrollback ships, `serialize_snapshot` no longer
needs to carry scrollback — the file already does.  Demote v2
scrollback section to **256-line backstop** (or remove entirely)
to keep state.bin small.  The cost of removing entirely: if file
is corrupted at the same moment as execv, we lose history.  256
lines is cheap insurance.

---

## 3.  Search engine

### Algorithm — v1 (literal substring)

L3-side, runs on demand.  Scans **logical lines** (groups of
physical rows joined where `wrapped == true`) so a match crossing
a DECAWM soft-wrap is one hit, not two.

```
fn search(query: &str, opts: SearchOpts) -> impl Iterator<Item = Hit>
```

```text
let mut buf = String::with_capacity(query.len() * 4 + 256);
for logical_line in scrollback_logical_lines().rev() {  // newest-first
    buf.clear();
    write_chars_into(logical_line, &mut buf);
    let needle = if opts.case_sensitive {
        Cow::Borrowed(query)
    } else {
        // lowercase both sides once per line, not per byte
        buf = buf.to_lowercase();
        Cow::Owned(query.to_lowercase())
    };
    for (byte_offset, _) in buf.match_indices(&*needle) {
        yield Hit {
            logical_line_idx: logical_line.idx,
            char_offset: byte_to_char(&buf, byte_offset),
            char_len: query.chars().count(),
        };
    }
}
// Then also scan the live grid for hits since the visible window
// may contain newer content than the most recent scrollback line.
```

Logical-line iterator walks the idx in reverse; groups consecutive
records where `wrapped == true` on row N+1 onto row N's text.

### Performance budget

| scrollback size | scan time (target) | rationale |
|---|---|---|
| 1 MB (~80 k cells, ~5 k lines) | < 5 ms | typical claudecode session |
| 10 MB (~50 k lines) | < 50 ms | long-running shell |
| 100 MB (~500 k lines) | < 500 ms | extreme; show "searching…" |

At 100 MB we may need a sidecar text-only mirror (see §10).  v1
ships linear scan.

### Streaming + cancellation

Searches return results **streaming** over the wire, not as one
big payload:

```
L2 → L3:  SearchScrollback { query_id, query, opts, max_total }
L3 → L2:  SearchResults    { query_id, hits: Vec<Hit>, has_more: bool }
L2 → L3:  SearchMore       { query_id, count, direction }  (paginated)
L2 → L3:  SearchCancel     { query_id }
```

L3 stamps every result frame with the query_id; an L2 that has
typed a new query (= new query_id) discards stale results.

L3 sends results in chunks of `min(max_total, 64)`.  After each
chunk it checks for `SearchCancel` on the control socket; cancelled
queries drop their iterator and free memory.

### Result shape

```rust
pub struct SearchHit {
    pub logical_line_idx: u64,   // index into the logical line stream
    pub char_offset: u32,        // chars into the logical line
    pub char_len: u32,           // length of the match in chars
    pub snippet: String,         // ±32 chars context around match,
                                 // pre-rendered with the match marked
    pub physical_rows: Vec<(u16, u16, u16)>,
                                 // (phys_row_idx_in_logical, col_start, col_end)
                                 // — at most one entry per wrapped row.
                                 // Used for L2 highlight rendering.
}
```

The `physical_rows` slice is what makes wrap-aware highlight work:
L2 doesn't need to re-derive how a logical match maps to physical
rows; L3 already knows from the wrapped flags in its scrollback.

---

## 4.  Wire protocol

### New message types (`MsgType` extensions)

| name | direction | payload (CBOR-like) |
|---|---|---|
| `SearchScrollback` | L2 → L3 | `{ query_id: u32, query: String, case_sensitive: bool, max_total: u32 }` |
| `SearchResults`    | L3 → L2 | `{ query_id: u32, hits: Vec<SearchHit>, has_more: bool, total_seen: u32 }` |
| `SearchMore`       | L2 → L3 | `{ query_id: u32, count: u32, direction: u8 /* 0=older, 1=newer */ }` |
| `SearchCancel`     | L2 → L3 | `{ query_id: u32 }` |

All new types follow `feedback-wire-upgrade-silent-lossless`:
unknown msg_type ⇒ silently skip + log at TRACE level.

Wire format reuses the existing `Frame` (msg_type + len + payload)
with `bincode`-encoded payloads (consistent with current frames).

### Existing types reused

- `GridScroll(view_offset)` — clicking a hit forwards
  `view_offset = computed_line_idx → row_offset` (L2 maths it from
  the snapshot's `scrollback_len`).
- `GridReady` — fires on any publish, including the one after L3
  honoured a SearchScrollback-triggered scroll.

---

## 5.  Pane toolbar framework

### Why a framework

Search is the first inhabitant.  Future features (status pills,
mini-buffer prompts, ephemeral notifications) will reuse the same
slots.  Solving "where does this UI element live" once now avoids
ad-hoc per-feature placement code.

### Slot model

Each pane has these toolbar slots:

```text
┌─────────────────────── pane rect ───────────────────────────────┐
│   ┌──────────────── title strip ───────────────────────────────┐│
│   │  N. label   ●  cc-plugin badge                  [≡]  [⟳]  ││  ← title strip (existing)
│   └────────────────────────────────────────────────────────────┘│
│   ┌────────────────────────────────────────────────────────────┐│  ← TOP_FIXED slot
│   │  optional: pinned tool — claims height, grid shrinks       ││     (default: empty)
│   └────────────────────────────────────────────────────────────┘│
│   ┌────────────────────── grid ────────────────────────────────┐│
│   │                                                            ││
│   │  ┌─ floating overlay — search bar ─┐                       ││  ← OVERLAY (free position)
│   │  │ [aA] [.* ] [query: foo___]      │                       ││
│   │  └─────────────────────────────────┘                       ││
│   │                                                            ││
│   │  ┌── floating overlay — result list ──┐                    ││  ← OVERLAY (free position)
│   │  │  > L2304: ... foo bar ...          │                    ││
│   │  │    L2287: ... foo qux ...          │                    ││
│   │  │    L2102: ... foo zoz ...          │                    ││
│   │  └────────────────────────────────────┘                    ││
│   │                                                            ││
│   └────────────────────────────────────────────────────────────┘│
│   ┌────────────────────────────────────────────────────────────┐│  ← BOTTOM_FIXED slot
│   │  optional: pinned tool — claims height, grid shrinks       ││     (default: empty)
│   └────────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
```

Slot types:

- **FIXED** (`TOP_FIXED`, `BOTTOM_FIXED`): claim a fixed height,
  layout shrinks the grid to fit, content is visible regardless
  of grid scroll position.  Used for things the user must always
  see while present.  Empty by default.
- **OVERLAY**: positioned over the grid at coordinates the tool
  picks; grid layout is unchanged; tool is responsible for not
  obscuring critical content.  Used for transient / dismissable
  UI.  Z-order: title strip > overlays > grid.

### `PaneTool` trait

```rust
pub trait PaneTool {
    /// Where does this tool live?
    fn slot(&self) -> ToolSlot;
    /// If FIXED: how many cell rows tall?  Layout shrinks grid by this.
    /// If OVERLAY: ignored (tool controls own size).
    fn fixed_height_rows(&self) -> u16 { 0 }
    /// Render hook — fills its rect with cell instances + glyph
    /// instances directly (same primitives as the title strip).
    fn render(&self, layout: &ToolLayout, ...);
    /// Input routing — return Handled to stop event propagation.
    fn on_key(&mut self, event: &MarspotKeyEvent, mods: Modifiers) -> InputDisposition;
    fn on_mouse(&mut self, ...) -> InputDisposition;
}
```

Search bar implements `PaneTool` with `slot = OVERLAY`.  Future
"pinned scratch buffer" tool would implement `slot = BOTTOM_FIXED`.

### Search-specific UI

#### Search bar

Floating overlay, anchored to top-right of grid (Chrome / VS Code
convention).  Fixed pixel size: ~360 px wide × ~36 px tall.
Contents:

```
┌─────────────────────────────────────────────────────────────┐
│  [Aa] [.* ] [⨯]                              [↑] [↓] [✕]   │
│  ┌────────────────────────────────────────────────────────┐ │
│  │ query text                                             │ │
│  └────────────────────────────────────────────────────────┘ │
│                                              23 of 412     │
└─────────────────────────────────────────────────────────────┘
```

Toggles:

- `Aa` — case sensitive on/off (visual: highlighted = on)
- `.* ` — fuzzy / regex placeholder.  Disabled in v1.  Reserved
  so the toggle is visible from day one but greyed out.
- `⨯` — clear query

Buttons:

- `↑` / `↓` — prev / next match (also bound to Cmd-G / Shift-Cmd-G)
- `✕` — close (also Esc)

Counter shows current focused result index / total matches found
so far.  Updates live as L3 streams more results.

#### Result list

Floating overlay, anchored below search bar.  Max 10 rows tall.
Virtual list — pre-computes only the visible window of cached
results, requests more as user scrolls.

Each row shows:

```
> L2304 [12:34:05] ... claude responded with foo bar baz ...
```

- Pointer ▶ marks focused row
- `L2304` = scrollback line index (debug-y; could be a relative
  "1234 lines ago")
- `[12:34:05]` = optional timestamp if the L3 captures one (future)
- Snippet text with the matched span underlined / highlighted

Click row → jumps to that scrollback position + highlights match
in the grid body.

### Bidirectional virtual list

State:

```rust
struct ResultWindow {
    query_id: u32,
    /// Loaded results, sorted by logical_line_idx descending
    /// (newest match first).
    hits: VecDeque<SearchHit>,
    /// Index into `hits` of the row visually at the top of the
    /// result list viewport.
    visible_top: usize,
    /// Which hit row is currently focused (selection cursor).
    focused: Option<usize>,
    /// Have we reached the oldest line in scrollback?
    exhausted_oldest: bool,
    /// Have we reached the newest line (visible grid bottom)?
    exhausted_newest: bool,
    /// In-flight SearchMore request, if any.
    pending_load: Option<(Direction, u32 /* count */)>,
}
```

Load policy:

- Initial: `SearchScrollback { ..., max_total: 64 }` — first batch.
- User scrolls down past `visible_top + visible_count >= hits.len() - 8` → emit `SearchMore { direction: older, count: 64 }`.
- User scrolls up past `visible_top <= 8` → if there might be
  newer results (live grid hits, or `exhausted_newest == false`),
  emit `SearchMore { direction: newer, count: 64 }`.
- Cap `hits` at 256 entries.  Trim from the side opposite to scroll
  direction to keep memory bounded — gives the feeling of
  infinite list without unbounded growth.

### Highlight rendering

Active highlight is a `(query_id, logical_line_idx, char_offset,
char_len)` tuple stored on `PaneSearchState`.  When the renderer
walks cells for the focused pane, it consults the highlight rect
and overrides background to the highlight colour for matched
cells.  Span crossing wrapped rows → multiple physical rects, all
drawn.

When user navigates to next/prev match, this tuple updates; renderer
takes care of redraw.

---

## 6.  Keybindings

| key | action | scope |
|---|---|---|
| Cmd+F | open search bar in focused pane | global (intercepted by L2) |
| Esc | close search bar; clear highlight | search-active only |
| Cmd+G | next match (focus next result) | search-active only |
| Shift+Cmd+G | prev match | search-active only |
| Enter | jump to focused result | search-active only |
| ↑ / ↓ (in search bar) | move focused result | search-active only |
| any printable key | append to query, debounce, re-search | search-active only |
| Backspace | edit query | search-active only |
| Cmd+A (in search bar) | select-all in query | search-active only |
| Cmd+C (in search bar) | copy query selection | search-active only |
| Cmd+V (in search bar) | paste into query | search-active only |

While search bar is active, keystrokes do NOT forward to the PTY.
This is the same "lock keys" mechanism RFC-003 §6 Amendment 17 uses
for plugin-held PaneSessions.

---

## 7.  Failure modes & edge cases

| scenario | behaviour |
|---|---|
| `.bin` corrupted (magic mismatch) | log + delete + start fresh; user loses history but session keeps running |
| `.idx` corrupted but `.bin` OK | rebuild `.idx` by linear-scanning `.bin` on attach; one-time ~50 ms hit |
| L3 crashes mid-write | next attach truncates trailing partial record (validated via `[len]` prefix); rest survives |
| disk full | `push_line` write returns ENOSPC; fall back to in-RAM ring; log; do not panic |
| L3 self-execv during search | iterator dropped; result query_id stale → L2 sends SearchCancel + restarts |
| search query empty | clear results; clear highlight; do nothing |
| 100 k+ matches | stream first 64; user sees "loaded 64 of many" with live counter; lazy-loads more on scroll |
| no matches | result list shows "0 results" placeholder |
| user closes pane mid-search | drop iterator; remove search state |
| L2 swap (dual-core) during search | new L2 reattaches but does not auto-restore search; user re-opens search bar if needed |
| live grid scrolls a row into scrollback during search | next SearchMore window picks it up; existing results unaffected (their logical_line_idx may shift by 1 — see §10) |

---

## 8.  Implementation phases

Each step independent and ship-able.  Each step gated by:

- mini bench (`bin/bench-remote.sh`) — 11/11 PASS
- new unit/integration tests for that step
- e2e verify via `bin/install-local.sh` + manual smoke

### Phase A — Persistent scrollback

| step | scope | gates |
|---|---|---|
| A1 | `Scrollback::File` variant in `marspot-term`: file header read/write, idx layout, hot-path append.  No reader integration yet. | bench, unit tests for append + roundtrip |
| A2 | Reader path: `cell_at`/`scrollback_line` consult RAM ring → mmap → seek+read tiers; LRU page cache | bench, soak: random reads across deep history stay < 100 µs p99 |
| A3 | L3 main wiring: open file on cold start + execv resume; populate RAM ring; replace `Scrollback::Disk` default with `Scrollback::File` | install-local cycle, scrollback survives manually |
| A4 | snapshot v2 scrollback section demoted to backstop (256 lines) | unit tests pass; install-local with prior snapshot still applies cleanly |
| A5 | Eviction: background head-truncate when `.bin` > 200 MB; sessions-dir reaper for closed panes | soak test: 10 k-line-per-second injection for 30 min stays bounded |

### Phase B — Search engine (L3-side)

| step | scope | gates |
|---|---|---|
| B1 | Pure-Rust search algorithm in `marspot-term`: literal substring, case toggle, wrap-aware logical lines, snippet extraction | unit tests: ASCII, CJK, wrap, multiple hits per line, no-match |
| B2 | Wire types: `SearchScrollback`, `SearchResults`, `SearchMore`, `SearchCancel`.  bincode encode/decode + forward-compat skip | unit tests for each frame; old reader silently skips |
| B3 | L3 main handler: spawn iterator on `SearchScrollback`, stream chunks, honour `SearchCancel`, GC stale query_ids | integration test: 1 MB scrollback returns first 64 hits < 50 ms |
| B4 | Live-grid search merge: L3 scans visible grid in addition to scrollback file | integration test: a query matching only live content still returns hits |

### Phase C — Search UI (L2-side)

| step | scope | gates |
|---|---|---|
| C1 | `PaneTool` trait + `ToolSlot` enum + layout integration (FIXED slots shrink grid, OVERLAY draws on top) | render-metal tests for layout math; no slot occupied = same layout as today |
| C2 | Search bar tool: input box + case toggle + counter; key routing while active | manual e2e: type query, see counter live-update |
| C3 | Result list tool: virtual list with bidirectional load; click-to-jump | manual e2e: scroll list past window edge, more results load; click a hit, grid jumps |
| C4 | Highlight overlay: render-time intercept on focused pane for active match; wrap-aware multi-row span | unit test: highlight rect matches a known multi-row hit's span; visual e2e |
| C5 | Keybindings + Esc-to-close + Cmd-G nav | manual e2e: full search-and-navigate flow |

### Phase D — Forward-looking (not v1)

| step | scope |
|---|---|
| D1 | Fuzzy / regex search behind the `.*` toggle |
| D2 | Sidecar text-only mirror for sub-50 ms search on 100 MB+ files |
| D3 | Optional inverted index for very-long-running shells |
| D4 | cc plugin reuses search engine for conversation-aware navigation |
| D5 | Cross-pane search ("search all panes" command palette) |

---

## 9.  Performance budgets (target, will be re-measured on mini)

| operation | budget |
|---|---|
| Phase A: push_line into File scrollback | < 300 ns / line |
| Phase A: cell_at(deep) — cold mmap page | < 50 µs / page (one-time fault) |
| Phase A: cell_at(RAM ring) | < 50 ns |
| Phase B: search 1 MB scrollback, first 64 hits | < 50 ms |
| Phase B: search 10 MB scrollback, first 64 hits | < 100 ms |
| Phase B: search 100 MB scrollback, first 64 hits | < 500 ms |
| Phase C: search bar key → re-search dispatched | < 16 ms (1 frame at 60 Hz) |
| Phase C: result list scroll → next 64 loaded | < 100 ms p99 |
| Phase C: highlight overlay render | < 100 µs added to render p99 |

Hot-path budget for daily perf (no search active):

| operation | budget |
|---|---|
| L3 push_line (parse → grid → scrollback file) | < 5 µs |
| L2 render p99 (no search active) | < 1200 µs (current bench floor) |
| Idle CPU (no PTY output) | ~0 % |

If any budget regresses on mini, the step that broke it does not
merge until fixed.

---

## 10.  Open questions

### Q1.  How big is the scrollback file in practice?

Need empirical numbers from real claudecode-heavy use.  Estimate
based on `~/Library/Caches/marspot/sessions/234/bytelog` ≈ 800 KB
after several hours — corresponds to ~30 k lines, well under any
cap.  100 MB → ~4 M lines = months of heavy use.

### Q2.  Sidecar text mirror — yes or no?

Linear scan of cell-encoded `.bin` at 5 GB/s memcpy is ~20 ms for
100 MB.  Adding a sidecar UTF-8 text mirror doubles disk usage but
search scans at memcpy speed instead of decoded-cell speed (5-10 ×
faster).  Decision: defer until Phase B B3 profiling shows real
queries breaching the 500 ms budget.

### Q3.  Logical line indexing — line_idx stability?

When live grid scrolls a row into scrollback during an active
search, the file's line_idx of every existing entry stays put
(scrollback is append-only; old indices never shift).  Existing
hits keep working.  Eviction at the head DOES shift indices —
mitigation: track an `eviction_epoch` per query; on eviction
mid-search, the iterator restarts from current head and L2 marks
the result list "history was trimmed during search".

### Q4.  Should L2 cache search results?

If user closes search bar then re-opens with same query, do we
re-run from scratch?  Yes for v1.  Results are quick to regenerate
and stale cache invalidation is its own bug surface.

### Q5.  Search timestamp dimension?

Future: capture per-line wall-clock timestamps in `.bin` (extra
8 bytes / line) so results show "12:34:05" prefix.  Reserve a
flag bit in the file header for this; add in Phase D.

### Q6.  Multi-pane / cross-pane search?

Out of scope for v1.  Each pane searches its own L3.  Reserved
under Phase D.

### Q7.  Should the search bar live in fixed slot instead of overlay?

Two designs trade off:

- **Floating overlay** (current plan): grid doesn't shrink; search
  bar can be moved; can dismiss without layout reflow.
- **Fixed BOTTOM_FIXED**: grid shrinks; permanent visibility;
  more affordance-y.

User prefers floating-by-default per request, with the toolbar
framework existing so a future "pin" toggle could move it to a
fixed slot.

### Q8.  Disk-IO scheduling — fsync ever?

v1: never fsync.  Graceful exit flushes BufWriter only.  Crash
risk is bounded to "last few seconds of scrollback".  If user
reports lost long stretches, revisit (could fsync every N pages
on a timer, or at execv boundary).

### Q9.  cc plugin reuse?

cc plugin (claudecode) sits in L1 and doesn't talk to L3 scrollback
today.  Future: cc could open the same scrollback.bin in RO mode
(or via a shared search wire) to scope queries to a conversation
range.  Out of scope for v1; designed-in via the wire layer being
non-cc-specific.

---

## 11.  Decisions captured

| decision | rationale |
|---|---|
| per-session file, NOT shared | sessions are independent contracts; cross-session interactions out of scope |
| append-only + sidecar idx | O(1) line lookup without scanning; cheap to rebuild idx if corrupt |
| no fsync in hot path | parser is already the source of truth for "did we see this byte"; disk lag is acceptable |
| linear scan in v1 | simple, fast enough for expected sizes, easy to evolve |
| streaming results with query_id | bounded memory; cancellable; matches a typed-query UX |
| wrap-aware via logical-line grouping | mandatory — single-row matching would split URLs and feel broken |
| floating overlay UI default | matches Chrome / VS Code muscle memory; preserves grid layout |
| ToolSlot framework | search is first; future tools plug in without bespoke layout code |
| basic level (not cc) | search is a terminal capability, not a conversation-tool capability |
| Cmd+F not Ctrl+F | macOS convention; the user's "Ctrl+F" was shorthand for "the search shortcut" |

---

## 12.  Glossary

- **Logical line** — sequence of one or more physical rows where
  rows 2..N have `wrapped == true` (DECAWM continuation).
  Searched as a single string.
- **Physical row** — a single grid row that the renderer paints.
- **Scrollback** — content that has scrolled above the live grid.
- **Live grid** — the visible bottom rows the PTY writes into.
- **logical_line_idx** — index of a logical line in the scrollback
  file's append order (0 = oldest live).
- **view_offset** — rows up from live tail in the publish window
  (0 = live tail, larger = older).
- **wrapped flag** — per-row bool indicating "this row started as
  an autowrap continuation of the row above".  Distinct from "hard
  newline".
- **PaneTool** — render-and-input-routed widget plugged into a
  ToolSlot.  Search bar is one; future status pills are others.

---

## 13.  Related docs / memory

- [`docs/architecture.md`](./architecture.md) — current layered model
- [`docs/per-session-l3.md`](./per-session-l3.md) — L3 process model + shm wire
- [`docs/rfc-003-l3-pty.md`](./rfc-003-l3-pty.md) — wire amendments
- [`docs/silent-update.md`](./silent-update.md) — execv contract for L3
- memory `project-scrollback-persistent` — earlier sketch of the
  storage layer, now subsumed by §2 here
- memory `feedback-wire-upgrade-silent-lossless` — protocol rule
  every new msg_type in §4 must follow
- memory `feedback-ceiling-first` — when search perf and feature
  fight, perf wins; design above reflects that

# Per-session L3 processes — design (polish target #4)

> Status: **design** (no code yet). This is the "round of design before
> touching code" the handoff demanded for target #4. It fixes the
> module boundary, the process model, the shared-grid IPC, the
> per-session update flow, and the commit-sized build plan. The
> *direction* was pre-decided by the user (light per-session process,
> **zero Metal growth**, shared grid, idle-restart updates); this doc
> nails down the load-bearing specifics that direction left open.

## Why

Today (post target #5) the process tree is:

```
marspot-shelld (PTYs + bytelog)  ←─ unix socket ─┐
marspot-shell  (window + IOSurface + supervisor) │
   └── marspot-core (ONE process: N terminals + Metal renderer)
```

`marspot-core` (L2) holds **every** session's `Terminal`/`Grid`/parser
*and* the single Metal renderer in one address space. Two consequences:

1. **No per-session version isolation.** A core update swaps all N
   sessions' parser at once (the dual-core swap of target #5). You
   cannot update one idle session's emulator while keeping another's
   long-running process on the old binary.
2. **No parser crash isolation.** A panic in one session's parser
   takes down the renderer and every other session in the same process.

Target #4 splits the **parser/terminal half** of each session into its
own light process (L3), leaving L2 as a pure renderer + input router +
compositor. Per-session updates and crash isolation fall out for free.

### The constraint that shapes everything: zero Metal growth

The user **vetoed** "one full render process per session" — N×Metal
drivers pre-load ≈ 300 MB, which violates marspot's memory floor. So
L3 must be a **terminal-only** process: parser + `Terminal` + `Grid` +
scrollback + PTY/shelld plumbing, and **nothing** that links
Metal/AppKit/CoreText. Target memory: ~3–5 MB resident per L3.

```
marspot-shelld (PTYs + bytelog)
marspot-shell  (window + IOSurface + supervisor)
   └── marspot-core (L2: ONE Metal renderer + compositor + input router)
        ├── marspot-session #1 (L3: parser→grid, NO Metal)   ─┐ shm grid
        ├── marspot-session #2 (L3: parser→grid, NO Metal)   ─┤ + ctl pipe
        └── marspot-session #N (L3: parser→grid, NO Metal)   ─┘
```

Memory: `1 Metal renderer (L2) + N × ~4 MB (L3, no Metal)` ≈ today's
single core + `N × ~4 MB`. For 9 sessions ≈ +36 MB — the price of
isolation, two orders of magnitude under the 300 MB the heavy-process
route would have cost. This is the accepted ceiling.

## Prerequisite (the *real* step 1): extract a zero-GUI core crate

The handoff said "first step: write the `marspot-session` skeleton."
That under-specified the hard part: a bin **inside the `marspot`
package** cannot avoid the GUI frameworks. The `marspot` lib depends on
`objc2-app-kit`/`objc2-metal`/`core-text`/… and carries `#[link(...
kind="framework")]` attributes; every binary that links the lib gets
those frameworks in its load commands, so dyld loads the Metal driver
at launch. Cargo features can't help: all bins in one package share a
single lib compilation (the union of features).

**Therefore L3 being light *requires* splitting the pure terminal core
into its own workspace crate with zero GUI deps.** This is also the
"孵石头" the project already planned (CLAUDE.md extraction backlog).

### Crate boundary (from a full module audit)

`marspot-term` — new workspace crate, **deps: `libc` only.** 15 modules:

| Pure module | Role |
|---|---|
| `parser` | VT/xterm escape state machine |
| `grid` | `Cell`/`CellAttrs`/`Color`/`Grid` (20-byte POD cell) |
| `scrollback` | memory + anon-mmap-ring scrollback |
| `terminal` | VT emulator (`feed` → grid), modes, local-echo predict |
| `pty` | `forkpty`/ioctl PTY wrapper |
| `session` | PTY + Terminal container + reader thread |
| `shelld_proto` | shelld wire protocol |
| `shelld_client` | shelld unix-socket client |
| `shell_proto` | shell↔core wire protocol (uses input value types) |
| `paths` | state-dir resolution (keys off `MARSPOT_STATE_DIR`) |
| `tmux` | tmux control-mode parser |
| `layout` | pane/cell/sidebar geometry (pure math) |
| `updater` | release poller (curl/openssl shell-out) |
| `render` | data types only: `SessionView`, box-drawing masks |
| `input_core` | **NEW**: `MarspotKeyEvent`/`key_event_to_bytes` — the |
|              | pure half split out of `input.rs` |

The only cut needed: `input.rs` splits into `input_core` (pure
key→bytes, ~180 lines + all its tests) which moves to the crate, and
the clipboard half (`read/write_clipboard_text`, the only `NSPasteboard`
touch) stays in `marspot`.

Stays GUI (`marspot` package): `app`, `font_cache`, `glyph_atlas`,
`iosurface`, `render_metal`, `input` (clipboard + re-export of
`input_core`), `pane` (facade), and the bins.

`marspot` lib does `pub use marspot_term::*;` at crate root, so existing
`marspot::grid::Grid` / `marspot::terminal::Terminal` paths keep
resolving — minimal churn in the bins. No reverse deps exist
(`grid`↛`render`, `terminal`↛`input`, verified), so the carve is clean.

> Crate name: **`marspot-term`** chosen deliberately — `marspot-core`
> is already the L2 *renderer* binary; naming the terminal crate
> `-core` would collide head-on. `-term` reads as "the terminal
> engine," which is what it is.

## Process model

### Who owns L3

**L2 (`marspot-core`) spawns and supervises the L3 processes.** L2
already owns the session set (new/close/layout/focus) and will own the
per-session update UI (refresh icon). shelld stays a dumb PTY
multiplexer. So L2 gains a small per-session supervisor — analogous to
shell's supervisor over core, one level down.

Each L3:
- is spawned by L2 with an inherited control socketpair (fd 3, exactly
  the shell→core pattern) **and** an inherited shm fd (fd 4) for its
  grid framebuffer;
- connects to shelld itself and attaches/creates **one** session
  (bytelog replays into its `Terminal` on attach);
- pumps PTY bytes → `Terminal` → `Grid`, then publishes the in-view
  window into the shared framebuffer and pokes L2.

### Shared grid: POSIX shm, published-snapshot + viewport

**Primitive: POSIX shared memory** (`shm_open`+`ftruncate`+`mmap
MAP_SHARED`), fd passed by inheritance then `shm_unlink`'d so no name
lingers — same spirit as the IOSurface-by-id and fd-3 patterns already
in the tree. *Not* IOSurface: IOSurface is a GPU-texture buffer; grid
cells are plain CPU POD. shm is the orthodox primitive for a shared
byte buffer.

**Model: published snapshot, not shared backing.** L3 keeps its `Grid`
private (so `grid`/`terminal` stay pure, hot-path-clean, miri-tested).
After each pump it `memcpy`s the **currently-in-view window** of cells
into the shm framebuffer and bumps a sequence counter. L2 reads the
framebuffer to render. The window is `rows × cols × 20 B` ≈ 40 KiB —
negligible, and it means L2 never needs L3's scrollback resident.

shm region layout:

```
[ header (cache-line) ]
   u32 magic, u32 version
   u32 cols, u32 rows
   u32 cursor_col, u32 cursor_row
   u32 flags         (cursor_visible | app_cursor_keys | bracketed_paste | …)
   u64 seq           (even = stable, odd = mid-write; L2 reads between evens)
   u64 scroll_push_count
   u32 scrollback_len
   u32 view_offset    (echo of the offset this frame was rendered at)
[ cells: rows*cols * sizeof(Cell)=20 ]   ← the in-view window
```

Tear-free read: seqlock. L3 writes `seq|1`, fills cells+header, writes
`seq+1`. L2 reads seq (retry if odd), copies, re-reads seq (retry if
changed). No locks across the process boundary.

### L2 ↔ L3 control channel (fd 3 socketpair, reuses `shell_proto` framing)

L2 → L3:
- `KeyEvent(event, mods)` — L2 forwards the *raw* key event; **L3**
  encodes via `key_event_to_bytes` (it has the terminal modes locally),
  writes the PTY, runs local-echo prediction, republishes. Keeps all
  terminal-coupled logic in L3; L2 stays terminal-agnostic.
- `Scroll(view_offset)` — L2 computes the target offset from wheel
  deltas; L3 publishes that window (live or scrollback rows).
- `GridResize(cols, rows)` — L2 computes per-cell dims from layout; L3
  resizes Terminal + ioctl PTY (via shelld) + reflows + republishes.
  **Resize is in-place, no fd hand-off:** the shm region is mapped once
  at create to a capacity cap (`grid_shm::MAX_CELLS`, ≈5 MiB virtual,
  lazily faulted so only the live dims are resident). The published dims
  ride in the header with every frame; the writer publishes the new
  `cols × rows` in place and the reader reads the dims per-frame, so a
  resize within the cap needs neither a remap nor passing a new fd over
  the control socket. A resize *past* the cap would be the only case
  needing a re-create — it can't happen for a single on-screen pane.
  (This resolves the handoff's in-place-vs-respawn fork toward in-place,
  via a documented growth bound — the project's standard discipline.)
- `GetSelectionText(anchor, focus, blockwise)` — rare (Cmd-C); L3
  walks its grid+scrollback and returns the string. Keeps scrollback
  out of L2.
- `Focus`, `Close`.

L3 → L2:
- `GridReady(seq)` — "new frame published," pokes L2's event loop
  (event-driven, idle CPU stays 0).
- `CaretRect` — focused-cell caret for IME anchoring (already a frame).
- `Exited` — session ended; L2 drops the pane.

**Latency note (must measure):** local echo now crosses one extra IPC
hop (L2 forwards key → L3 predicts+publishes → pokes L2 → L2 renders)
versus today's in-process predict. On a localhost socketpair this is
tens of µs, but it touches the latency tail the project gates on. The
build plan measures keystroke→paint against `bench/baseline.json`
before the new path replaces the old.

### Selection / scroll ownership

Selection state (anchor/focus/mode) and `view_offset` **stay in L2**
(they're driven by mouse hit-testing against L2's layout). L2 projects
selection onto the published window for highlight; for *text* it asks
L3. `view_offset` is L2-owned but pushed to L3 each scroll so L3 knows
which window to publish.

## Per-session silent update

shelld already supports **multiple subscribers per session + full
bytelog replay on attach + survival across shelld restart** — exactly
the primitives this needs.

- **Idle session:** L2 spawns a *new* L3 on the new binary; it attaches
  the same shelld session, replays the bytelog into a fresh grid at the
  current cols/rows, and publishes into a *second* shm buffer. Once its
  `GridReady` lands and its frame matches, L2 atomically swaps the
  pane's shm pointer to the new L3 and kills the old L3. Invisible —
  this is the dual-core swap of target #5, scoped per session and one
  layer down.
- **Active (focused) session:** don't auto-swap (replay could blip a
  TUI mid-keystroke). Show a refresh affixed to the cell title's right
  edge; clicking it triggers the same swap on demand.
- **Crash isolation:** an L3 panic ends one session; L2 logs it, drops
  or relaunches that pane, and the window + other sessions are
  untouched.

## Incremental build plan (each step commits; ceiling-guarded)

Every step ends green on `bin/test-all.sh` + the lib suite, and the
shm/process steps add a soak that asserts L3 RSS stays bounded and idle
CPU ≈ 0.

0. **Extract `marspot-term` crate.** Workspace `Cargo.toml`; move the
   15 pure modules; split `input` → `input_core`; `marspot` lib
   `pub use marspot_term::*`. Build + 196 lib tests + miri (4 modules
   now live in the crate) + bench must be unchanged. **Pure mechanical
   refactor, no behavior change — the riskiest-to-review but
   safest-to-reason step; do it first and in isolation.**
1. **`marspot-session` skeleton.** New bin in… (TBD: its own crate or
   `marspot-term`'s bin) depending only on `marspot-term`. Boots,
   attaches one shelld session, pumps bytes → Terminal, prints grid
   dims periodically. Assert: links no Metal/AppKit (`otool -L` has no
   Metal.framework), RSS ~3–5 MB.
2. **shm publish.** L3 creates/inherits the shm framebuffer, publishes
   the in-view window with the seqlock after each pump. A throwaway
   reader verifies tear-free reads.
3. **L2 reads one L3.** Behind a flag (`MARSPOT_L3=1`), L2 spawns one
   L3, mmaps its shm, renders the published window for one pane,
   forwards input over fd 3. Old in-process path stays default.
4. **N sessions + lifecycle.** L2 spawns one L3 per pane; new/close/
   resize/scroll/focus wired through; selection-text request path.
   Measure latency tail vs baseline.
5. **Per-session update + refresh icon + crash isolation.** Idle swap,
   active on-click swap, panic→drop. Soak: repeated per-session swaps,
   RSS bounded, steady-state N L3 + 1 L2.
6. **Flip default**, retire the in-process multi-grid path in core,
   re-lock bench baseline.

## Decisions (confirmed 2026-06-13)

The forks the handoff's direction left open, now settled:

1. **Crate name** → `marspot-term` (avoids the `marspot-core`-bin
   collision). ✓ confirmed.
2. **L3 owner** → L2 spawns/supervises (L2 already owns sessions +
   update UI; shelld stays dumb). ✓ proceed.
3. **Shared-grid primitive** → POSIX shm + fd inheritance (orthodox for
   CPU byte buffers; not IOSurface). ✓ proceed.
4. **Grid crossing** → **published snapshot + viewport request** (keeps
   `grid`/`terminal` pure; L2 never holds scrollback). ✓ confirmed,
   deliberately diverging from the handoff's literal "shared mmap
   ring" wording — the snapshot keeps the hot-path modules pure and
   miri-able, at the cost of a ~40 KiB/session memcpy per frame and an
   L3 round-trip for Cmd-C / scrollback text.

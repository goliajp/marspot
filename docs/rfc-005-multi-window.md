# RFC-005 — Multi-window

Status: ACCEPTED 2026-07-26.  Execution in progress (step 0 shipped
as core 0.12.37).

## Goal

N native windows, each carrying its own pane grid; panes movable
between windows by drag (and menu, same step).  The uniform model is
**window → pane, always** — a single pane is the `windows.len() == 1`,
`panes.len() == 1` degenerate case, not a special one.  Everything
per-pane (history/scrollback, snapshot, PTY, search, scroll offset,
badges) stays per-pane and must be provably untouched by window
membership.

## Architecture decision: one core, N surfaces

One L2 process holds `Vec<WindowState>`; L1 owns N NSWindows and one
IOSurface pair per window.  The rejected alternative (one L2 process
per window) loses on every hard budget this project has:

| criterion | core-per-window | one core, N surfaces |
|---|---|---|
| RSS | atlas/font/device duplicated ×N | shared; increment = layout + render target |
| idle CPU | N event loops + N supervision wires | one loop, still 0% |
| silent update | N pending cores, N swaps | one dual-core swap flips all windows |
| pane move | cross-process socket/shm re-handoff per drag | in-process `Vec` move; L3 never notices |
| crash blast | window-isolated | all windows — same class as today's all-panes; loop-never-blocks + LoopWatch already own this risk |

Pane-level crash isolation stays where it always was: L3, one process
per pane.

The pyramid invariant is unchanged: L1 holds the window-shaped
resources (NSWindows, surface pairs, presenters), L2 holds the pane
registry + control sockets, L3 holds PTY/scrollback/shell.  L1 still
supervises exactly one core.

## Data model (L2)

```
CoreApp {
    windows: Vec<WindowState>,   // always >= 1
    key_window: usize,           // L1-reported key window
    // stays global: sid-keyed maps (pane_badges/titles/cwds/
    // pane_sessions/last_cwd_refresh/reconnecting/cwd_unresolvable),
    // event_tx, reconnect bookkeeping, renderer shared layer,
    // cc_usage_modal, all_exited
}
WindowState {
    window_id: u32,              // L1-allocated, monotonic
    surfaces + render target,    // per-window
    layout, panes: Vec<Pane>, focused_idx,
    grid_cols/rows, w_phys/h_phys/scale,
    selection, selection_dragging, editing_title, title_edit_buffer,
    sidebar_collapsed, context_menu, layout_modal, process_panel,
    ime_preedit, last_caret_sent, needs_render, first-frame gate
}
```

`Pane` is already fully self-contained (backend, sid, control socket,
scroll/search/title state — step 0 moved `custom_title` in).  All
remaining per-pane L2 state is sid-keyed and window-agnostic.

## Renderer split (step 3)

`MetalRenderer` today couples shared resources with the one window's
target.  Split:

* **Shared, keyed by backingScaleFactor**: FontCache, ShapeCache,
  GlyphAtlas + color atlas.  Same-scale windows share one set; each
  distinct scale gets its own (glyphs are rasterised at physical px,
  so cross-scale sharing would be wrong, not just wasteful).  RSS
  bound = number of distinct display scales, not number of windows.
* **Shared, single**: MTLDevice, command queue, pipelines.
* **Per-window**: drawable textures, dimensions, instance buffers.

## Wire changes (step 2) — silent + lossless, per standing rule

* `window_id` added to: `SurfaceAttach`, input frames (KeyEvent /
  Mouse* / Scroll / Preedit / FileDrop), `CaretRect`.  Old readers
  skip the extra field; missing field decodes as window 0.
* New L1→L2 frame `WindowClosed(window_id)`.
* A `SurfaceAttach` carrying an unseen `window_id` **is** the window
  birth event — no separate create frame.
* `SurfaceReady` / `FrameRendered` unchanged: surface ids are global,
  L1 resolves window by looking up which pair contains the id.
* Boot handshake migrates from `ENV_SURFACE_ID*` to per-window
  `SurfaceAttach` frames sent after core spawn.  The env path is kept
  one release for version-skew tolerance, then removed.
* Dual-core swap: L1 sends the pending core one `SurfaceAttach` per
  window and requires all N `SurfaceReady` acks before `UPDATE_SWAP`.
  N = 1 reproduces today's sequence exactly.

## Semantics (all decided up front — no mid-flight decisions)

| topic | ruling |
|---|---|
| Cmd-N | new window, 1×1 grid, one fresh pane; becomes key window |
| Cmd-W / red button | closes THAT window and all its panes (`close_session` each; retired/ recycle bin is the safety net, no confirm dialog) |
| Cmd-Q | quits the app; every window and its panes persist and are restored on next launch (today's close-and-resurrect contract, inherited window-wide) |
| last pane moved out | empty window auto-closes |
| move-in landing | appended to target window's pane list (sidebar overflow if grid full); focus follows the pane; target becomes key |
| pane move mechanics | whole `Pane` moved between `WindowState.panes` vecs; source window clears `selection`/`editing_title` if they referenced it; both windows `rebuild_layout` + save |
| drag AND context menu | land in the same step — drag is the ceiling interaction, the menu (Move to Window N / New Window) is the macOS-conventional twin, not a predecessor |
| SESSION_COUNT_HARD_CAP | per window (it is a sidebar/UI capacity) |
| orphan adoption at boot | key (first) window |
| per-window UI | sidebar, process panel, selection, IME preedit, layout modal, context menu |
| global UI | cc usage modal, dev panel — open on key window; cc plugin is sid-keyed and needs zero change |
| window_id | allocated by L1, monotonic u32; survives core swap via SurfaceAttach replay |
| standalone bin (`src/main.rs`) | adopts the same WindowState structure (len == 1) so lib/tests stay parallel; grows no window-opening UI — it is the bench/dev harness, not the product surface |

## Persistence (step 6)

* `shell-state.bin` v2: `windows: Vec<{ frame, display_id, grid_cols,
  grid_rows, focused_idx, panes: Vec<SavedPane> }>` + `key_window`.
  v1 files parse into a single window (fail-soft versioning already
  in place).  Writer emits v2 only.
* `window-state.bin` becomes a list (frame per window).
* `MARSPOT_RESTORE_FRAME` (L1 execv handoff) carries N frames.
* `sessions/<id>/` registry stays window-blind — window membership
  lives only in the layout file, exactly like pane order does today.

## L1 (step 4)

`AppState`/`MarspotAppCtx`/`MarspotView`/delegate collections keyed
by window; one `SurfacePair` + `ShellPresenter` + first-frame gate
per window.  The dev window (`dev_window.rs`) is the proven template,
including its hard-won rule: **all AppKit mutations from dispatch
context go through the deferred-action queue** (`makeKeyAndOrderFront:`
/ `setFrame:display:` synchronously re-enter the delegate and
double-borrow the state cell otherwise).  Input handlers tag frames
with the window_id of the NSWindow that received them; key-window
changes are reported to L2.

## Execution plan (each step gates + installs independently)

0. `basic` custom_title → Pane field (**shipped**, core 0.12.37)
1. `basic` CoreApp → WindowState extraction; `windows.len() == 1`;
   zero behavior change; full nextest + bench gate green
2. `infra` wire window_id + WindowClosed + boot handshake env→frames
3. `basic` renderer split (shared-by-scale / per-window target);
   still single-window; bench gate locks perf
4. `basic` L1 window collections + Cmd-N + close semantics +
   per-window presenter + input tagging
5. `basic` pane move: drag + context menu, same commit series
6. `infra` persistence v2 + per-window boot assembly
7. E2E: two windows through install-local UPDATE_SWAP; L1 execv
   restoring N frames; L3 execv untouched

Gates per step: mini `cargo nextest run` (all targets, 780+), build
0 warnings, `bin/bench-remote.sh` on perf-adjacent steps (3 above
all), `install-local.sh` live verification.  Idle CPU stays 0% —
render remains poke-driven per window; no timers introduced anywhere.

## Non-goals

* Tabs (macOS native tabbing or otherwise) — separate feature, not
  started here.
* Cross-window tmux-CC integration changes — tmux mode keeps its
  current single-window behavior.
* Per-window font size / theme — fonts stay global (single config).

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

## Architecture decision: windows are peers

There is no main window.  L1 owns N windows of equal standing; L2
holds one `WindowState` per window and no notion of a privileged one.
`key_window` means exactly one thing — which window currently has
keyboard focus — and nothing may read it to mean "the real window".

This was decided after step 4c, when the second window turned out not
to work despite every piece of it being written.  The reason was that
"one window" had been encoded structurally in three places that the
`Vec<WindowState>` refactor did not reach:

* the IOSurface pair, its textures and the write cursor were locals in
  `main()` — the second window's attach overwrote them, and the first
  window's presenter went on sampling a surface nobody painted;
* `pump_all` / `process_search_debounces` walked `win!(self)` only, so
  a pane in an unfocused window received no PTY bytes at all;
* `save_session_state` persisted only the key window's panes, and the
  exit condition asked only the key window whether its panes had died.

None of those are "the second window is a bit rough".  Each is the
single-window assumption re-appearing as a data structure.  The rule
that falls out, and that every later step is checked against:

> Any state that describes a window lives in `WindowState`.  Any loop
> that acts on windows iterates all of them.  Any file that records
> windows records a list.  `key_window` is readable only where the
> question genuinely is "where is the keyboard".

Step 4d (paint + pump) and step 6a (persistence) implement it;
step 4e finishes it on the input path, where `win!(self)` still stands
in for "the window this event is about".

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
    windows: Vec<WindowState>,   // always >= 1; peers
    key_window: usize,           // keyboard focus ONLY
    // stays global: sid-keyed maps (pane_badges/titles/cwds/
    // pane_sessions/last_cwd_refresh/reconnecting/cwd_unresolvable),
    // event_tx, reconnect bookkeeping, renderer shared layer,
    // cc_usage_modal, all_exited
}
WindowState {
    window_id: u32,              // L1-allocated, monotonic
    surfaces: Option<WindowSurfaces>,  // pair + textures + write cursor
    last_render_at,              // per-window frame-interval cap
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

The split turned out to be far smaller than this RFC first assumed,
because the render target texture is **already** a parameter of
`render_layout_to_texture` — the hard part was done.  What is left on
the renderer that cannot be shared is exactly two things, and they now
live in `render_metal::WindowRender`:

* `pane_caches` — indexed by a window's pane order, so window A's slot
  0 and window B's slot 0 are different panes.
* `clear_bg_required` — "does *this* window still owe a full clear";
  another window's resize must not discharge it.  Pinned by
  `window_state_tests::each_window_owns_its_clear_flag`.

Everything else is genuinely shared, which is the RSS win: MTLDevice,
command queue, every pipeline, the FontCache + ShapeCache, both glyph
atlases, and every scratch buffer (a scratch buffer is only live
inside one `render_*` call, and windows render one after another).

**No per-scale atlas.**  This RFC originally called for atlases keyed
by `backingScaleFactor`, on the assumption that glyphs are rasterised
at each display's physical pixel size.  Reading the code refuted it:
`FontCache::build` rasterises at a fixed `FONT_POINT`, the renderer's
`scale` argument is used only for `layer.setContentsScale`, and
`GlyphKey` already carries a quantised size — so one atlas serves
windows on displays of any backing scale, with no thrash and no
duplication.

## Wire changes (step 2) — silent + lossless, per standing rule

**PROTO_VERSION stays at 2 — deliberately.**  Implementation found
that the shell *kills* a core whose `HelloAck` version differs from
its own, so bumping the version is itself the hazardous act: a
core-only update would put a v3 shell against a v2 core and
respawn-loop it.  The window id therefore rides along in ways a peer
that has never heard of it cannot notice.

* `window_id` is **appended** to the pointer frames (Mouse* / Scroll /
  FileDrop), `Preedit`, `KeyEvent` and `CaretRect`.  Every one of those
  decoders already reads `payload.len() < N` and ignores a longer
  tail, so an old reader is unaffected and a new reader falls back to
  `FIRST_WINDOW_ID` when the tail is absent.  Pinned in both
  directions by `shell_proto` tests built from hand-written legacy
  payloads.
* `SurfaceAttach` could **not** grow — its decoder demands exactly 32
  bytes, so appending would hard-fail every installed core.  The
  window-aware form is a new msg type, `SurfaceAttachWindow`, which
  old readers silently skip.  The shell sends both for the first
  window; the core ignores the legacy frame from the first
  window-aware one onward, so neither peer attaches twice.
* New L1→L2 frames: `WindowClosed(window_id)`, `WindowFocus(window_id)`.
* A `SurfaceAttachWindow` carrying an unseen `window_id` **is** the
  window birth event — no separate create frame.
* `SurfaceReady` / `FrameRendered` unchanged: surface ids are global,
  L1 resolves window by looking up which pair contains the id.
* **The env handshake stays.**  RFC-005 originally called for
  migrating `ENV_SURFACE_ID*` to frames; implementation showed that is
  strictly worse.  Env hands the first window a surface at spawn, so
  first paint has nothing to wait for, and additional windows arrive
  by frame regardless — the env path does not need to scale.  Keeping
  it also leaves the most delicate path in the system (boot) untouched.
* Dual-core swap: L1 sends the pending core one attach per window and
  requires all N `SurfaceReady` acks before `UPDATE_SWAP`.  N = 1
  reproduces today's sequence exactly.

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

**6a — format (shipped).**

* `shell-state.bin` v2: `windows: Vec<{ grid_cols, grid_rows,
  focused_idx, panes: Vec<SavedPane> }>` + `key_window`.  v1 files
  parse into a single window (fail-soft versioning already in place).
  Writer emits v2 only.  Geometry is *not* here — see below.
* `window-state.bin` v2: a list of frames in window creation order,
  written whole on every change by L1.  Entry *i* pairs with entry *i*
  of the layout file; that pairing is the only coupling between the
  two files, and both are ordered by window creation.
* Keeping frames out of `shell-state.bin` is deliberate and unchanged
  from F3+6.1: L1 owns AppKit geometry, L2 owns panes, one writer per
  file, no cross-process atomic-rename race.
* `sessions/<id>/` registry stays window-blind — window membership
  lives only in the layout file, exactly like pane order does today.

**6b — restore (shipped).**  L2 boots window 0 from `windows[0]` and
keeps `windows[1..]` queued.  For each queued record it sends
`WindowOpenRequest(frame_index)` (msg type 64 — a new type, which old
readers skip, so still no PROTO bump).  L1 opens the window at entry
`frame_index` of `window-state.bin`; the resulting
`SurfaceAttachWindow` reaches `adopt_window`, which pops the front
record and restores from it instead of spawning one fresh pane.  The
queue is also what tells the two cases apart — no extra flag.

**The assembly runs off-loop.**  Reattaching an L3 blocks on a UDS
handshake with its own deadline, and this RFC's own rule is that
opening a window must not freeze the windows already up.  So the
window appears at once with its saved grid and one "starting…"
placeholder per saved slot, a worker thread runs
`assemble_panes_at_boot`, and `WindowRestoreFinished` swaps the real
panes in.

Two consequences that are load-bearing:

* **A restored assembly must not sweep the registry.**  The sweep —
  adopting live sessions no slot claimed, retiring unclaimed dirs — is
  global, and a restored window knows only its own saved sids.  Left
  on, it would adopt the boot window's panes a second time and retire
  the dirs of every session it never heard of.  Exactly one assembly
  per boot sweeps: the boot window's (`sweeps_registry`).
* **Off-loop results must find their pane in any window.**
  `L3SpawnFinished`, `L3ControlReconnected` and `SearchResults` all
  looked their pane up in the key window only, so a result landing
  while the user had focused a different window was silently dropped.
  They go through `find_pane_by_sid`, which searches every window.

`MARSPOT_RESTORE_FRAME` (L1 execv handoff) still carries one frame,
the boot window's, and does not need to grow: the restored windows
read their geometry from `window-state.bin` like any other launch.

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
2. `infra` wire window_id + window lifecycle frames (**shipped**)
3. `basic` renderer split (`WindowRender` per window, everything
   else shared) (**shipped**)
4. `basic` L1 window collections + Cmd-N + close semantics +
   per-window presenter + input tagging (**4a–4c shipped**)
4d. `basic` L2 paint + pump go peer: per-window surfaces / textures /
   write cursor / frame cap, attach routed by `window_id`, render pass
   over all dirty windows, pump + search debounce over all windows,
   exit only when every window is dead (**shipped**, core 0.12.43)
6a. `infra` persistence v2: both files hold window lists
   (**shipped**, core 0.12.44 / shell 0.7.14)
6b. `infra` restore N windows at boot, assembly off-loop (**shipped**)
4e. `basic` input goes peer: `win!(self)` on the mouse / key / scroll /
   drag / preedit paths takes the window the event names.  Scroll must
   land in the window under the cursor even when it is not key; drag
   and release belong to the window that took the press, not to
   whichever window became key mid-drag.  The `self.key_window`
   arguments left behind by 4d are the work list.
    Then: re-enable Cmd-N (and press it once in the sandbox before
   installing).
5. `basic` pane move: drag + context menu, same commit series
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

# RFC-006 — Pane drag & drop, ceiling UX

Status: DRAFT 2026-07-29.  Successor to RFC-005 step 5, which shipped
the mechanism (cross-window move by menu and by drag).  This RFC ships
the *experience*: split placement with a live preview, dormant
placeholders on the source side, and drag-to-desktop.  Field driver:
"window2 是 1×1 时拖一个 pane 过去就看不见了"— today's landing rule
(append; overflow to sidebar) is correct for the menu and wrong for a
drag, because a drag points at a *place*, and the place must be
honoured.

## 1. Interaction model

One drag, four endings.  All four begin identically: press a pane's
title, travel past the 10 px slop, the title grows the ⇢ marker.

| pointer at release | result |
|---|---|
| over a pane's **edge band** in any marspot window | the dragged pane **splits into that side** of the hovered pane (grid reshapes, §2) |
| over a pane's **center** in another window | **swap**: dragged pane takes the hovered pane's slot, hovered pane goes to the dragged pane's old slot (cross-window exchange in one gesture) |
| over a pane's **center** in its own window | swap within the window (slot exchange, same as the layout modal's drag but without opening it) |
| **outside every marspot window** | a **new 1×1 window** opens with the pane, centred on the pointer (§4) |

Zones of a hovered pane's rect: the four edge bands are each 25 % of
the rect's width (left/right) or height (top/bottom), clamped to at
least 48 physical px so slim panes keep usable bands; the remainder is
the center.  Corners resolve to the axis with the deeper penetration.

**Live preview** (the "幽灵"): while the drag is active, the window
under the pointer draws a translucent accent rect over the region the
pane would occupy on release — the half of the hovered pane for an
edge drop, the whole hovered pane for a swap; the source pane dims.
No preview → release cancels (that is also the reading rule: what you
see highlighted is exactly what release will do).  Esc during a drag
cancels it outright.

## 2. Split placement — grid semantics

marspot's layout is a cols×rows card grid (row-major `panes` Vec +
`card_slots` permutation), not a tmux split tree.  This RFC stays
inside that model — split placement is a *grid reshape + slot
insertion*, chosen so the flagship cases are exact and the general
case is predictable:

* **Hover pane at (r, c), drop on left/right band**
  - if some column can absorb it (`panes.len() < cols×rows` after
    reshape rules below): insert the dragged pane at row-major index
    `r×cols + c` (left) or `r×cols + c + 1` (right); later panes
    shift by one slot.
  - if the grid is full: **cols += 1** (cap 6), then insert as above.
* **Hover pane at (r, c), drop on top/bottom band**
  - if full: **rows += 1** (cap 6), then insert at
    `r×cols + c` (top) or `(r+1)×cols + c` (bottom).
* **Both dimensions at cap (6×6)**: no reshape; the drop downgrades
  to append + sidebar overflow (today's rule), and the preview shows
  a sidebar highlight instead of a split rect so the downgrade is
  visible *before* release.
* The flagship case falls out exactly: 1×1 window, drop on the right
  band → 1×2 side-by-side; drop on the bottom band → 2×1 stacked.

Row-major insertion in a wider grid is an approximation of "goes
right there" (a mid-grid insert shifts later panes across row
boundaries).  That is accepted: the preview shows the true landing
slot, and the layout modal remains the tool for wholesale
rearrangement.  A drag is for *this pane, that place*.

Swap never reshapes anything: it is a pure two-slot exchange, which
is why it is the center-zone default — the most common "I want these
two the other way around" costs one gesture and disturbs nothing.

## 3. The source side — dormant placeholders

Today `remove(idx)` compacts the Vec: later panes slide back one
slot.  For a *close* that is right.  For a *move-out* it is wrong —
the user is arranging space, and the arrangement they left behind
should hold still.  New rule:

* A pane moved out leaves a **dormant placeholder** in its slot: the
  grid does not reshape, nothing shifts, nothing is spawned.
* A dormant slot renders as a recessed empty card — background
  slightly darker than a live pane, centred hint `empty — click to
  start a shell` in the muted ink.  It is not focusable by
  keyboard-next-pane cycling; clicking it spawns a fresh shell in
  that slot (explicit user action, never automatic — the incident
  rule "nothing spawns that the user didn't ask for" applies here
  too).
* **Liveness rule**: a window's population = its non-dormant panes.
  When the last live pane leaves a window (moved out by drag or
  menu), the window closes — placeholders alone keep nothing alive.
  This generalises the shipped "1×1 dragged away → window vanishes"
  and the RFC-005 auto-close, and resolves their apparent conflict:
  placeholders persist only while at least one live pane shares the
  window.
* Closing a pane (⌫ button, menu, shell exit) keeps today's compact
  behaviour — close means "I'm done with this space".  Only move-out
  leaves a placeholder.  A placeholder itself can be closed via its
  own ✕ (compacts like a close).
* Persistence: a placeholder is part of the layout and survives a
  restart.  `SavedPane` grows a `flags: u8` (bit 0 = dormant) —
  `shell-state.bin` bumps to **v3**; the v2 reader maps to
  `flags = 0`, fail-soft versioning unchanged.  Boot assembly skips
  reattach/resurrect for dormant slots and materialises the
  placeholder directly.

Implementation note: the placeholder rides the existing
`PaneBackend::Vacant` machinery (already a paneless slot with its own
grid) plus a `dormant` bit that (a) suppresses the revive-on-keystroke
path in favour of revive-on-click, (b) excludes it from the liveness
count, (c) renders the recessed style instead of the "session
unavailable — press any key" message.

## 4. Drag out — a window is born, a window may die

* Release outside every marspot window: a new 1×1 window opens at
  default size, **centred on the release point** (clamped to the
  screen's visible frame), carrying the pane.  Mechanism: the shipped
  park-by-sid (`pending_move_sid`) + `WINDOW_OPEN_USER`, extended to
  carry the release point so L1 can place the window.
* If that drag emptied the source window of live panes, the source
  closes (§3 liveness) — dragging the sole pane of a 1×1 window to
  the desktop therefore reads as *the window following its pane*.
* Mis-drop protection stays structural: the 10 px slop, the ⇢ marker,
  and the preview (no marspot window under the pointer → the drag
  ghost renders nothing, but the title marker is still visible; the
  new-window ending is announced in the RFC table above and the
  layout it creates is trivially recoverable by dragging back).

## 5. Wire (all backward-compatible tail appends)

AppKit delivers the whole drag to the pressed window, so L1 resolves
the *hovered* window during the drag exactly as it resolves the drop
window today (`windowNumberAtPoint`, topmost-window semantics), and
converts the pointer into that window's physical coordinates.

* `MouseDrag` payload gains a tail: `hover_window_id u32, hover_x f64,
  hover_y f64` (after the existing window_id tail).  Old readers stop
  at their own tail; a new reader missing it gets hover = 0 = no
  preview, drag still resolves on release.
* `MouseUp` tail (already `drop_window_id`) gains `drop_x f64, drop_y
  f64` — physical coords in the drop window when `drop_window_id ≠ 0`,
  **screen points** when it is 0 (that is the only case that needs
  screen space: placing the new window).
* No PROTO bump, per the standing rule.

## 6. Rendering

* **Drop preview**: a `drop_preview: Option<Rect>` on `WindowState`,
  published to the renderer like every per-window overlay (the same
  slot pattern as context menu / layout modal).  Drawn as a
  translucent accent fill + 1 px accent frame via the existing
  `ui_rect` overlay primitives.  Cleared whenever the hover leaves
  the window or the drag ends — the same latch discipline as
  `hover_chrome_btn`.
* **Source dim**: while a drag is active the source pane's cell gets
  a translucent scrim (same primitive), replacing the bare ⇢-marker
  as the "you are dragging THIS" signal (marker stays — it also
  covers the pointer-outside-any-window case).
* **Dormant slot**: recessed background + centred hint, drawn by the
  cell painter from the pane's dormant flag; no per-frame cost when
  no dormant slot exists.

## 7. Edge cases (the checklist the tests pin)

| case | ruling |
|---|---|
| hover window closes mid-drag | preview clears; release over its ghost region = outside any window → new-window ending (or cancel if over another app) |
| source pane closes mid-drag (shell exits) | drag cancels (`pane_drag = None` on close_session — shipped) |
| source window closes mid-drag | drag cancels (shipped) |
| drop on own window's edge band | split within the own window (reshape + reinsert; the "swap only across windows" asymmetry would be surprising) |
| drop on the dragged pane's own center | cancel (no-op swap) |
| Esc mid-drag | cancel, preview clears |
| core swap / UPDATE_SWAP mid-drag | drag state dies with the core (it lives in CoreApp); release lands as a plain mouse-up in the new core — nothing moves, nothing crashes |
| grid at 6×6 both axes | downgrade to append+sidebar, previewed as such |
| population cap (64) | untouched — moves never spawn |
| drag over another app's window that overlaps a marspot window | `windowNumberAtPoint` is topmost-aware: the other app wins, preview clears, release there = outside → new window.  Deliberate: what you see is what you hit |
| multi-display | screen points from `NSEvent.mouseLocation` are global; new-window placement clamps to the release point's screen visible frame |
| dormant slot dragged | placeholders are not draggable (no live content to move); title press on one is inert |
| last live pane + dormant siblings, pane dragged out | window closes, dormant siblings die with it (they are layout, and their layout is gone) |

## 8. Execution plan (each step gates + installs independently)

1. `basic` dormant placeholders: Vacant+dormant backend, move-out
   leaves one, click revives, liveness rule + window auto-close on
   zero live panes; persistence v3 (`SavedPane.flags`).
2. `infra` wire tails: MouseDrag hover triple, MouseUp drop coords;
   L1 hover resolution during drag.
3. `basic` drop zones + grid reshape + swap; landing replaces
   step 5's append rule for drags (menu keeps append).
4. `basic` preview + source dim + dormant rendering.
5. `basic` drag-out → new window at pointer; window-follows-pane.
6. E2E: extend `test-multi-window.sh` with a scripted drag phase
   (dev seam driving the same state machine the mouse does), pinning:
   1×1 + right-band → 1×2; move-out leaves dormant; last-live-out
   closes the window; drag-out births a window at the point.

Non-goals: tmux-style arbitrary split trees (the grid stays), tab
strips, animated previews (the preview is a static rect; motion
polish is a later pass), drag of sidebar rows.

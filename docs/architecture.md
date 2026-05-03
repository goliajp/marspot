# Mars architecture

A living document. Updated alongside any structural change. The point is
not "how was it built" but "**where does work happen, what's the cost,
and where is the next bottleneck**."

## Modules and ownership

```
main.rs         entry point + winit event loop
                owns: Window, Renderer, Terminal, Pty, Receiver<Vec<u8>>
                threads: main (event loop) + 1 PTY-reader

pty.rs          forkpty wrapper
                owns: master fd, child pid (Drop reaps both)

parser.rs       byte → VT event state machine
                stateless across feeds (parser owns its UTF-8 / CSI state)

terminal.rs     parser callbacks → grid mutations
                bridges parser events to Grid + tracks current SGR attrs

grid.rs         passive 2D cell array + ring scrollback
                cell width helper (East Asian Wide detection)

render.rs       CGContext (CPU bitmap) → CGImage → CALayer.contents
                font fallback registry, per-codepoint cache
                run-length compresses BG fills + FG glyph runs by font/colour
```

## Hot paths

The two paths that determine perf:

### 1) Bytes path (PTY → screen)

```
kernel pipe → reader thread (libc::read, blocking)
            → mpsc::sync_channel(64) (bounded — backpressure)
            → main thread (winit user_event)
            → Terminal::feed (Parser::advance per byte → Handler callbacks)
            → Grid::set_cell / scroll_up / set_cursor
            → window.request_redraw()
```

Per-byte cost so far (measured/expected):
- libc::read: 1 syscall per chunk (typically 4 KiB), amortised ≪ 1 ns/byte
- channel send/recv: ~50 ns per chunk
- Parser::advance: per-byte branch, no allocation in steady state
- Handler::print: 1 cell write + cursor update; wide-char path writes 2 cells

### 2) Frame path (Grid → CALayer.contents)

```
Grid (cells × attrs) →
draw_frame (CGBitmapContext create) →
for each row:
  BG run-length fill (1 fill_rect per same-bg run)
  per cell font/glyph lookup via char_cache (HashMap hit, fallback miss = CT call)
  FG run-length glyph batches grouped by (font_idx, fg color)
  font.draw_glyphs (1 CTFontDrawGlyphs call per run) →
draw_cursor →
ctx.create_image() → CGImageRef →
layer.setContents(CGImageRef)
```

Per-frame cost (target):
- < 16 ms p99 on user's 1x display at 122×39 grid (full repaint)
- < 1 ms when nothing changed (TODO: dirty-region tracking; we currently
  always do the full redraw, no early-out)

## Allocation budgets

CLAUDE.md commits us to **zero per-byte and per-frame allocation in the
hot path**. Status:

| Layer | Per-byte alloc | Per-frame alloc | Notes |
|---|---|---|---|
| reader thread | 1 chunk Vec (≤4 KiB) | n/a | could amortise via ring buffer |
| Parser | 0 | n/a | ✅ |
| Handler | 0 | n/a | ✅ |
| Grid::set_cell | 0 | n/a | ✅ |
| Grid::scroll_up | 0 | n/a | ✅ (uses copy_within) |
| Renderer::draw_frame | n/a | 4× Vec::with_capacity | ❌ to fix: move buffers onto Renderer |
| Renderer::resolve_char | n/a | HashMap::insert on first sight | OK: warm-up only |

## Latent issues (architectural, not just bugs)

- **Retina double-scale**: WindowEvent::Resized used to multiply size by
  backingScaleFactor (winit already returns physical). Fixed in resize
  path; initial sizing in `resumed()` still has the same bug. On 1x
  displays it cancels out so it looks fine. Will bite when the user
  moves the window to a Retina monitor.

- **No dirty tracking**: every redraw rebuilds the entire CGImage from
  scratch. For a typing session where only one cell changes per
  keystroke, this is wasteful. Future: maintain a dirty-rect mask, only
  re-render changed rows.

- **CALayer contents replaced wholesale every frame**: we hand CA a
  fresh CGImage. CA may not detect "same image, no change" — we should
  early-out when grid is unchanged.

- **font_cache is unbounded**: HashMap<u32, _> grows monotonically. A
  user typing all 1.1M codepoints would eat ~16 MB. Acceptable for
  realistic terminal use, but document the bound.

## Architecture-review checklist

Run before each merge to develop:

- [ ] Any new per-frame allocation? → must justify or move to Renderer field
- [ ] Any new per-byte allocation? → must justify or remove
- [ ] Any new dependency? → must be FFI-only or document why
- [ ] Module boundary violation? (e.g. renderer touching PTY) → redraw lines first
- [ ] New "TODO: fix later" → already pile up? schedule a refactor commit
- [ ] Hot path got a new branch / lookup / lock? → measure delta vs baseline

If any answer is "yes, made worse": refactor before merging or open an
explicit "tech-debt" task with a deadline.

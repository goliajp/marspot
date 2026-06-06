# Marspot architecture

A living document. Updated alongside any structural change. The point is
not "how was it built" but "**where does work happen, what's the cost,
and where is the next bottleneck**."

## Modules and ownership

```
main.rs         entry point + MarsApp impl
                owns: Renderer, Terminal, Pty, Receiver<Vec<u8>>
                threads: main (event loop) + 1 PTY-reader

app.rs          AppKit-direct window + run loop (NSApplication.run)
                owns: NSWindow, custom NSView, CFRunLoopSource (wake)
                dispatches MarsApp callbacks on the main thread

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
            → main thread (MarsApp::user_event via CFRunLoopSource wake)
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

- **Retina double-scale**: long-standing bug from the winit-era code —
  initial sizing in `resumed()` could use the wrong backing scale on
  multi-display setups.  After the winit removal, `app::run_app` fires
  an explicit Resized after window mount that uses the actual
  view-bounds × backingScaleFactor, so the bug should now be self-
  correcting; the `Latent issue` line stays here until verified
  on a real multi-monitor setup.

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

## Gates and dev-loop tooling

The benchmark gate alone won't keep marspot honest — perf is necessary
but not sufficient.  Correctness (no UB, no parser panics), dependency
hygiene (no audit-flagged crates, no license drift, no unused deps),
and footprint stability (no fd / RSS leak over thousands of cycles)
each get their own gate.  Together they form the "every push is
honest" envelope; missing any one and a regression class becomes
invisible.

| Script | Tier | Runs | Catches |
|---|---|---|---|
| `bin/bench.sh` | fast | every commit / pre-push | parse / render perf floor (median over 5 trials) |
| `bin/bench.sh --full` | gate | pre-merge to develop | live PTY throughput + vs-best-other ratio against `bench/baseline.json` |
| `bin/bench-remote.sh [--full]` | gate | when local box is busy or baseline lock-in needed | the same gate on `ssh mini` — clean idle Apple Silicon host, no foreground jitter |
| `bin/bench-run.sh` | refresh | manual; refreshes multi-session snapshot | multi-session-9x, scrollback-1m, idle-9x, vim-jump, htop-60s, active-9x-soak |
| `bin/test.sh` | fast | per change | `cargo nextest run --lib` — 134 tests with parallel scheduling + per-test process isolation |
| `bin/soak.sh` | nightly / pre-release | manual | the 5 `#[ignore = "soak"]` tests: 1000 spawn fd / child / RSS leak (pty.rs), 10 M-line scrollback bound for mem + disk variants (terminal.rs) |
| `bin/fuzz.sh` | nightly / on parser change | manual; `DURATION_S=` configurable | parser panics on arbitrary byte input (cargo-fuzz, libFuzzer) |
| `bin/miri.sh` | on UB risk | manual | UB / aliasing violations in pure-Rust modules (parser, grid, tmux, input — 44 tests; FFI-using modules covered by integration tests) |
| `bin/lint-deps.sh` | pre-push | manual / hook | `cargo audit` (RustSec advisories) + `cargo deny check` (license policy, duplicate-version=deny, source provenance) + `cargo machete` (unused declared deps) |
| `bin/profile-samply.sh` | when investigating regression | manual; `--attach <pid>` supported | flamegraph-friendly profile via samply (Speedscope / Firefox-profiler JSON) — visual complement to `bin/profile-live.sh`'s text sample report |
| `bin/sync-toolchain.sh` | onboarding / when bench host drifts | manual; `HOST=mini` | one-command alignment of 13 cargo bins between dev box and remote bench host |

### Why this layering matters

The bench gate catches "did this change make marspot slower," but
several whole regression classes are invisible to it:

- A parser that panics on a CSI edge case still benches fine on
  realistic input — `fuzz.sh` is what surfaces it.
- A use-after-free in the grid that only manifests under specific
  alias patterns benches fine until it doesn't — `miri.sh` catches
  it on the pure-Rust modules where Miri can run.
- A dep upgrade that pulls in an RustSec-flagged transitive dep
  benches fine — `lint-deps.sh` (`cargo audit`) catches it.
- A debug-print left in a hot path benches fine in release builds —
  `cargo build` is gated against warnings (0 warnings invariant
  re-established 2026-06-06).
- An fd or process leak that only shows after 1000+ PTY spawns
  benches fine on the 5-trial median — `soak.sh` is what catches it
  before it manifests in a 9-hour Claude-Code working day.

Each tool is one cheap, automatic check; together they make "every
green merge" mean meaningfully more than "the bench gate passed."

# Marspot architecture

A living document. Updated alongside any structural change. The point is
not "how was it built" but "**where does work happen, what's the cost,
and where is the next bottleneck**."

## RFC-003 §6 Amendment 16 — three-layer split + L3 self-execv (2026-06-17)

The shipped architecture is three processes per running marspot
instance + N per-pane L3s + (per-layer) the shell child a user
actually typed at:

```
L1 marspot-shell    Pure "shell".  NSWindow owner + NSApp delegate
                    (Cmd-Q routes through CloseRequested), binary
                    tree manager (current/prev/pending/quarantine),
                    install-local trigger, supervisor state machine
                    (Idle / Probing, restart_core, crash budget).
                    NO business logic — does not know what L2 does
                    with the L3s it spawns; does not hold L3 fds.

                    Self-update path: probe the staged binary on a
                    background thread, then dump frame to NSUser-
                    Defaults, execv "current/marspot-shell", reattach
                    IOSurface (kernel object survives the image swap),
                    restore frame.  ~100 ms visible flash; "size +
                    position + content all preserved" is the user
                    contract.  Rare path — L1 binary updates seldom.

L2 marspot-core     UI brain.  Metal renderer / cell layout / pane
                    management / input dispatch / per-frame composit-
                    ion.  Spawns L3 children at boot, reattaches to
                    surviving L3s across an L2 swap via the on-disk
                    session registry.  THIS is the version users
                    mean when they say "what marspot are you on?".

                    Self-update path: probe, then single-core in-place
                    swap.  L1 execs the staged core once on a back-
                    ground thread — proving it starts and paying its
                    Gatekeeper assessment while the live core is still
                    drawing — then retires the live core and spawns
                    the replacement onto the SAME IOSurface pair.
                    ~200-500 ms where no core is writing frames.

                    It was a dual-core swap until 127f3c9: pending and
                    active L2 ran side by side through a probation
                    window.  RFC-003 made L3 single-client, so the
                    pending core's hello killed the active core's L3
                    control sockets — panes visible, keystrokes on the
                    floor, for the whole window.  The probe recovers
                    what dual-core was actually for (never retire the
                    incumbent until the successor is known-good)
                    without the two-clients-at-once problem.

L3 marspot-session  One process per pane.  Owns the PTY master fd,
                    shell child, VT parser, grid, scrollback,
                    bytelog, shm framebuffer (L2-readable), UDS
                    control socket.  Each L3 writes its registry
                    entry on bind: `sessions/<id>/entry.toml` carries
                    pid, socket path, cols/rows, shm name, AND
                    shell_child_pid (the zsh that L1 plugins like
                    claudecode walk via pidtree).

                    Self-update path (Amendment 16, L4-shelld model):
                    on SIGTERM, the L3 reads its own MARSPOT_FP_TERM
                    rodata fingerprint and the `current/marspot-
                    session` binary's MARSPOT_FP marker.  If they
                    differ:

                      1. extract_for_handoff: mem::forget(self) so
                         Pty::Drop doesn't SIGHUP the shell
                      2. serialize_snapshot → sessions/<id>/state.bin
                      3. clear CLOEXEC on master_fd + listen_fd
                      4. write /tmp/marspot-session-handoff.<pid>.tsv
                      5. setenv MARSPOT_L3_HANDOFF_MANIFEST=<path>
                      6. execv("current/marspot-session", argv)

                    The new image's main() detects the env sentinel,
                    parses the manifest, and rebuilds LocalSession +
                    SessionListener via from_handoff — adopting the
                    inherited PTY master fd + UDS listener fd +
                    apply_snapshot to the saved Terminal state.
                    PID is preserved across execv, shell child is
                    unchanged, sock file path is unchanged, L2's
                    control socket sees a brief read pause then
                    resumes.  ~1-2 ms total per L3.

                    If fingerprints match (no real update) or
                    current/marspot-session doesn't exist: clean-exit
                    branch.  serialize_snapshot, process::exit(0) —
                    no unwind, no Pty::Drop, so the kernel SIGHUPs
                    the shell as the parent dies.  That's the
                    user-quit semantic.

shell (zsh)         The actual login shell at the slave end of L3's
                    PTY.  PID + tty session preserved across every
                    silent install from v0.5.0 onward — vim, ssh,
                    REPLs, claudecode all keep running.
```

### Pyramid invariant

The whole silent-update story rests on one rule: **each layer holds
the stateful resources of the layer below**.

```
L0 = kernel / WindowServer    holds: IOSurface
L1 = marspot-shell             holds: IOSurface refs, binary slots, plugin host runtime
L2 = marspot-core              holds: pane registry, control sockets to L3s
L3 = marspot-session           holds: PTY master fd, UDS listener fd, shell child PID
shell                          holds: user's working state
```

When L3 updates, L3 keeps its own fds (via execv inheritance).  When
L2 updates, L2 reattaches L3s by reading their on-disk registry — fds
are not transferred, sockets are reopened by path.  When L1 updates,
L1 dumps geometry to disk + execv's; IOSurface is held by the kernel
(via WindowServer), L2 keeps holding its half of the pair, content
restores from the same kernel objects.

The earlier Amendment 15 tried to put L3 PTY fds in an "L1 fd-vault"
and was wrong — `OwnedFd` defaults `FD_CLOEXEC=1`, so when L1
execv'd itself the vault's fds closed and every shell got SIGHUP'd.
The fix (Amendment 16): persons holding the fd are the ones doing
the execv.  L3 owns its own fd → L3 execvs itself → fd survives.

### L1↔L2 wire (`shell_proto`)

Bidirectional UDS over fd 3 (inherited from L1's socketpair at L2
spawn).  Active core only — pending is held off this wire until it
gets promoted.  Frame format = magic + msg_type + length-prefixed
payload.  `Frame::read_from` silently skips unknown msg_types
(forward-compat across version skew).

```
L1 → L2: KeyEvent / MouseDown/Drag/Up / Scroll / Preedit / Resize /
          SurfaceAttach / Focus / Hello / Ping /
          PaneBadge / PaneSessionBegin / PaneSessionEnd /
          InjectInput               (cc plugin pushes raw PTY bytes)

L2 → L1: HelloAck / Pong / SurfaceReady / FrameRendered /
          CaretRect / PaneBadgeClicked /
          PaneSessionKey / PaneSessionUserEscape / PaneSessionOverlay
```

### L2↔L3 wire (per-pane UDS at sessions/<id>/sock)

`marspot-term::shell_proto::Frame` framing (same crate as L1↔L2
wire).  Hello/HelloAck handshake on connect.  L3 generations adopt
the latest client → forwards keystrokes from L2, publishes
GridReady poke after every shm publish.

```
L2 → L3: KeyEvent / GridResize / GridScroll / Paste /
          GetSelectionText /
          InjectInput               (relayed from cc, raw bytes →
                                     PTY no bracketed-paste wrap)

L3 → L2: GridReady / SelectionText
```

### L1 ↔ L3 (no direct wire)

L1 never opens an L3 control socket.  Plugins running in L1 (e.g.
claudecode) read entry.toml to enumerate sessions, walk pidtree
from `shell_child_pid` to find descendants (a claude binary,
say), and bounce keystrokes through L2 via the InjectInput proxy:

```
cc plugin tick → host.cc_inject_proxy(sid, bytes)
              → InjectInputRequest channel
              → shell main loop drain
              → MsgType::InjectInput frame to active L2
              → L2::inject_input(sid, bytes)
              → pane.forward_inject_input
              → MsgType::InjectInput frame on L3's UDS
              → L3 main loop SessionEvent::InjectInput(bytes)
              → session.write(bytes)  (raw PTY write)
```

This keeps L1 ignorant of L3 internals; L2 is the only layer that
talks to L3.

### On-disk state

```
~/Library/Caches/marspot/
├── binaries/
│   ├── current/       L1 spawns from here on every layer
│   ├── prev/          rollback target
│   ├── pending/       staged by install-local; promoted on next L1 / L2 / L3 boot
│   └── quarantine/    binaries that failed probation
├── sessions/<id>/
│   ├── entry.toml     pid + socket + shm_name + shell_child_pid + ...
│   ├── sock           L2↔L3 UDS listener path (also survives L3 execv)
│   ├── state.bin      Terminal snapshot persisted on SIGTERM
│   └── bytelog        raw PTY bytes, append-only, scrollback source of truth
├── shell.pid          L1 supervisor pid
└── logs/marspot.log   logx TSV (shared by every binary)
```

`/tmp/marspot-session-handoff.<pid>.tsv` is a transient handoff
manifest written by the pre-execv L3 image, consumed and deleted by
the post-execv image.

### Cmd-Q / window-close semantics

`MarspotWindowDelegate` implements both `NSWindowDelegate` (red
button / Cmd-W) and `NSApplicationDelegate` (Cmd-Q / dock Quit / menu
Quit).  Both routes dispatch `EventKind::CloseRequested`, which
runs L1's `close_requested`:

  1. plugins stop
  2. SIGTERM every entry.toml pid
  3. pgrep sweep marspot-session for orphans (entry.toml may have
     been overwritten on same-id spawn races; pgrep covers leaks)
  4. 200 ms wait for SIGTERM handlers to land state.bin
  5. shutdown active core
  6. tear down pending update (if any)
  7. ctx.exit() → run_app returns → process exits cleanly

`applicationShouldTerminate:` returns `NSTerminateCancel` so AppKit
doesn't race the `windowShouldClose:`-returns-false path; close_
requested is the single sink.

Replaces the prior L1+L2+L3+L4 split where L4 was `marspot-shelld`,
a per-user daemon that owned every PTY master.  See
`docs/rfc-003-l3-pty.md` for the migration trail and Amendment
post-mortems (14 SIGUSR2-killed-sessions, 15 fd-vault-broke-L1-
update, 16 self-execv landing).

## Modules and ownership (pre-RFC-003 standalone-marspot view, still accurate for src/main.rs)

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

## Idle / present cadence (BG flicker class)

The shell's `redraw()` short-circuits when no `FrameRendered` poke
is pending, but a stale safety net used to force-present after 1 s.
At idle the core sleeps a 1 s recv_timeout and never re-renders, so
that branch fired every ~1 s, presenting an unchanged IOSurface and
letting WindowServer composite sub-LSB alpha-blend rounding into a
faint 1 Hz BG flicker the user sees across all panes.

Fix (commit b06aea8, shell 0.5.1): stale threshold 1 s → 5 s.
Crash respawn is detected separately via `child.try_wait()` →
`restart_core`, so this safety net only catches "core alive but
silent", which is fine at 0.2 Hz.

If a future change re-introduces a per-second forced present anywhere
in the render path: it WILL flicker.  The bar is "present only when
content actually changed."

## Main-loop blocking discipline (2026-07-19/20)

L2 and L3 are each a single-threaded event loop.  Any blocking call made
*from* the loop freezes everything the loop owns — for L3 that is one
pane, for L2 it is **every** pane.  Five bugs of exactly this shape
shipped and were fixed in one pass; the rule below exists so a sixth
doesn't.

**Rule: the loops never block.**  Anything that can wait — a socket
write, a disk write, a connect handshake, a process spawn — is handed to
a thread and the loop gets a queue or an event back.

| What used to block | Where it lives now |
|---|---|
| `write(2)` to the PTY master | `PtyWriter` thread (`marspot-session/local_session.rs`) |
| L3→L2 control frames | `ControlWriter` → `FrameWriter` (`marspot-term/frame_writer.rs`) |
| L2→L1 and L2→L3 frames | `FrameWriter` (same type, three call sites) |
| periodic `state.bin` write | `SnapshotWriter` thread (`marspot-session/main.rs`) |
| bytelog compaction (50 MiB copy) | replaced by O(1) rotation (`marspot-term/bytelog.rs`) |
| `wait_and_connect` on EOF-reconnect | worker thread → `CoreEvent::L3ControlReconnected` |
| `spawn_l3` on `[+]` / revive | worker thread → `CoreEvent::L3SpawnFinished`, slot renders "starting…" |

Two measurements motivate the socket cases, both taken on this codebase
rather than assumed:

* macOS gives an `AF_UNIX`/`SOCK_STREAM` socket an **8 KiB** send buffer.
  A peer that stops reading parks a blocking `write_all` at byte 8193.
* A PTY master in **raw** mode (any TUI) accepts ~1 KiB then blocks;
  in canonical mode it discards instead.  That asymmetry is why only
  TUI panes ever froze.  Reproduced by
  `local_session::tests::write_does_not_block_on_a_child_that_ignores_stdin`,
  whose child runs `stty raw -echo; sleep 3` — the `stty raw` is
  load-bearing, a canonical-mode child lets the old blocking code pass.

**Queues are bounded and drop whole frames.**  A partial frame would
desynchronise the protocol stream permanently; a dropped frame costs one
poke, one reply, or one redraw.  Caps: 4 MiB per control socket (must fit
one `SelectionText` carrying a deep selection), 1 MiB L2→L1, 256 KiB per
PTY.

**Async pane bring-up.**  `[+]` and revive-on-keystroke go through
`spawn_l3_pane_async`: the loop only allocates a session id, drops in a
slot that renders "starting…", and the fork/shm-map/handshake run on a
worker that reports back via `CoreEvent::L3SpawnFinished`.  "Starting"
is not a peer state to "vacant" — it is `VacantPane { pending: true }`,
because a slot with a spawn in flight *is* a vacant slot that has work
coming.  The flag also makes `is_exited()` report false, which is what
stops the next keystroke from firing a second spawn on top of the first.
Boot assembly still uses the synchronous path: there is no loop to
freeze before the loop starts.

**`LoopWatch` makes a stall self-reporting** (`marspot-term/loop_watch.rs`).
Each iteration is timed and split into named phases; an iteration past
the threshold (L3 150 ms, L2 80 ms — L2 is tighter because it owes a
frame) emits one `l3.loop.stall` / `l2.loop.stall` line naming the
slowest phase.  Below the threshold it is silent, so a healthy session
logs nothing.  This exists because the first two stalls could only be
diagnosed by catching the process live and reading a stack: sessions log
on events, so a wedged loop and an idle loop were indistinguishable.

## Windows (RFC-005, in progress)

L2's `CoreApp` used to hold one window's worth of state directly —
one `Layout`, one `Vec<Pane>`, one `focused_idx`, one grid shape, one
`(w_phys, h_phys, scale)` — which encoded "there is exactly one
window" into ~450 field accesses.  Those fields now live in
`WindowState`, and `CoreApp` holds `windows: Vec<WindowState>` plus
`key_window`.

Access goes through `win!(self)`, a macro that expands to
`self.windows[self.key_window]`.  It is a macro rather than an
accessor method so it stays a plain **field path**: the borrow
checker still sees `win!(self).panes` and `self.renderer` as disjoint
borrows, which an `fn win_mut(&mut self)` would not (it borrows all
of `self` and makes most of the render path unwritable).

What stayed on `CoreApp` is the set that is *not* window-scoped:
everything keyed by session id (`pane_badges`, `pane_titles`,
`pane_cwds`, `pane_sessions`, …), which travels with a pane across
windows for free, plus the renderer and the event channel.

The split is what makes a pane portable: a `Pane` owns its backend,
session id, control socket, scroll offset, search state and title, so
moving it between windows is a `Vec` move and L3 never learns it
happened.  `window_state_tests::moving_a_pane_between_windows_carries_its_state`
pins that.

See `docs/rfc-005-multi-window.md` for the full plan — one core with
N surfaces (not a core per window), the wire's `window_id`, the
renderer's shared-by-scale split, and the per-window persistence
format.

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

- ~~**font_cache is unbounded**~~ — fixed 2026-07-20.  `char_cache` is
  capped at `CHAR_CACHE_CAP` (8192) and evicts the single coldest entry
  via a recency stamp (`src/font_cache.rs`).  It used to `clear()` the
  whole table on overflow, which turned the cap into a cliff: the next
  frame re-resolved every visible cell, and each codepoint the base font
  lacks costs a `CTFontCreateForString` cascade.  `ShapeCache`
  (`src/font_shape.rs`) got the same treatment — its old `VecDeque`
  ordering made every cache *hit* an O(cap) scan of `String` keys, the
  one place in the tree that literally got slower the longer it ran.

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
| `bin/soak.sh --smoke` | pre-push (when touching PTY / spawn / fd code) | manual; ~30 s warm | the 3 pty spawn-loop assertions (fd / child / RSS leak over 1000 spawns) — every push catches OS-resource leaks without slowing the gate by minutes |
| `bin/soak.sh` | nightly / pre-release | manual; ~5–10 min | all 5 `#[ignore = "soak"]` tests: above + 10 M-line scrollback bound for mem + disk variants (terminal.rs) |
| `bin/scenarios/idle-9x.sh marspot <out> --extended` + `active-9x-soak` (via `bench-run.sh --extended`) | pre-release | manual; 30 min each | end-to-end process soak — mcli + glyph atlas + Metal + AppKit; what `soak.sh` can't cover because it stays headless cargo-test territory |
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

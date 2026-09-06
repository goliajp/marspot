# RFC-008 — the same pane affordances for more than one agent

Status: in progress (2026-09-06).  Slice 1 (badge) and slice 2 (wheel)
shipped; the rest is listed at the end and not committed to.

## Why this is small

The plugin framework was already built for more than one agent, which
is why adding codex is a 200-line plugin rather than a fork of
`claudecode.rs` (7,900 lines):

- the pane→process binding takes a **predicate** (`looks_like_*`),
- badges and titles go through **`PluginHost`**, not through anything
  claudecode owns,
- the registry **dispatches to whichever plugin claims a session**.

A pane runs one agent at a time, so both plugins register
unconditionally and each claims only the sessions whose process tree
it recognises.

## Recognition

argv[0], not `comm`: a released agent renames its own process
(claudecode to its version string, via `exec -a`).  The basename must
match exactly — `codex-code-mode-host` is a helper codex spawns, and
accepting it would bind a pane to the wrong pid and badge it twice.

Sessions come from `session_registry::list_session_entries()`, not
from pane indices: an index is a position in a layout that moves when
panes are dragged or closed, while a session id is what a badge is
addressed to.

## Slice 1 — badge

`model` and `model_reasoning_effort` from `~/.codex/config.toml`,
published as `gpt-6-astra·high` — the shape claudecode's badge already
uses, so a window holding both reads as one system.  Only top-level
keys count; the file carries `[projects."…"]` tables whose own `model`
would otherwise win by being last.

## Slice 2 — the wheel (`PaneWheelKeys`, msg 79)

### What was actually wrong

codex could not be scrolled at all.  Measured, same pane geometry:

```text
  claudecode  flags=0x1d  MOUSE_TRACKING on   scrollback_len 0
  codex       flags=0x05  MOUSE_TRACKING off  scrollback_len 0
```

Neither has scrollback, and that is correct: both repaint in place and
never push a line out of the top (`scroll_push = 0` against 617 lines
of output, verified).  The difference is the one bit — claudecode asks
for mouse reporting, so the wheel is forwarded as SGR events and it
scrolls its own history.  codex asks for nothing, so the wheel landed
in an empty ring.

Three explanations were tried and each was wrong, which is worth
recording because each was self-consistent:

| tried | why it failed |
|---|---|
| alt-screen has no scrollback | marspot gives the alt screen a full ring on purpose; not the cause |
| send arrow keys when in alt-screen | codex is in the MAIN screen while its UI is up (`ALT_SCREEN off`) |
| `codex --no-alt-screen` ("preserving terminal scrollback history") | measured: 617 lines out, `scroll_push` still 0 |

### What is true

codex has its own scrolling, behind its own keys — and it is
two-stage.  Verified by injecting into a real pty:

```text
  normal view   PageUp/PageDown  →  nothing changes
  Ctrl+T                         →  opens /TRANSCRIPT/ (32 of 33 rows change)
  transcript    PageUp/PageDown  →  pages (29 of 33)
                ↑/↓              →  line by line (28 of 33)
  Esc                            →  back to the input view
```

So reaching such a program means pressing keys only its plugin knows,
and `PageUp` alone would have done nothing — a fix that looked right
and moved nothing.

### Shape

The plugin **declares**, L2 **executes**.  A round trip to L1 per tick
would sit inside a momentum scroll (tens of ticks), so L2 holds the
declaration and runs it.

**State is read, never remembered.**  The first version kept an
`entered` bool set when `enter` was sent and cleared on the user's
`Esc`, and it broke the same day: leaving the transcript and scrolling
again could not get back in.  Two things it got wrong —

- the program leaves that view on its own, not only by the user's key,
  so the flag went stale with nothing to clear it;
- `enter` is typically a **toggle**.  Measured: a second `Ctrl+T`
  closes codex's transcript (33 of 33 rows change back, and `PageUp`
  stops working).  So a stale flag does not merely fail to open the
  view — it shuts it.

The declaration therefore carries a `marker`: text the program shows
while its view is open (codex draws a `/TRANSCRIPT/` rule that
survives paging — verified).  L2 scans the visible grid for it once
per wheel event and sends `enter` only when it is absent.  No flag can
go stale because there is no flag.

Nothing automates leaving that view.  The user's ruling: *"滚轮主动进
但不主动退出"* — one stray tick at the bottom would otherwise close
what they were reading.  Leaving stays the user's `Esc`, and since
there is no flag, nothing has to notice that they pressed it.

And only an UPWARD tick may open it (*"滚动鼠标向下的时候我不想触发
ctrl+t 要向上才触发"*).  Reaching for history is an upward gesture; a
downward tick with the view closed means "I am at the newest, show me
what is below", and answering that by opening a history view is a
surprise.  Such a tick is not ours at all — it falls through to the
pane's own routing rather than being swallowed.  An OPEN view still
takes both directions, or there is no way back down.

The marker is compared with whitespace squeezed out of both sides.  A
program draws headings for humans, not for matchers: codex
letter-spaces its rule, so the cells read `/ T R A N S C R I P T /`
while the plugin sensibly declares `/TRANSCRIPT/`.  Matching literally
never succeeded, and since `enter` is a toggle, every tick then opened
and shut the view — the "一动就闪" report, and the third wrong fix in
a row.  Squeezing cannot invent a match: a marker is a distinctive run
a plugin picked because ordinary output does not contain it.

The wheel maps to the program's LINE keys, not its page keys.  The
caller already turns a trackpad's pixels and a mouse's notches into an
accelerated line count; spending a whole screen on one flick of a
finger was the gap against iTerm2 (*"鼠标滚动也是一行行带加速，我们
一下就滚一屏"*).

### The declaration has to survive a core swap

A badge is re-issued by its plugin every tick, so a frame dropped
while no core is attached costs one tick.  A wheel declaration is
issued ONCE per pane — the plugin says how the wheel reaches it and
has nothing to repeat — and the drain dropped it silently when no core
was attached.

That is not a rare window; it is exactly the gap a core swap opens,
and it is where codex landed: L1 re-execed, the plugin declared a
second later, the core was still booting, and the pane had no wheel
keys for the rest of its life.  The shell now keeps the mapping and
replays it on every handshake.

### The declaration is the honest "an agent paints this pane"

Link scanning has to merge a path a program broke across lines itself
(no DECAWM flag, because the program wrapped it, not the terminal).
That merge was gated on "the plugin badge is non-empty" — and codex's
badge is built from a model and an effort read off disk, so a read
that comes back empty leaves the badge empty and the pane silently
stops merging wrapped links (2026-09-06 field report: a path
underlined only as far as `…/lab36-continus/`).

A wheel declaration is the durable assertion instead: a plugin only
makes it about a program it is driving, and it now survives a core
swap.  `SessionView::agent_tui` carries it.

Keys are bytes on the wire, not a named enum: what a program answers
to is its own business, and the protocol should not need a new variant
each time a plugin learns a new program.

### Not applicable to claudecode

It asks for mouse reporting, so it keeps the `MouseEvents` route and
never sees this path.  The generic arrow-key route added in core
0.12.169 also remains, for programs that DO have history the terminal
can move but cannot receive a wheel (`less`, `man`, a mouse-less
`vim`).

## What chasing codex exposed in the terminal itself

None of these are plugin work — they are places where marspot was
wrong and only an agent TUI happened to stand on them.  Detail in
CHANGELOG.md.

- **DEC 2026 (synchronized output) was on the accept-and-ignore
  list.**  codex brackets every frame with it — 8,493 pairs in one
  session's byte log.
- **A screen wiped between two PTY reads reached the display.**  codex
  clears OUTSIDE its synchronized batch (`… 2026l · CSI J · 2026h ·
  paint …`), so the empty grid in between was published: the black
  flash on opening its transcript, measured as a `38% → 0% → 44%` fill
  sequence.  A completely blank screen now waits 50 ms.
- **Every frame blocked the main loop on `waitUntilCompleted`.**
  Across 166 stalls: mean wait 374.5 ms against 3.3 ms of GPU
  execution, worst 3.6 s — with input queued behind all of it.  The
  frame is now committed and polled.

## Not done

- **profile-cycle for codex.** claudecode's (SIGTERM → await quiet →
  relaunch with `--resume`) leans on claude's session-resume
  semantics.  codex's equivalent is not established, and guessing puts
  a plugin in a position to kill a running agent mid-task.
- **badge right-click menu for codex.** claudecode's switches profile;
  the codex equivalent would switch model or effort, which needs a
  safe way to change them in a running session.
- **A program with no on-screen marker.** The scan needs something to
  look for.  Declaring an empty `marker` currently means "assume the
  view is open", i.e. never send `enter` — safe (it cannot toggle the
  view shut) but only useful for a program whose scroll keys work
  without opening anything.

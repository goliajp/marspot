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

## Slice 3 — markup a program does not render (`PaneRenderMarkup`, msg 80)

codex does not render HTML, so a model that writes `<u>…</u>` has its
markup arrive on screen as text.  Asked for on 2026-09-06: *"`<u></u>`
是下划线，你就渲染就好了"*.

It took three attempts, and the third correction is the one worth
keeping:

1. **Semantics.**  Turning underline on at the opening tag underlines
   everything after an unmatched one — and text that merely MENTIONS
   the tag is most of any conversation about this feature.  Only a
   matched pair styles anything; the span is withheld until `</u>`
   arrives, and if it never does the tag is printed exactly as it came.
   The failure mode is then the behaviour from before the feature
   existed.
2. **Scope.**  Shipped as a global setting, it ate the tags out of the
   conversation SPECIFYING it, including the user's own words coming
   back on screen.  Whose output is markup is not a property of the
   terminal — a terminal is where people talk about markup.  It belongs
   to the pane, and only the plugin driving a program knows the program
   does not render its own HTML.
3. **Persistence.**  The one that cost the most.  A feature that should
   affect a span of text was writing TERMINAL STATE: the underline
   attribute survives in the pen, is inherited by every later cell, and
   rides across snapshots and execv.  A full-screen program can run for
   hours without emitting `CSI 0 m`, so nothing takes it back.  One
   pane stayed underlined through six image swaps.

The last one produced two lasting fixes beyond this feature:
`reset_process_owned_modes` now clears the pen (a style left on by a
dead program is the same debt as a mode it left set), and
`PaneResetAttrs` (msg 81) gives a stuck pen a way back that does not
cost the user their session.

## Slice 4 — the badge tells the truth, and answers a right-click

Two things that had to happen in that order.

**The badge was reading the wrong file.**  `~/.codex/config.toml` says
what a FRESH codex starts with, not what the one in this pane is
doing: two panes on different efforts both showed the global value.
codex writes a `turn_context` per turn carrying `cwd`, `model` and
`effort` together, so the last one in a session's rollout is the
answer, matched to a pane by the codex process's own working
directory.  Bounded — rollouts reach 63 MB, so only the last 256 KiB
is read, the cwd→file mapping is cached and rescanned every 20 s, and
the content is re-read only when that file's mtime moves.  A record
that cannot be found leaves the badge on the global fallback rather
than on a wrong value.

**Then the menu could mean something.**  Right-click offers
`low`/`medium`/`high` with the current one marked, and a pick takes
codex down and brings the SAME session back at the new effort.

This was the RFC's headline "not done", deferred because codex's
resume semantics were unknown and guessing puts a plugin in a position
to kill a running agent.  Measured instead:

- `codex resume --last` filters by working directory, so inside a
  pane's own cwd the most recent session is that pane's.
- a `-c` value is parsed as TOML and falls back to the raw string, so
  `-c model_reasoning_effort=high` needs no quotes — which matters,
  because `pty_op`'s command line rejects quotes as a class.
- the three efforts come from codex's own serde variant table.

And on killing a running agent: claudecode's profile-cycle does
exactly this today.  The click is the authorisation; what it blocks is
a session held by another process, not an agent that happens to be
busy.

## What chasing codex exposed in the update pipeline

Half a day was spent on "the declaration does not work" that turned out
not to be about declarations at all.  Every one of these reported
success while doing nothing.

- **A silent update swapped L1 and L2 but not the running L3s.**  Only
  panes spawned afterwards got the new session image; existing ones
  kept theirs for as long as they lived.  Two session-layer fixes had
  never once run on the machine they were installed on.
- **A freshly-installed binary is a cold inode.**  Thirteen panes
  probing it at the same instant all got a failure back within 144 ms
  and refused to adopt it — correctly, by their own rule — while four
  consecutive installs said they had succeeded.  The installer now
  runs it once itself before asking anyone else to.
- **The probe had no deadline, and "probe outstanding" means "already
  handled".**  One check that never answered retired a pane's
  self-update permanently and silently.
- **The installer reported signals SENT.**  That is not the same claim
  as "they took it".  It now compares each pane's mapped inode against
  the installed one and names the stragglers.

The shape they share: a protection that is right, whose failure is
silent and permanent, behind a report that says success.

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

- **A first test that was green and proved nothing.**  Written for the
  probe deadline, it used `/bin/sleep --version` as "a candidate that
  never answers" — but that rejects the argument and exits at once,
  taking the answered-non-zero path.  Three tests passed in 0.03 s
  having never entered the branch under test.  Replaced with a script
  that genuinely hangs, against an injected deadline.

## Not done
- **A program with no on-screen marker.** The scan needs something to
  look for.  Declaring an empty `marker` currently means "assume the
  view is open", i.e. never send `enter` — safe (it cannot toggle the
  view shut) but only useful for a program whose scroll keys work
  without opening anything.

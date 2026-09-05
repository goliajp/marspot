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
declaration and the `entered` flag, and sends `enter` once when
scrolling starts from the program's normal view.

Nothing automates leaving that view.  The user's ruling: *"滚轮主动进
但不主动退出"* — one stray tick at the bottom would otherwise close
what they were reading.  A real `Esc` keypress is what clears
`entered`, tracked where keys are forwarded rather than guessed.

Keys are bytes on the wire, not a named enum: what a program answers
to is its own business, and the protocol should not need a new variant
each time a plugin learns a new program.

### Not applicable to claudecode

It asks for mouse reporting, so it keeps the `MouseEvents` route and
never sees this path.  The generic arrow-key route added in core
0.12.169 also remains, for programs that DO have history the terminal
can move but cannot receive a wheel (`less`, `man`, a mouse-less
`vim`).

## Not done

- **profile-cycle for codex.** claudecode's (SIGTERM → await quiet →
  relaunch with `--resume`) leans on claude's session-resume
  semantics.  codex's equivalent is not established, and guessing puts
  a plugin in a position to kill a running agent mid-task.
- **badge right-click menu for codex.** claudecode's switches profile;
  the codex equivalent would switch model or effort, which needs a
  safe way to change them in a running session.
- **`Ctrl+T` state tracking.** L2 assumes the view is closed until a
  wheel opens it and an `Esc` closes it.  A user pressing `Ctrl+T`
  themselves leaves the flag stale, costing one redundant `Ctrl+T` on
  the next scroll.  Cheap to fix if it turns out to matter; not worth
  a key-sniffing rule before then.

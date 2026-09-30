# Marspot

A macOS terminal written in Rust, for people who keep a lot of sessions
open and run coding agents in most of them.

Metal for drawing, AppKit through Rust bindings, one process per pane.
No Swift, no Xcode project — `cargo build` produces the whole thing.

> **Status: pre-1.0.** It is used daily by its authors and is not yet
> packaged for anyone else. Expect missing terminal features (see
> [Known gaps](#known-gaps)) and no upgrade path between builds.

## Why another terminal

Two things it is built around, both measurable:

**It stays cheap when you keep 20 panes open.** Each pane is its own
process that owns its PTY; the window process draws, and nothing
animates when nothing changes. Idle panes cost close to nothing, and a
pane that crashes does not take the window with it.

**It treats a coding agent as a first-class program, not as text.**
Marspot knows which agent is running in which pane, what it is doing
right now, and what it costs — and it can act on that: switch an
account that has run out of quota and pick the conversation back up,
hand a session from one agent to another, park a pane that has been
idle and bring it back on a keystroke.

## Requirements

macOS 14.0 or newer, Apple silicon.

## Building

```sh
cargo build --release
```

Three binaries come out: `marspot` (the app), `marspot-core` (renderer)
and `marspot-session` (one per pane). `mcli` is a single-session
companion.

## Performance

Measured 2026-09-29 on an idle Mac mini (M-series, macOS 26.5), every
terminal driven through its own GUI in one sequential cycle so each
measurement is that terminal against an otherwise quiet machine.
Throughput of `cat`-ing a prepared byte stream, MB/s, higher is better:

| stream | marspot | Ghostty 1.3.1 | iTerm2 3.7.3 | Terminal 2.15 |
|---|---:|---:|---:|---:|
| ASCII | **128.0** | 97.0 | 84.2 | 43.2 |
| mixed | **128.0** | 88.9 | 24.1 | 36.0 |
| CJK | **133.3** | 118.5 | 10.0 | 35.6 |
| emoji | **128.0** | 110.3 | 0.5 | 38.6 |

Read that table with its limits in mind, which are real:

- It measures how fast a terminal reads and parses a PTY, nothing else.
  It says nothing about frame rate or input latency. A terminal that
  coalesces frames does less drawing work here, and that is a
  legitimate design choice rather than cheating.
- One run, one machine, one day. Run-to-run repeatability has not been
  established, and three of the four numbers landing on exactly 128.0
  says the timer's resolution is the limiting factor at these speeds.

The harness is in `bench/` and `bin/bench.sh`. A benchmark you cannot
re-run is a marketing claim, so the intent is that you re-run it.

## Keys

Everything not listed goes to the program in the pane, unchanged.

| | |
|---|---|
| `⌘N` | New window |
| `⌘C` | Copy the selection |
| `⌘V` | Paste |
| `⌘F` | Search this pane's scrollback |
| `⌘B` | Show or hide the sidebar |
| `⇧⌘C` | Agent usage for this account |
| `⌘W` | Close the panel that is open |
| `Esc` | Close the panel that is open; three times in five seconds ends a stuck agent session |

## Known gaps

Honest list, because you will hit these in the first hour:

- One cell holds one code point, so combining marks, ZWJ emoji and flag
  sequences collapse to their first code point.
- No OSC 8 (hyperlinks), OSC 7 (working directory) or OSC 133 (prompt
  marks).  OSC 52 puts text on the clipboard; reading it back is
  refused on purpose, because answering hands any program that can
  write to the pty whatever you last copied.
- `ESC ( 0` line drawing is not translated, so `dialog`-style TUIs draw
  letters instead of box characters.
- There is no terminfo entry of its own; `TERM` is set to
  `xterm-256color`, which claims a few capabilities not yet
  implemented.
- The preferences window exposes a handful of settings. Fonts, themes
  and key bindings are not configurable yet — the keys listed above
  are the keys.

## Licence

Dual licensed under [Apache 2.0](LICENSE-APACHE) and
[MIT](LICENSE-MIT), at your option.

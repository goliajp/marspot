# Marspot

A macOS terminal written in Rust, for people who keep a lot of sessions
open and run coding agents in most of them.

Metal for drawing, AppKit through Rust bindings, one process per pane.
No Swift, no Xcode project — `cargo build` produces the whole thing.

> **Status: 0.6, pre-1.0.** Signed and notarised builds are on the
> [releases page](https://github.com/goliajp/marspot/releases/latest);
> an installed copy checks there and updates itself in place, and the
> panes keep running through the update. Expect missing terminal
> features (see [Known gaps](#known-gaps)).

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

macOS 15.0 or newer, Apple silicon. Intel Macs are not supported and
are not planned: the renderer targets Metal on Apple silicon and the
performance numbers below are what the design is for.

## Where it keeps things

Everything is under `~/Library/Application Support/marspot`:

| | |
|---|---|
| `sessions/<id>/` | one directory per pane: its scrollback, its byte log |
| `binaries/` | the version running now, and the one staged next |
| `settings.toml` | your settings, read live — edit it and the next sweep uses it |

An older version kept this under `~/Library/Caches/marspot`, and a
symlink is left there so anything that still computes the old path
finds the new one. An empty `logs/` may be left from that era too — the
log has not been written there since.

The log is where macOS keeps logs: `~/Library/Logs/Marspot/marspot.log`,
holding what every layer wrote, rotated and bounded.

The app itself is wherever you put it — `/Applications` when it came
from the disk image, `~/.local/Marspot.app` when it was installed from
a source checkout.

A source install may also leave one LaunchAgent,
`com.marspot.land-bundle`. It is armed only when an update has been
staged and the running app is holding the old binary open; it waits
for the app to quit, swaps the bundle, and reopens. Nothing is installed to keep the app running or to start it at
login.

## Uninstalling

```sh
launchctl bootout "gui/$(id -u)/com.marspot.land-bundle" 2>/dev/null
rm -f  ~/Library/LaunchAgents/com.marspot.land-bundle.plist
rm -rf ~/.local/Marspot.app
rm -rf ~/Library/Application\ Support/marspot ~/Library/Caches/marspot
rm -rf ~/Library/Logs/Marspot
```

That is all of it: no receipt in the package database, nothing in
`/usr/local`, and no line added to your shell's rc files. Panes are
child processes of the app and go when it does.

## Building

```sh
cargo build --release
```

Three of the binaries that come out are the terminal: `marspot-shell`
owns the window and supervises, `marspot-core` draws, and one
`marspot-session` runs per pane and owns that pane's pty — a pane that
crashes takes nothing else with it. `marspot-peer` carries messages
between the agent sessions running in panes. `marspot` opens no window:
it holds the headless benchmarks (`marspot --bench <mode>:<arg>`).

## Performance

Measured 2026-11-22 on a Mac mini at load 1.6 to 2.5 (M-series, macOS
27.2.1), every terminal driven through its own GUI, four rounds each; the
first round is a warm-up and each number is the median of the other
three. Throughput of `cat`-ing a prepared byte stream, MB/s, higher is
better:

| stream | marspot | Ghostty 1.4.0 | iTerm2 3.7.9 | Terminal 2.15 |
|---|---:|---:|---:|---:|
| ASCII | **302.7** | 282.2 | 81.8 | 38.5 |
| mixed | **268.6** | 255.5 | 21.6 | 37.3 |
| CJK | **289.3** | 269.1 | 9.5 | 36.0 |
| emoji | **297.2** | 279.1 | 0.4 | 39.7 |

Read that table with its limits in mind, which are real:

- It measures how fast a terminal reads and parses a PTY, nothing else.
  It says nothing about frame rate or input latency. A terminal that
  coalesces frames does less drawing work here, and that is a
  legitimate design choice rather than cheating.
- One machine, one day. Between rounds neither marspot nor Ghostty
  moved by more than about 2 %, and on every stream marspot's slowest
  round was faster than Ghostty's fastest. marspot and Ghostty ran in
  windows of the same 99 by 35 cells; iTerm2 and Terminal opened at
  their own default sizes.

The harness is in `bench/` and `bin/bench.sh`. A benchmark you cannot
re-run is a marketing claim, so the intent is that you re-run it.

## Keys

Everything not listed goes to the program in the pane, unchanged.

| | |
|---|---|
| `⌘,` | Settings… |
| `⌘H` | Hide Marspot |
| `⌥⌘H` | Hide Others |
| `⌘Q` | Quit Marspot |
| `⌘N` | New Window |
| `⌘T` | New Pane |
| `⌘W, Esc` | Close Panel |
| `⌘C` | Copy |
| `⌘V` | Paste |
| `⌘F` | Find |
| `⌘B` | Show or Hide Sidebar |
| `⇧⌘L` | Layout |
| `⇧⌘P` | Process Monitor |
| `⇧⌘C` | Agent Accounts |
| `⌃⌘F` | Enter or Exit Full Screen |
| `⌘M` | Minimize |
| `⌘[` | Previous Pane |
| `⌘]` | Next Pane |
| `⌘1 – ⌘9` | Pane 1 – 9 |
| `⌘/` | Keyboard Shortcuts |

The same commands are in the menu bar, and ⌘/ lists them in the app. Closing a pane ends its session, so no key does it. Esc pressed three times in five seconds ends a stuck agent session.

## Known gaps

Honest list, because you will hit these in the first hour:

- Combining marks, ZWJ emoji and flag sequences survive scrolling
  back and a restart, but not a change of window width: the lines
  already in history are re-cut to the new width from what is on
  disk, and a re-cut line shows each such cluster as its base
  character.
- OSC 8 links in history kept on disk survive scrolling back and a
  restart. Links still on screen are lost when the session is
  updated or restored from a snapshot, and so are links in history
  kept in memory. A change of window width turns the links already
  in history into plain text; links on screen keep working.
- OSC 8 links open only `http`, `https`, `file` and `mailto` targets.
  A link to anything else is shown as plain text, on purpose: the
  target comes from whatever program writes to the pty, and opening
  it hands it to whichever app claims the scheme.
- No OSC 7 (working directory; the pane's directory is read from its
  process instead). OSC 133 prompt marks
  are read and kept with the history, but nothing uses them yet:
  there is no key to jump to the previous command. OSC 52 puts text
  on the clipboard; reading it back is refused on purpose, because
  answering hands any program that can write to the pty whatever you
  last copied.
- There is no terminfo entry of its own. `TERM` is set to
  `xterm-256color`, whose entry claims a few capabilities not yet
  implemented — an entry of our own waits on an answer for what a
  remote host should use over ssh, because an entry nothing can look
  up is worse than borrowing one that is close.
- The settings panel offers the theme (dark, light, high contrast),
  the language and a handful of behaviours. Fonts, font size and key
  bindings are not configurable yet — the keys listed above are the
  keys.

## Licence

Dual licensed under [Apache 2.0](LICENSE-APACHE) and
[MIT](LICENSE-MIT), at your option.

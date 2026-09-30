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

macOS 14.0 or newer, Apple silicon. Intel Macs are not supported and
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

The app itself installs to `~/.local/Marspot.app`.

One LaunchAgent may exist: `com.marspot.land-bundle`. It is armed only
when an update has been staged and the running app is holding the old
binary open; it waits for the app to quit, swaps the bundle, and
reopens. Nothing is installed to keep the app running or to start it at
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

Five binaries come out. Three of them are the terminal: `marspot-shell`
owns the window and supervises, `marspot-core` draws, and one
`marspot-session` runs per pane and owns that pane's pty — a pane that
crashes takes nothing else with it. `mcli` is a single-session
companion, and `marspot` is a single-process build of the same engine,
useful for working on the renderer without the other two.

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

- Combining marks, ZWJ emoji and flag sequences survive scrolling
  back, but not a restart: the cluster is kept beside the row in
  memory, while the row itself goes to disk holding the base
  character.
- No OSC 8 (hyperlinks), OSC 7 (working directory) or OSC 133 (prompt
  marks).  OSC 52 puts text on the clipboard; reading it back is
  refused on purpose, because answering hands any program that can
  write to the pty whatever you last copied.
- There is no terminfo entry of its own. `TERM` is set to
  `xterm-256color`, whose entry claims a few capabilities not yet
  implemented — an entry of our own waits on an answer for what a
  remote host should use over ssh, because an entry nothing can look
  up is worse than borrowing one that is close.
- The preferences window exposes a handful of settings. Fonts, themes
  and key bindings are not configurable yet — the keys listed above
  are the keys.

## Licence

Dual licensed under [Apache 2.0](LICENSE-APACHE) and
[MIT](LICENSE-MIT), at your option.

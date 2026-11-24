# Changelog

Notable changes for people using Marspot.

Per-layer version numbers live in `version-vector.toml`.

## 0.13.0

- A helper can be viewed in the pane it works for: the helper panel's
  View here, or `marspot-peer pane place <helper> peek`, shows the helper's
  live screen in its parent's place, at the same size, to type into and
  scroll; the parent's screen, scroll position and half-typed line come
  back unchanged. Another program cannot switch what is shown while you
  type there.
- An optional list of helpers under their pane (Settings, off for now):
  each helper's task, time and context size in the style of Claude
  Code's own list of agents, taken from the bottom rows of the pane. A
  click or the arrow keys from an empty input box view a helper, `x`
  stops it.
- `pane.run` in a pane that has just opened no longer answers with the
  shell's start-up status, and a command interrupted with Ctrl-C or
  ended by a syntax error is answered with its own status at once.
- An idle Codex that went back to an older conversation with /resume
  gets its inbox again after Marspot updates itself.

## 0.12.0

- A pane whose session has shadow panes shows how many after its title
  (`+2`), and the sidebar's bottom row counts them all. Either opens a
  panel with a row per shadow pane: its name, its task, how long it has
  run, and buttons to move it to a window or close it. Under each
  session are the buttons that close all of them, and that block or
  allow pane control from the session's directory. A pane's badge menu
  opens the panel too.
- A helper can run in a Linux container (`isolation: container`, or
  `marspot-peer helper start --container`), which needs Docker: it can write its working directory and nothing else of the
  Mac. Claude, Gemini and Kimi can be run so; Codex cannot yet. The
  image is built the first time. Claude gets only an access token, from
  an account whose token outlives the helper's time budget.
- Closing a pane no longer leaves its session's files behind now and
  then. The session's history is deleted only when the pane was closed
  in Marspot: a hangup from anything else saves the screen and keeps it.
  Closing a window while one of its panes starts keeps that pane, and
  closing a pane whose session is lost moves its history to the recycle
  directory for 14 days.
- A request to a pane right after it was opened is no longer refused as
  if the pane were an older build.
- Waiting for a text in a pane finds it when a clear, the text and the
  next prompt come at once, and is not misled by full-screen programs.
- Helper messages: an approval prompt is told once, also over an update
  of the app; a result written during an update is delivered; a session
  that moves to another agent keeps its helpers.
- Memory used while a pane printed a lot is given back when it is quiet
  in two cases where part of it stayed.

## 0.11.0

- A session can start helpers: other Claude, Codex, Gemini or Kimi
  sessions that work in the background on a task it hands them, under
  an account picked by the room left on it. The result comes back to
  the session that asked as a message; nobody has to open or watch
  anything. A helper ends with its time budget (30 minutes unless
  asked otherwise), when it is stopped, or with the pane that started
  it. The MCP tools are `HelperStart`, `Helpers` and `HelperStop`; on
  the command line it is `marspot-peer helper`. Helpers run without
  asking for each step; `helpers.mode` in the settings file changes
  that. Every start, stop and answered first-run screen is in the
  control audit log.
- A pane opened by an agent is answered with the name it is listed
  under, and a pane just opened or just closed is right in the next
  list.
- What an agent reads from a pane no longer contains the status line
  that running a command there types.
- A command sent to a pane right after another one waits until a slow
  prompt is drawn, in shells that mark where their prompt ends.
- Codex no longer asks before it runs one of Marspot's own MCP tools.
  Marspot sets `default_tools_approval_mode = "approve"` in the
  `marspot` MCP server entry it keeps in each Codex `config.toml`,
  which a Codex helper needs to answer the session that started it. A
  mode already set there is kept, for helpers too; set `"prompt"` to be
  asked. With `"approve"`, in every Codex session of the account,
  `PaneRun` and the other pane tools run without a Codex prompt and
  outside Codex's sandbox.
- A Codex, Gemini or Kimi session takes messages from its helper, and a
  helper from the session that started it, also when one of the two
  runs without permission prompts and the other does not.
- A message an agent sends in its first seconds waits, up to 20
  seconds, until Marspot knows which pane it runs in.

## 0.10.0

- Agents in Marspot panes can drive other panes. The `marspot-peer mcp`
  server gives Codex, Gemini and Kimi six tools: list panes, open a
  pane, type into one, run a shell command and get its exit status,
  read a pane's screen or wait for text, a prompt, quiet or the end of
  a command, and resize, move or close a pane. A new pane opens in the
  background by default, with no window, and ends with the pane that
  opened it. What a pane shows is handed back marked as screen text,
  not as instructions. A Claude that Marspot starts (after an account
  switch, a handoff or a reclaim) gets the same tools; any agent can
  use the `marspot-peer pane` command line, which a new
  `marspot-panes` skill describes to all four agents.
- A background pane opened in its parent's directory no longer changes
  the parent's name from `spg` to `spg#3381`; the background pane is
  always called by its full name.
- In a zsh pane, `exec bash` followed at once by a command sent from
  another pane no longer garbles that command in the macOS bash.

## 0.9.0

- A session told that its account is about to run out of quota keeps
  working. The notice says to carry on and expect to be moved to
  another account at any moment. Before, it asked the session to wrap
  up, and sessions stopped mid-task. After the move, by quota, by
  rebalancing or by hand, a session that had the notice is told to
  pick up where it left off, unless it was waiting for you.
- A Claude session whose request for the Fable model is refused, its
  own or a background agent's, is moved in the same scan to an account
  that still has Fable quota and told which request to send again. The
  pane is remembered as needing Fable for 12 hours, also across a
  restart, and is no longer moved away to leave Fable to other panes.
  When no account has Fable left it stays where it is.
- Text pasted into a program that asked for bracketed paste can no
  longer end the paste early. Paste markers inside the text are
  removed before it is wrapped, so the rest of the text reaches the
  program as pasted text, not as typed keys. Before, a pasted
  `ESC[201~` made everything after it run as if typed.

## 0.8.0

- Links a program marks with OSC 8 can be clicked. A marked link wins
  over one guessed from the text's shape. Only http, https, file and
  mailto targets open; a file link must point at this machine.
- A program ringing the bell (BEL) makes the system alert sound. A
  window sounds at most once every 250 ms, so a stream of bells does
  not turn into a stream of sounds. Before, BEL made no sound.
- A pane scrolled back into its history shows a scroll bar on its
  right edge. Drag it, or click beside it, to move through the
  history. It is not drawn while the pane shows the latest output.

## 0.7.0

- The settings panel is 480 points wide and the keyboard shortcuts
  panel 720, the widths the other panels use. Before, they were 440
  and 640, and the two read as narrower than the rest.

## 0.6.2

- A Codex pane that answers a message from another session now has
  that answer sent back to the session that asked. Before, Codex
  answered in its own pane only, and the asker never heard back.

## 0.6.1

Fixes for panes that switch between accounts when quota runs out.

- A pane whose conversation was sent to the background, or forked, is
  moved to another account with that conversation, not the one it
  left. Before, the pane came back to the conversation as it stood at
  the moment it was sent on, and everything since was out of view.
- A pane is told once that its account is about to run out, and told
  again only after the account has come back and runs low a second
  time. Before, the same notice could arrive several times in a row.
- A pane resting on an account one step from running out moves to one
  with room, and no pane is moved onto such an account. Before, a pane
  could stop work on a nearly spent account and stay there for hours.
- A key typed into the last column of a line is no longer drawn before
  the program echoes it.

## 0.6.0

The first signed and notarised release.

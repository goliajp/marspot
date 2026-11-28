# Changelog

Notable changes for people using Marspot.

Per-layer version numbers live in `version-vector.toml`.

## 0.20.0

- The sidebar lists each pane's children under it, with a short code
  before every name. A row's menu can make a pane the child of another
  or a top-level pane again, and closing a pane that has children asks
  first. Panes nest two levels at most.
- "Replace with…", in a pane's right-click menu or its sidebar row's
  menu, puts a hidden pane, or a new shell, into the slot of the pane
  in use, and swaps the two.
- A pane counts as shown only when its window really draws it: a pane
  past a full grid is listed as hidden, and choosing it brings it into
  view next to the current pane.
- A pane in a minimized window has a hollow dot until the window comes
  back. View › Helpers (⇧⌘H) opens the helper panel at any time.
- After a restart, Claude Code and Codex panes that were idle are
  resumed on the account and model they were using, without anyone
  typing.
- A Codex pane is no longer handed into a new conversation when you
  switch its account and Marspot does not know its conversation: the
  switch is refused with a note on the badge. Codex panes started
  without the conversation on their command line are recognised again.
- When a Codex account has used up its weekly allowance, its panes move
  to another Codex account that still has allowance, at the next moment
  nothing is running in them, instead of spending paid credits. The
  setting sits in the settings panel's new Codex group; its other two
  choices do not move panes yet.
- The settings panel's switching row (steady, balanced, full use) now
  drives the account planner, which takes over account choices step by
  step after its checks pass, and steps back on its own if a switch
  goes wrong.
- Input sent by another agent wakes a pane whose agent was asleep.
- Unix socket paths longer than 104 bytes work.

## 0.19.0

- The settings panel chooses the terminal font family and size. The
  change reaches every open window at once, without a restart. Only
  monospaced faces already installed on the Mac are offered.
- Cmd-Up and Cmd-Down jump to the previous and next command prompt in
  a pane's history, for shells that mark their prompts (OSC 133). The
  prompt you land on is highlighted for a moment.
- Scrolling back through a pane while its program keeps printing no
  longer makes the view jump on the next scroll step.
- Before Marspot installs an agent CLI (Claude Code, Codex, Gemini or
  Kimi) for the first time, it asks above the focused pane; answer with
  Ctrl-Option-Y or Ctrl-Option-N. The settings panel keeps each answer,
  and a no also stops Marspot's own updates of that CLI (Claude Code
  and Kimi still update themselves).
- Marspot no longer changes the default model of every Claude Code
  account. Each Claude Code session it starts or restarts is given its
  model explicitly, `/model` typed into a Claude Code prompt is sent
  only after you confirm it with y, and a program cannot send `/model`
  into a Claude Code prompt.
- The accounts panel lists the usage resets Marspot can read (Codex
  reset credits today), and a reset is claimed from there with two
  presses.
- Usage readings, account balancing and account switches rely only on
  what Marspot collects itself; no outside service is needed.
- A pane can open in a window of its own (`marspot-peer pane open
  --new-window`, or `new_window` in the PaneOpen tool).
- `marspot-peer send --from NAME` lets a program outside any agent
  session message a pane, which sees it as from `external:NAME`.
- Quitting Marspot and opening it again right away no longer leaves a
  stray icon in the Dock.
- The +N in a pane's title and the helper rows under it count the same
  panes, and with the helper rows turned off the +N updates as soon as
  a pane's helpers change, even while the pane is idle.
- A working agent's badge ends with a static mark instead of a spinner
  that stopped moving.
- Output that arrives in small pieces while the Mac is busy costs less
  to draw.

## 0.18.0

- The helper rows under a Claude Code pane take only the rows they
  draw: under Claude's own subagent list they sit right below it with
  no empty rows. Each row names the agent and the model it runs on,
  shows the short title the caller gave the helper (HelperStart and
  `marspot-peer helper start` take `title`), and says `idle` when a
  helper has finished its turn and can be talked to again.
- An agent session can end processes of its own pane that it no longer
  needs, by pid and start time, with the `ProcStop` tool or
  `marspot-peer procs stop`; another pane's processes are refused and
  every stop is written to the control audit log.
- Quitting from the Dock right after an update or a reopen is no
  longer refused.
- The process panel shows real CPU percentages on Apple silicon (they
  read about 1/42 of the real value before).

## 0.17.0

- Claude Code sessions Marspot starts itself (account switches,
  helpers, handoffs) use Marspot's pane and helper tools without
  asking for permission each time. Your own Claude Code settings are
  not changed.
- With `madcawl-mcp` and `madcawl-shell` installed in `~/.local/bin`,
  every agent Marspot configures gets the madcawl page tools, and
  Marspot starts the page host on the first call and lets it go after
  five idle minutes.

## 0.16.0

- When the account a Claude Code pane runs on will not last
  two more hours at its current pace, its idle panes move to another
  account before they are refused; the slowest pane stays to use up
  what is left.
- Claude Code panes refused on their account, and clicks on the
  account badge, now choose the next account by how long each account
  can carry the pane and when it renews, keeping Fable for the panes
  that need it. Moving resting panes ahead of time to balance the
  accounts runs as a rehearsal for now: it is written to the log and
  nothing is moved (`claudecode.rebalance = dryrun`, the default; an
  older `on` reads as `dryrun`).
- New helpers spread over the accounts that have room, instead of all
  going to the one with the most left; a helper is not turned away
  when the accounts are short.
- Switching a Codex pane's account from its badge waits until the
  session may move and resumes the same conversation; while a turn is
  running it says why it does not switch.
- Codex panes are told ahead of time when their account is about to
  run out, and refusals are recorded; Codex panes are not moved
  automatically yet.
- Each Codex account's conversation write locks are shared, so one
  conversation cannot be written from two accounts at once; the
  change is made only while that account's Codex is not running.

## 0.15.0

- How many helpers can run at once now follows the machine's memory:
  as many idle helpers as fit in a quarter of it, from 4 on an 8 GB
  Mac to 24 on 48 GB or more (`control_shadows_total` in the settings
  still overrides it).
- The helper panel says on a helper's row when moving it to a window
  failed, and a helper moved to a window gets back the title its
  agent's plugin gave it.
- A helper working for a plain shell shows as stopped once it stops,
  instead of running until an agent starts in that pane.
- Codex helpers notify once per approval prompt even when the
  conversation above the prompt changes.
- The list of helpers under a Claude Code pane keeps its right edge
  when a task's own words look like a time or a status, and in narrow
  panes.
- Waiting for text in a pane is not fooled by long terminal-graphics
  or clipboard sequences, nor by sequences the terminal ignores.
- Closing panes no longer stalls the window while a session process
  takes its time to exit.

## 0.14.0

- The list of helpers under their pane is on by default. In a Claude
  Code pane it continues Claude's own list of agents, row for row in
  the same columns, glyphs and colours; it stays aligned when one of
  Claude's rows shows no time or tokens. Turn it off in Settings.
- Viewing a helper in a window with a single pane now says in the
  window's header which helper is shown and how to leave.
- Codex helpers can run in a container: they get only an access token,
  from an account whose token outlives the helper's time budget, and are
  started again with a renewed one when it is refused.
- Closing panes leaves no session process, no zombie process and no
  stray socket behind, and a closed pane does not come back as an empty
  session after the core restarts.
- Waiting for text in a pane no longer misses text printed just before a
  full-screen program starts, nor matches text drawn by one that has
  left.
- Helpers notify once per approval prompt across restarts, and follow
  the conversation their pane was handed to, not one after a `/clear`.

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

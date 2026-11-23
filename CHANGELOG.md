# Changelog

Notable changes for people using Marspot.

Per-layer version numbers live in `version-vector.toml`.

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

# Changelog

Notable changes for people using Marspot.

Per-layer version numbers live in `version-vector.toml`.

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

# Changelog

Notable changes for people using Marspot.

Per-layer version numbers live in `version-vector.toml`.

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

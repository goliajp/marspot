# RFC-009 — handing a pane's conversation between claude and codex

Status: shipped (2026-09-22).  Steps 1–7 below are done; see "Not
verified" for what the tests cannot reach.

## What it is

The badge menu of a claude pane offers `hand off to codex P<n>`, and
a codex pane's offers `hand off to claude P<n>`.  Picking one ends the agent in the pane
and starts the other one, which is told what happened so far.  The
user keeps talking in the same pane.

## What it is not

Neither agent's history file is written.  Both formats are private,
both change between versions (codex 0.155 moved its history into a
sqlite projection keyed by rollout byte offsets), and neither carries
over anyway: claude's `thinking` is signed, codex's `reasoning` is
encrypted, and each side's tool calls name tools the other does not
have.  A forged file would at best be read without the reasoning, and
at worst be read wrong.

So the handoff goes through each CLI's public front door: a new or
resumed session, and a message typed into it.  Only the *reading* of
history files is format-dependent, and a reader that meets a shape it
does not know produces a thinner document, never a broken session.

## The two rules the user set

1. **The handoff is the point**, and it must be short and must not
   start anything: the receiving agent restates where things stand and
   waits for the user.
2. **Switching back and forth must not leave garbage.**

## Design

### One session per tool per pane

Each pane has a ledger: the claude session uuid and the codex thread
id it has used, and for each, how far that history has already been
handed over (a byte offset — both files are append-only).

- First time a pane goes to a tool: start it fresh, hand over
  everything relevant.
- Every later time: `--resume` / `resume <id>` that same session and
  hand over only what happened on the other side since.

So a pane that goes back and forth fifty times still has exactly one
claude session and one codex thread, each keeping its own context,
reasoning and cache for the parts it did itself.

### Handoffs never nest

Every handoff message carries a marker, `[marspot handoff]`.  A turn
that starts with it (the message and the reply restating it) is left
out when that side is read later.  Without this, A→B→A would hand B's
copy of A's history back to A.

### What the document holds

Little, on purpose: the repository already records what was done, and
the next agent reads `git status` / `git log` better than any summary
of tool calls.  What the repository does not hold is the conversation,
so the document is the latest three exchanges — the user's words and
the agent's final reply, verbatim, quoted — plus the path of the full
history.  No tool calls, no reasoning, no file lists, no summaries.
~10 KB for a real claude session, ~1 KB for a real codex one.

Read from the later of the watermark or the last compaction (claude's
`compact_boundary`, codex's `compacted`), so a 1.1 GB rollout reads in
under 100 ms.  Records the harness writes into the user's slot — a
background task reporting in (`origin.kind = task-notification`), a
slash command's output, `<system-reminder>` blocks — are not the user.

### The message

One line: where the document is, what it came from, and the
instruction to restate the state and wait.  Delivered as a paste once
the new agent has drawn its first frame — not as a command-line
argument, which `PtyCommand` refuses to quote.

### Garbage, bounded

| thing                        | bound                                   |
|------------------------------|-----------------------------------------|
| agent sessions               | one per tool per pane, reused            |
| handoff documents            | one per pane, overwritten each switch    |
| ledger                       | one small file per pane                  |
| both, after the pane closes  | removed at the next switch anywhere      |

The one thing the pane's own ledger cannot bound is a session the user
starts by hand in between (a `/clear`, a plain `codex`); that is theirs,
and the next switch simply reads it from the top.

### Order of the script

Extract first, kill second.  The document is built on a background
thread (a 1 GB rollout is real) while the pane is held; the op waits
for it, and only then takes the source down.  If the extraction fails,
nothing has been killed.

    hold → await document → TERM source → type target command
         → await first frame → check target process → commit ledger
         → paste → Enter

The first frame is counted from the moment the command is typed, not
from when the process appears: an agent that paints everything in the
gap between the two would otherwise leave nothing to count.

### Found on the way: the bytelog had stopped tracking the pane

Both "has the first frame arrived" and L1's "is this pane quiet"
(`pane_status::pty_quiet`) read the size of the pane's bytelog.  Since
2026-08-19 the bytelog is written through a 64 KiB `AsyncWriter`
buffer, so its size only moved every 64 KiB — a small first frame
never arrived (the switch timed out; so would the idle-reclaim wake on
a session whose first frame is under 64 KiB), and a spinner at ~60 B/s
looked silent for about twenty minutes.  `AsyncWriter::hand_off` now
passes a partial buffer to the writer at the end of each burst when it
can do so without blocking or allocating, and L3 calls it after every
pump that read something.

## Roadmap

1. `pty_op`: an `AwaitJob` step — wait for work done on another
   thread, fail the run with its error.  (basic)
2. `handoff::json` — a small JSON reader; the rest of this module
   cannot be written against substring matching.  (cc)
3. `handoff::transcript` — the neutral turn model and the renderer.
   (cc)
4. `handoff::claude` / `handoff::codex` — read one history into turns,
   from a watermark or last compaction, skipping marked turns.  (cc)
5. `handoff::ledger` — per-pane state, pruned against the session
   registry.  (cc)
6. Wiring: menu rows in both plugins, the job, the op.  (cc)
7. `bin/test-agent-handoff.sh` — real pty, stand-in `claude` and
   `codex`, a fake `HOME` with history files: there and back twice,
   asserting resume-by-id, the paste, the skipped marker turn, one
   document, and cleanup.  (cc)

## Not verified

- Whether `codex resume <id>` under a different `CODEX_HOME` finds a
  thread written under another one.  `sessions/` is a shared symlink
  here and each profile's `state_5.sqlite` indexes 400 of the same
  threads, which suggests it does; the plugin checks the rollout file
  is reachable from the target home before choosing resume, and the
  real CLI is not in the test chain.

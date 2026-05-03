---
description: Run mars cross-terminal perf bench and summarize results
---

You are running the mars perf benchmark mechanism. The mechanism is documented in `docs/bench.md`; the runner is `bin/bench-run.sh`.

## What to do

The user invoked `/benchmark $ARGUMENTS`. Parse `$ARGUMENTS` to determine the run shape (defaults below if empty), then execute the steps.

**Argument parsing:**
- `--quick` (default in interactive use): use idle-9x's 60 s mode
- `--extended`: use idle-9x's 30 min soak mode (only if user asked)
- `--scenarios <list>`: comma-separated, restrict the matrix
- `--terminals <list>`: comma-separated, restrict terminals (mars / iterm / terminal / warp)
- bare scenario / terminal name: shorthand for `--scenarios <name>` / `--terminals <name>`

If `$ARGUMENTS` is empty, run `bin/bench-run.sh --quick` (full matrix, ~3 min).

## Execution

1. **Pre-flight checks**:
   - Confirm we are in a clean working tree state if the user wants to baseline; warn but proceed if dirty
   - Check that `iTerm2` and `Terminal.app` are accessible (just verify with `osascript -e 'tell application "X" to version'`)
   - Note the user's frontmost app at start — the bench mechanism preserves focus, but mention this so the user knows their windows / focus will be left as-is

2. **Run**:
   - Execute `bin/bench-run.sh` with the parsed arguments using the Bash tool
   - Stream the output. Don't wrap the whole bench in `2>&1 | tail -X` — if a scenario fails the user wants to see why

3. **Summarize**:
   - Read the most recent snapshot from `bench/results/<run-id>.json`
   - Build a Markdown comparison table per scenario (mars / Terminal.app / iTerm2 / Warp)
   - Highlight: mars's win ratio for each scenario, any FAIL gates from idle-9x, and any unexpected variance
   - If a previous timeseries row exists for the same scenario × terminal, compute and show the delta (e.g., "throughput 119 MiB/s, ↑3 % from last run")

4. **Window safety**:
   - The drivers use ID-tracked cleanup; you do **not** need to manually close iTerm2 / Terminal.app windows
   - If the bench terminates abnormally, run `bin/bench-run.sh` again or fall back to `osascript`-driven cleanup of windows whose tab title starts with `mars-bench-`
   - Never `pkill iTerm2` / `pkill Terminal` / `pkill Warp` — those are user apps with real work

## Output style

Lead with the headline number (e.g. "mars 1.8× faster than Terminal.app on multi-session-9x"), then the table, then any caveats. Don't paste the raw JSON — link to the snapshot path. If the user passed `--quick` and a metric was thin, say so.

If the bench produced no surprises (all numbers within ±10 % of the last run), keep the summary to ~5 lines.

## Important

- The mechanism is **persistent** infrastructure. If the user asks for new scenarios / terminals / metrics, **don't add them inline** in the slash command — extend `bin/scenarios/<id>.sh` or `bin/drivers/<term>.sh` per the recipe in `docs/bench.md` § 6.
- If a bench run reveals a regression that crosses the gate (e.g. mars's throughput on multi-session-9x drops below 100 MiB/s, or idle CPU exceeds 5 %), surface that loudly — the user wants to catch regressions early.

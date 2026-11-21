#!/bin/bash
# bin/_fake-agent-shell.sh — a pane's shell that runs bin/_fake-agent-tui.py
# as its foreground job, the way a login shell runs a coding agent: the
# agent gets a process group of its own and the terminal, and the shell
# waits underneath.  Set as SHELL by bin/test-awake-cost.sh;
# FAKE_AGENT_TICKS names the directory the agent counts its frames in.
set -m
python3 "$(dirname "$0")/_fake-agent-tui.py" "$FAKE_AGENT_TICKS"

---
name: marspot-profiles
description: Use when the user wants several accounts of Claude Code (claude), OpenAI Codex CLI (codex) or Gemini CLI (gemini) side by side on this machine — "set me up with several accounts", "add another account", "second login", "switch accounts when I run out of quota", claude2 / codex2 / gemini2 aliases, or a check of an existing ~/.<agent>-profile-N layout. Converts the default config directory into one shared directory plus one numbered directory per account, adds accounts, installs the shell aliases and audits the result.
---

# marspot-profiles

Several accounts of the same coding agent on one machine. Each account has its
own numbered directory holding only its login and identity; everything else is
a symlink into one shared directory, so sessions, settings and skills are the
same whichever account is in use. marspot switches a pane between accounts when
one runs out of quota, and it can only do that with this layout.

```
~/.claude-shared/        projects, sessions, history, settings, skills, ...
~/.claude-profile-1/     account 1: its own login state, symlinks for the rest
~/.claude-profile-2/     account 2
~/.claude                symlink to profile 1, so the bare `claude` still works

claudeN  = CLAUDE_CONFIG_DIR=~/.claude-profile-N claude
codexN   = CODEX_HOME=~/.codex-profile-N codex
geminiN  = GEMINI_CLI_HOME=~/.gemini-profile-N gemini
```

The same three pieces exist for `codex` and `gemini`. Gemini's config
directory is `.gemini` inside `GEMINI_CLI_HOME`, so its symlinks live in
`~/.gemini-profile-N/.gemini/` and `~/.gemini` points there.

All of it is done by `profiles.sh`, which sits next to this file. Run it with
`bash <directory of this file>/profiles.sh ...`. Do not reproduce its steps by
hand.

## Set up several accounts (first time)

For each agent the user asked for (`claude`, `codex`, `gemini`):

1. Ask the user to quit every running session of that agent, including the one
   you are running in if it is the same agent. `init` refuses while a process
   of that name is alive, because a live process writes into the directory that
   is about to move. If you are that agent, give the user the commands below to
   run in a plain terminal after quitting instead of running them yourself.
2. Show what will happen: `profiles.sh init <agent> --dry-run`.
3. Convert: `profiles.sh init <agent>`.
4. Install the aliases once: `profiles.sh aliases`.
5. Add one directory per further account: `profiles.sh add <agent>` (next free
   number) or `profiles.sh add <agent> 3`.
6. Check: `profiles.sh doctor` must end in `all clear`.

`init` does nothing when the layout is already there, so it is safe to run
again.

## Add another account (layout already exists)

`profiles.sh add <agent>`, then `profiles.sh doctor`. If `add` says the shared
directory does not exist, the machine was never converted: do the first-time
steps.

## What to tell the user afterwards

Logging in cannot be scripted. Tell the user, per new account:

- open a new shell, so the aliases exist
- Claude Code: run `claudeN`, then `/login`
- Codex: run `codexN login`
- Gemini: run `geminiN` and authenticate when it asks (`/auth`)

After `init`, account 1 differs by agent:

- Codex and Gemini: the login file moved with account 1, so `codex1` /
  `gemini1` and the bare command stay logged in.
- Claude Code: `claude1` needs `/login` once. Claude Code keeps its login in
  the macOS keychain under a name derived from the config directory path, so a
  login does not follow a directory to a new path. The bare `claude` keeps its
  old login.

A Gemini API key entered through `/auth` is stored in the keychain under a
fixed name and is therefore the same for every account; only Google sign-in
(`oauth_creds.json`) is per account. A user who wants a different API key per
account sets `GEMINI_API_KEY` for that account instead.

## Never

- Never copy or move a credential file (`auth.json`, `oauth_creds.json`,
  `.claude.json`) or a keychain entry from one account to another. Each
  account logs in itself.
- Never delete the original directory or the backup. `init` renames what it
  did not move to `~/.<agent>.backup-YYYYMMDD-HHMMSS`; removing it is the
  user's decision.
- Never rename or move a profile directory afterwards. For Claude Code that
  orphans its keychain login.
- Never replace a per-account file with a symlink into the shared directory,
  and never edit the alias block in the shell rc by hand: the aliases are
  derived from the directories that exist.

## What is shared

- Claude Code: memory file, settings, commands, agents, skills, rules, projects
  and sessions, prompt history, plugin cache and marketplaces. `history.jsonl`
  is shared on purpose, so that `--resume` finds a session whichever account
  started it. Per account: `.claude.json`, the list of installed plugins,
  caches and statistics.
- Codex: `AGENTS.md`, `config.toml`, sessions and their index, skills, rules,
  plugins, packages, caches. Per account: `auth.json`, `history.jsonl`, the
  local databases.
- Gemini: `GEMINI.md`, `settings.json`, commands, skills, agents, policies,
  extensions, sessions (`tmp/`) and checkpoints (`history/`). Per account:
  `oauth_creds.json`, `google_accounts.json`, the other token files,
  `installation_id`, `projects.json`.

## Verify

`profiles.sh doctor` checks every profile of every agent: the shared directory
exists, every shared entry is a symlink that resolves, per-account files are
real files, and the alias block is in the shell rc. It prints one line per
finding and exits non-zero when anything is wrong. Report its `problem:` lines
to the user as they are; fix only what the line names.

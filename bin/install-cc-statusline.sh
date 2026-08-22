#!/usr/bin/env bash
# Install marspot's status-line hook into Claude Code's settings.
#
# Why: the pane badge used to infer the model by tailing the session
# transcript, which names one only when an assistant turn completes or
# when `/model` prints its confirmation.  A switch made on a parked
# pane, or a `--resume` under a different profile, left the badge
# stating the previous model until the session next answered.  Claude
# Code's status line carries what claude itself currently believes and
# re-runs on state change (not on a timer), so this replaces the
# inference with a report.
#
# What it costs on screen: nothing.  The hook prints no output, and
# claude renders an empty status line as no line at all (verified
# against 2.1.239).
#
# Never clobbers: a config that already has a `statusLine` is left
# exactly as it is and reported.  To uninstall, delete the
# `"statusLine"` key.
set -euo pipefail

# Which binary the hook runs.
#
# It must be one that KNOWS `--cc-statusline`.  A shell old enough not
# to recognise the flag used to fall through to the GUI start and come
# up as a full supervisor — one window per render, with claude calling
# it.  The flag landed in 0.7.116, so that is the floor, and it is
# checked rather than assumed.  (0.7.117 additionally made *any*
# unrecognised option exit 2 instead of launching, which closes the
# same hole for whatever flag comes next.)
#
# The bundle binary is preferred: its path is stable and a cold launch
# refreshes it.  While an older bundle is still pinned open by the
# running app, `binaries/current/` holds the newer shell — which is
# the very binary the bundle would exec into anyway.
MIN_SHELL="0.7.116"
STATE="${MARSPOT_STATE_DIR:-$HOME/Library/Application Support/marspot}"

shell_version() {  # path → "X.Y.Z", empty if it will not say
  [[ -x "$1" ]] || return 0
  MARSPOT_NO_REDIRECT=1 "$1" --version 2>/dev/null |
    sed -n 's/^marspot-shell \([0-9][0-9.]*\).*/\1/p' | head -1
}

version_ge() {  # $1 >= $2
  [[ "$(printf '%s\n%s\n' "$2" "$1" | sort -t. -k1,1n -k2,2n -k3,3n | head -1)" == "$2" ]]
}

HOOK_BIN=""
for cand in "${MARSPOT_APP:-$HOME/.local/Marspot.app}/Contents/MacOS/marspot-shell" \
            "$STATE/binaries/current/marspot-shell"; do
  v="$(shell_version "$cand")"
  [[ -n "$v" ]] && version_ge "$v" "$MIN_SHELL" && { HOOK_BIN="$cand"; break; }
done

if [[ -z "$HOOK_BIN" ]]; then
  echo "    no marspot-shell >= $MIN_SHELL found — hook not installed"
  echo "    (the badge keeps reading the model out of the transcript)"
  exit 0
fi

# Quoted: claude runs the command through a shell, and the state
# root's path contains a space ("Application Support") — unquoted, it
# is simply never invoked (verified against 2.1.239, both ways).
HOOK="\"$HOOK_BIN\" --cc-statusline"

targets=()
for d in "$HOME/.claude" "$HOME"/.claude-profile-*; do
  [[ -d "$d" ]] || continue
  f="$d/settings.json"
  [[ -f "$f" ]] || continue
  # Profiles here symlink one shared settings.json; resolve so it is
  # edited once rather than once per profile.
  targets+=("$(python3 -c 'import os,sys;print(os.path.realpath(sys.argv[1]))' "$f")")
done

if [[ ${#targets[@]} -eq 0 ]]; then
  echo "no Claude Code settings.json found — nothing to do"
  exit 0
fi

printf '%s\n' "${targets[@]}" | sort -u | while read -r f; do
  python3 - "$f" "$HOOK" <<'PY'
import json, os, sys, tempfile

path, hook = sys.argv[1], sys.argv[2]
text = open(path, encoding="utf-8").read()
cfg = json.loads(text)

def write(out):
    json.loads(out)  # never leave a settings.json that will not parse
    d = os.path.dirname(path)
    fd, tmp = tempfile.mkstemp(dir=d)
    with os.fdopen(fd, "w", encoding="utf-8") as fh:
        fh.write(out)
    os.chmod(tmp, os.stat(path).st_mode & 0o7777)
    # Replace the real file: the profiles' settings.json are symlinks
    # to it and must stay symlinks.
    os.replace(tmp, path)


if "statusLine" in cfg:
    have = cfg["statusLine"].get("command", "")
    if "--cc-statusline" not in have:
        print(f"    {path}: has its own statusLine — left alone")
        print(f"      (to feed marspot too, chain: {hook})")
    elif have == hook:
        print(f"    {path}: already installed")
    else:
        # Ours, but naming a different binary — e.g. installed while
        # the bundle was still an old shell, and current/ has since
        # moved on.  Repoint rather than leave it aimed at whatever it
        # was aimed at.
        write(text.replace(json.dumps(have)[1:-1], json.dumps(hook)[1:-1], 1))
        print(f"    {path}: statusLine repointed → {hook}")
    sys.exit(0)

# Insert after the opening brace so the rest of the file — key order,
# indentation, anything a formatter cares about — survives byte for
# byte.  The result is parsed before it is written.
i = text.index("{")
block = '\n  "statusLine": { "type": "command", "command": ' + json.dumps(hook) + ' },' 
write(text[: i + 1] + block + text[i + 1 :])
print(f"    {path}: statusLine installed")
PY
done

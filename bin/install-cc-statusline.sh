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

HOOK="${MARSPOT_APP:-$HOME/.local/Marspot.app}/Contents/MacOS/marspot-shell --cc-statusline"

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

if "statusLine" in cfg:
    have = cfg["statusLine"].get("command", "")
    if "--cc-statusline" in have:
        print(f"    {path}: already installed")
    else:
        print(f"    {path}: has its own statusLine — left alone")
        print(f"      (to feed marspot too, chain: {hook})")
    sys.exit(0)

# Insert after the opening brace so the rest of the file — key order,
# indentation, anything a formatter cares about — survives byte for
# byte.  The result is parsed before it is written.
i = text.index("{")
block = f'\n  "statusLine": {{ "type": "command", "command": "{hook}" }},'
out = text[: i + 1] + block + text[i + 1 :]
json.loads(out)

d = os.path.dirname(path)
fd, tmp = tempfile.mkstemp(dir=d)
with os.fdopen(fd, "w", encoding="utf-8") as fh:
    fh.write(out)
os.chmod(tmp, os.stat(path).st_mode & 0o7777)
os.replace(tmp, path)
print(f"    {path}: statusLine installed")
PY
done

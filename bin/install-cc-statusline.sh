#!/usr/bin/env bash
# Register marspot's status-line hook with Claude Code.
#
#   install-cc-statusline.sh              install (opt-in)
#   install-cc-statusline.sh --refresh    only fix an already-installed
#                                         hook that names a stale binary
#   install-cc-statusline.sh --uninstall  remove it, restoring whatever
#                                         status line it replaced
#
# What it buys: the pane badge's model comes from claude itself instead
# of being inferred from the session transcript, which names a model
# only at turn boundaries and at `/model`.  Without the hook the badge
# still works — it reads the transcript, and the pane's own startup
# banner — it is just a turn behind in the cases the transcript is
# silent about.
#
# Why it is opt-in: this edits Claude Code's settings.json, which is
# somebody else's configuration.  `bin/install-local.sh` only ever runs
# `--refresh`, so installing marspot never silently changes it.
#
# If a status line is already configured, it is not taken away: the
# original command is moved into `--chain` and the hook runs it with
# the same payload, relaying its output.  `--uninstall` puts it back.
set -euo pipefail

MODE="${1:-install}"
case "$MODE" in
  install|--refresh|--uninstall) ;;
  *) echo "usage: $0 [--refresh|--uninstall]" >&2; exit 64 ;;
esac

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

if [[ -z "$HOOK_BIN" && "$MODE" != "--uninstall" ]]; then
  echo "    no marspot-shell >= $MIN_SHELL found — hook not installed"
  echo "    (the badge keeps reading the model from the transcript and banner)"
  exit 0
fi

targets=()
for d in "$HOME/.claude" "$HOME"/.claude-profile-*; do
  [[ -d "$d" ]] || continue
  f="$d/settings.json"
  [[ -f "$f" ]] || continue
  # Several profiles can symlink one shared settings.json; resolve so
  # it is edited once rather than once per profile.
  targets+=("$(python3 -c 'import os,sys;print(os.path.realpath(sys.argv[1]))' "$f")")
done

if [[ ${#targets[@]} -eq 0 ]]; then
  echo "    no Claude Code settings.json found — nothing to do"
  exit 0
fi

printf '%s\n' "${targets[@]}" | sort -u | while read -r f; do
  python3 - "$f" "$HOOK_BIN" "$MODE" <<'PY'
import json, os, shlex, sys, tempfile

path, hook_bin, mode = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(path, encoding="utf-8").read()
cfg = json.loads(text)
existing = cfg.get("statusLine", {}).get("command", "")
ours = "--cc-statusline" in existing


def command(chained=""):
    # Quoted: claude runs the command through a shell, and the state
    # root's path contains a space ("Application Support") — unquoted,
    # it is simply never invoked (verified against 2.1.239, both ways).
    c = f"{shlex.quote(hook_bin)} --cc-statusline"
    return f"{c} --chain {shlex.quote(chained)}" if chained else c


def chained_out_of(cmd):
    """The command our hook was told to run after itself, if any."""
    parts = shlex.split(cmd)
    return parts[parts.index("--chain") + 1] if "--chain" in parts else ""


def write(new_text):
    json.loads(new_text)  # never leave a settings.json that will not parse
    fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path))
    with os.fdopen(fd, "w", encoding="utf-8") as fh:
        fh.write(new_text)
    os.chmod(tmp, os.stat(path).st_mode & 0o7777)
    # Replace the real file: the profiles' settings.json are symlinks
    # to it and must stay symlinks.
    os.replace(tmp, path)


def set_command(new_cmd):
    write(text.replace(json.dumps(existing), json.dumps(new_cmd), 1))


def without_status_line(src):
    """`src` minus its `statusLine` member, leaving the rest verbatim.

    Cutting text rather than re-dumping the parsed object: this is a
    file people hand-edit, and a round trip through json.dumps would
    reflow every line of it to uninstall one key.
    """
    i = src.index('"statusLine"')
    # Walk the value to its matching brace.  The values in play here
    # are objects of short strings; a quote-aware scan is enough.
    j = src.index("{", src.index(":", i))
    depth, in_str, esc = 0, False, False
    while j < len(src):
        c = src[j]
        if in_str:
            in_str = not (c == '"' and not esc)
            esc = c == "\\" and not esc
        elif c == '"':
            in_str, esc = True, False
        elif c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                j += 1
                break
        j += 1
    # A member is only separable together with one of its commas.
    k = j
    while k < len(src) and src[k] in " \t":
        k += 1
    if src[k : k + 1] == ",":
        k += 1
    else:  # last member — the comma that joins it sits in front
        pre = i - 1
        while pre >= 0 and src[pre] in " \t\r\n":
            pre -= 1
        if src[pre : pre + 1] == ",":
            i = pre
    # Take the whole line when nothing else shares it — and take one
    # of the two newlines bracketing it, whichever one is there, so a
    # file that was written on a single line goes back to being one.
    bol = src.rfind("\n", 0, i) + 1
    if not src[bol:i].strip():
        i = bol
        if src[k : k + 1] == "\n":
            k += 1
        elif i > 0 and src[i - 1] == "\n":
            i -= 1
    return src[:i] + src[k:]


if mode == "--uninstall":
    if not ours:
        print(f"    {path}: no marspot hook")
    else:
        chained = chained_out_of(existing)
        if chained:
            set_command(chained)
            print(f"    {path}: removed, status line restored → {chained}")
        else:
            write(without_status_line(text))
            print(f"    {path}: removed")
    sys.exit(0)

if ours:
    want = command(chained_out_of(existing))
    if existing == want:
        print(f"    {path}: already installed")
    else:
        # Ours, but naming a different binary — e.g. installed while
        # the bundle was still an old shell, and current/ has since
        # moved on.
        set_command(want)
        print(f"    {path}: repointed → {hook_bin}")
    sys.exit(0)

if mode == "--refresh":
    # Nothing of ours here, and refresh does not install.  This is what
    # keeps `install-local.sh` from touching a configuration the user
    # never asked us to touch.
    sys.exit(0)

if existing:
    # Claude Code allows one status-line command.  Take over the slot
    # but keep running theirs, so nothing they wrote disappears.
    set_command(command(existing))
    print(f"    {path}: installed, chaining your status line → {existing}")
    sys.exit(0)

# Insert after the opening brace so the rest of the file — key order,
# indentation, anything a formatter cares about — survives byte for
# byte.  The result is parsed before it is written.
i = text.index("{")
block = '\n  "statusLine": { "type": "command", "command": ' + json.dumps(command()) + ' },'
write(text[: i + 1] + block + text[i + 1 :])
print(f"    {path}: installed")
PY
done

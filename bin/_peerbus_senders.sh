# Sourced by test-peerbus.sh: sending as a session, and the pane table.

# what a sender is answered, on the two paths a message takes
mcp_send() { # sending thread, recipient, text → the tool's answer; fails when the tool reports an error
  python3 - "$SB/bin/marspot-peer" "$1" "$2" "$3" <<'PY'
import json, subprocess, sys
req = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
    "name": "SendMessage", "arguments": {"to": sys.argv[3], "message": sys.argv[4]}, "_meta": {"threadId": sys.argv[2]}}}
out = subprocess.run([sys.argv[1], "mcp"], input=json.dumps(req) + "\n", capture_output=True, text=True).stdout
r = json.loads(out.splitlines()[0])["result"]
print(r["content"][0]["text"])
sys.exit(1 if r["isError"] else 0)
PY
}
hook_send() { # recipient → what the Claude Code SendMessage hook answers
  printf '{"hook_event_name":"PreToolUse","tool_name":"SendMessage","session_id":"x","tool_input":{"to":"%s","message":"hi"}}' "$1" \
    | "$SB/bin/marspot-peer" claude-hook
}
hook_field() { # hook answer, field of hookSpecificOutput ("updatedInput.to" for the rewritten recipient)
  python3 - "$1" "$2" <<'PY' 2>/dev/null
import json, sys
v = json.loads(sys.argv[1])["hookSpecificOutput"]
for k in sys.argv[2].split("."):
    v = v[k]
print(v)
PY
}
hook_to() { hook_field "$(hook_send "$1")" updatedInput.to; }
hook_refusal() { hook_field "$(hook_send "$1")" permissionDecisionReason; }

pane_entry() { # socket → name<TAB>code<TAB>base<TAB>agent, from marspot's pane table
  python3 - "$REGISTRY" "$1" <<'PY'
import glob, json, os, sys
for p in glob.glob(os.path.join(sys.argv[1], ".marspot-panes-*.json")):
    for e in json.load(open(p)).get("panes", []):
        if e["socket"] == sys.argv[2]:
            print("\t".join(str(e.get(k, "")) for k in ("name", "code", "base", "agent")))
PY
}

#!/usr/bin/env python3
"""What the recording gives a real kimi in place of marspot's helper:

  probe.py hook <log>   a hook: logs its stdin; for UserPromptSubmit
                        prints a context line when the prompt asks for
                        one, so the recording shows where kimi puts it
  probe.py mcp <log>    a stdio MCP server named like marspot's, with a
                        SendMessage tool, logging every request

Run by record.py only.
"""
import json
import sys

KIND, LOG = sys.argv[1], sys.argv[2]


def log(obj):
    with open(LOG, "a") as f:
        f.write(json.dumps(obj) + "\n")


def hook():
    raw = sys.stdin.read()
    log({"stdin": raw})
    try:
        v = json.loads(raw)
    except ValueError:
        return 0
    if v.get("hook_event_name") == "UserPromptSubmit" and "with context" in json.dumps(v.get("prompt")):
        print("PROBE-CONTEXT from the hook")
    return 0


TOOL = {"name": "SendMessage", "description": "Send a message to another agent session.",
        "inputSchema": {"type": "object", "properties": {"to": {"type": "string"}, "message": {"type": "string"}},
                        "required": ["to", "message"]}}


def mcp():
    for line in sys.stdin:
        try:
            req = json.loads(line)
        except ValueError:
            continue
        log(req)
        if "id" not in req:
            continue
        method = req.get("method")
        if method == "initialize":
            result = {"protocolVersion": req.get("params", {}).get("protocolVersion", "2025-06-18"),
                      "capabilities": {"tools": {}}, "serverInfo": {"name": "marspot", "version": "0"}}
        elif method == "tools/list":
            result = {"tools": [TOOL]}
        elif method == "tools/call":
            result = {"content": [{"type": "text", "text": "sent"}]}
        else:
            err = {"code": -32601, "message": "no such method"}
            sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": req["id"], "error": err}) + "\n")
            sys.stdout.flush()
            continue
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": result}) + "\n")
        sys.stdout.flush()
    return 0


sys.exit(hook() if KIND == "hook" else mcp())

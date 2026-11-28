"""The model side of a turn: one chat-completions request to the model
named by KIMI_MODEL_* (what the recorded kimi sent, streamed), read
into text and tool calls, or into the error kimi records."""
import json
import os
import urllib.error
import urllib.request

SYSTEM = "You are Kimi Code CLI, an interactive general AI agent running on a user's computer."


class Reply:
    def __init__(self, text="", calls=None, error=None):
        self.text, self.calls, self.error = text, calls or [], error


def _error(code, message, name, status=None):
    e = {"code": code, "message": message, "name": name, "retryable": False}
    if status is not None:
        e["details"] = {"requestId": None, "statusCode": status}
    return e


def complete(messages, tools, session_id):
    base = os.environ.get("KIMI_MODEL_BASE_URL", "").rstrip("/")
    if not base:
        return Reply(error=_error("provider.connection_error", "no model configured", "TypeError"))
    body = {"model": os.environ.get("KIMI_MODEL_NAME", ""), "messages": [{"role": "system", "content": SYSTEM}] + messages,
            "prompt_cache_key": session_id, "thinking": {"type": "enabled", "keep": "all"}, "stream": True,
            "stream_options": {"include_usage": True}}
    if tools:
        body["tools"] = tools
    req = urllib.request.Request(base + "/chat/completions", data=json.dumps(body).encode(), method="POST",
                                 headers={"Content-Type": "application/json",
                                          "Authorization": "Bearer " + os.environ.get("KIMI_MODEL_API_KEY", "")})
    try:
        resp = urllib.request.urlopen(req, timeout=600)
    except urllib.error.HTTPError as e:
        try:
            err = json.loads(e.read()).get("error", {})
        except ValueError:
            err = {}
        message = err.get("message", "HTTP %d" % e.code)
        if e.code == 429 and err.get("code") == "exceeded_current_quota_error":
            return Reply(error=_error("provider.api_error", message, "APIProviderQuotaExhaustedError", 429))
        return Reply(error=_error("provider.api_error", message, "APIProviderError", e.code))
    except (urllib.error.URLError, OSError):
        return Reply(error=_error("provider.connection_error", "fetch failed", "TypeError"))
    text, calls = "", {}
    with resp:
        for raw in resp:
            line = raw.decode("utf-8", "replace").strip()
            if not line.startswith("data:") or line == "data: [DONE]":
                continue
            for choice in json.loads(line[5:]).get("choices", []):
                delta = choice.get("delta", {})
                text += delta.get("content") or ""
                for c in delta.get("tool_calls") or []:
                    slot = calls.setdefault(c.get("index", 0), {"id": "", "name": "", "arguments": ""})
                    slot["id"] = c.get("id") or slot["id"]
                    fn = c.get("function", {})
                    slot["name"] += fn.get("name") or ""
                    slot["arguments"] += fn.get("arguments") or ""
    return Reply(text, [calls[i] for i in sorted(calls)])

"""One turn, written to the session's record in the order and the
shapes the recorded kimi wrote: the prompt hook's output in front of
the prompt, a step per model request, MCP tool calls and their results,
the Stop hook that may keep the turn going once, and how it ended."""
import datetime
import json
import uuid

import model
import peers
from wire import now_ms


def _prompt_id():
    return "msg_" + uuid.uuid4().hex[:26].upper()


def _content(parts):
    texts = [p["text"] for p in parts]
    return texts[0] if len(texts) == 1 else [{"type": "text", "text": t} for t in texts]


class Turn:
    def __init__(self, app, text):
        self.app, self.s, self.text = app, app.session, text
        self.id, self.started = app.turns, now_ms()
        self.prompt_id = _prompt_id()

    def payload(self, event, **more):
        base = {"hook_event_name": event, "session_id": self.s.id, "cwd": self.app.cwd, "client_type": "kimi_code_cli",
                "session_title": self.app.title or self.text}
        base.update(more)
        return base

    def step_event(self, key, step, uuid_, **fields):
        event = dict(fields, uuid=uuid_, turnId=str(self.id), step=step)
        rec = self.s.tpl.make("context.append_loop_event:" + key, event=event)
        if fields.get("finishReason") == "error":
            # a step that failed says only that
            rec["event"] = {"type": "step.end", "uuid": uuid_, "turnId": str(self.id), "step": step, "finishReason": "error"}
        self.s.write(rec)

    def run(self):
        s = self.s
        s.lock(self.app.pid)
        try:
            self._run()
        finally:
            s.unlock()

    def _run(self):
        app, s = self.app, self.s
        parts = []
        prompt = [{"type": "text", "text": self.text}]
        for _, out, _ in peers.run_hooks(app.home, "UserPromptSubmit", self.payload("UserPromptSubmit", prompt=prompt,
                                                                                      is_steer=False), app.cwd):
            if out.strip():
                parts.append({"type": "text", "meta": {"contentType": "text/xml", "source": "user prompt submit hook"},
                              "text": '<hook_result hook_event="UserPromptSubmit">\n%s\n</hook_result>' % out.strip()})
        parts += prompt
        s.write(s.tpl.make("turn.prompt", input=parts, promptId=self.prompt_id, turnId=self.id))
        message = {"role": "user", "content": parts, "toolCalls": [], "id": self.prompt_id, "origin": {"kind": "user"}}
        s.write(s.tpl.make("context.append_message", message=message))
        app.messages.append({"role": "user", "content": _content(parts)})
        app.post("user", self.text)
        step, continued = 0, False
        while True:
            step += 1
            step_uuid = str(uuid.uuid4())
            self.step_event("step.begin", step, step_uuid)
            r = model.complete(app.messages, app.tool_specs, s.id)
            if r.error:
                self.step_event("step.end", step, step_uuid, finishReason="error")
                return self.end("failed", r.error)
            usage = {"inputOther": 10, "output": 3, "inputCacheCreation": 0}
            s.write(s.tpl.make("usage.record", model=s.alias, usage=usage))
            if r.text:
                part = {"type": "text", "text": r.text}
                self.step_event("content.part", step, str(uuid.uuid4()), stepUuid=step_uuid, block=0, part=part,
                                startedAt=now_ms(), durationMs=0)
            if r.calls:
                self.tools(r, step, step_uuid)
                self.step_event("step.end", step, step_uuid, finishReason="tool_use", usage=usage)
                continue
            app.messages.append({"role": "assistant", "content": r.text})
            self.step_event("step.end", step, step_uuid, finishReason="end_turn", usage=usage)
            app.post("reply", r.text)
            more = [err for code, _, err in peers.run_hooks(app.home, "Stop", self.payload(
                "Stop", turn_id=self.id, step=step, first_step_of_turn=step == 1, finish_reason="completed",
                usage=usage, stop_hook_active=continued), app.cwd) if code == 2 and err.strip()]
            if more and not continued:
                continued = True
                text = "\n".join(m.strip() for m in more)
                message = {"role": "user", "content": [{"type": "text", "text": text}], "toolCalls": [],
                           "origin": {"kind": "system_trigger", "name": "stop_hook"}}
                s.write(s.tpl.make("context.append_message", message=message))
                app.messages.append({"role": "user", "content": text})
                continue
            return self.end("completed", None)

    def tools(self, r, step, step_uuid):
        app = self.app
        app.messages.append({"role": "assistant", "content": r.text or None, "tool_calls": [
            {"id": c["id"], "type": "function", "function": {"name": c["name"], "arguments": c["arguments"]}}
            for c in r.calls]})
        for c in r.calls:
            call_uuid = str(uuid.uuid4())
            try:
                args = json.loads(c["arguments"] or "{}")
            except ValueError:
                args = {}
            self.step_event("tool.call", step, call_uuid, stepUuid=step_uuid, toolCallId=c["id"], name=c["name"], args=args)
            _, server, tool = (c["name"].split("__", 2) + ["", ""])[:3]
            srv = next((x for x in app.servers if x.name == server), None)
            try:
                out = srv.call(tool, args) if srv else "no such tool: %s" % c["name"]
            except (OSError, ValueError, TimeoutError, EOFError, RuntimeError) as e:
                out = "tool error: %s" % e
            event = {"parentUuid": call_uuid, "toolCallId": c["id"], "result": {"output": out, "durationMs": 1}}
            rec = self.s.tpl.make("context.append_loop_event:tool.result")
            rec["event"] = dict(event, type="tool.result")
            self.s.write(rec)
            app.messages.append({"role": "tool", "tool_call_id": c["id"], "content": out})
            app.post("tool", (server, tool, args.get("to", ""), out))

    def end(self, reason, error):
        s = self.s
        fields = {"turnId": self.id, "reason": reason, "durationMs": now_ms() - self.started}
        rec = s.tpl.make("turn.ended", **fields)
        rec.pop("error", None)
        if error:
            rec["error"] = error
            rec.pop("replyModel", None)
        s.write(rec)
        finished = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds")
        s.write(s.tpl.make("prompt.completed", promptId=self.prompt_id, reason=reason,
                           finishedAt=finished.replace("+00:00", "Z")))
        if error:
            self.app.post("error", error)
        self.app.post("ended", now_ms() - self.started)

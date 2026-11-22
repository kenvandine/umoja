#!/usr/bin/env python3
"""A scripted OpenAI-compatible model for umoja-agents-test.

Real models answer differently every time; the agent tests need Hermes to
do the same thing on every run. This server plays a fixed script chosen
by a tag in the user's prompt and otherwise behaves like a chat
completions endpoint (streaming and not, tool calls included).

  [[write NAME TEXT]]   run `printf TEXT > /sandbox/workspace/NAME`, then
                        answer "Wrote NAME"
  [[fetch URL]]         run `curl -sS -m 20 URL | head -c 300`, then say
                        what came back
  [[danger]]            run `rm -rf /sandbox/workspace/umoja-test-old`
                        (Hermes flags recursive deletes: an approval)
  [[ask]]               ask the user a question (clarify), then echo the answer
  [[slow N]]            run `sleep N` (to test Stop and Pause)
  [[escape]]            try to reach the desktop: ls the host home, the
                        D-Bus socket, and a host port; report what worked

Usage: fake-model.py [--port 13399] [--log FILE]
Every request is appended to the log as one JSON line (for the report).
"""

import argparse
import json
import re
import sys
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODEL = "umoja-test-model"
LOG = None


def log(entry):
    if LOG:
        with open(LOG, "a", encoding="utf-8") as f:
            f.write(json.dumps(entry) + "\n")


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(p.get("text", "") for p in content if isinstance(p, dict))
    return ""


def tool_names(tools):
    names = set()
    for t in tools or []:
        fn = t.get("function", t)
        if fn.get("name"):
            names.add(fn["name"])
    return names


def plan(messages, tools):
    """Returns ("tool", name, args) or ("text", answer)."""
    user = ""
    for m in messages:
        if m.get("role") == "user":
            user = text_of(m.get("content"))
    results = [text_of(m.get("content")) for m in messages if m.get("role") == "tool"]
    last_tool = None
    for m in reversed(messages):
        if m.get("role") == "assistant" and m.get("tool_calls"):
            last_tool = m["tool_calls"][-1]["function"]["name"]
            break
    names = tool_names(tools)
    if not tools:
        # Side requests (titles, summaries) get plain text.
        return ("text", "Umoja test task")
    terminal = "terminal" if "terminal" in names or not names else sorted(n for n in names if "term" in n or "shell" in n or "exec" in n)[:1] or ["terminal"]
    terminal = terminal if isinstance(terminal, str) else terminal[0]

    def run(cmd):
        return ("tool", terminal, {"command": cmd})

    m = re.search(r"\[\[write (\S+) (.+?)\]\]", user)
    if m:
        if not results:
            name, body = m.group(1), m.group(2)
            return run("printf '%s\\n' " + shell(body) + " > /sandbox/workspace/" + name)
        return ("text", "Wrote %s in your Hermes folder." % m.group(1))
    m = re.search(r"\[\[fetch (\S+)\]\]", user)
    if m:
        if not results:
            return run("curl -sS -m 20 %s | head -c 300" % shell(m.group(1)))
        return ("text", "Fetched it. It said: " + results[-1][:200])
    if "[[danger]]" in user:
        if not results:
            return run("rm -rf /sandbox/workspace/umoja-test-old")
        return ("text", "Cleanup result: " + results[-1][:200])
    if "[[ask]]" in user:
        if "clarify" in names and last_tool != "clarify":
            return ("tool", "clarify", {"question": "Which colour should the report use?",
                                        "choices": ["Blue", "Green"]})
        return ("text", "You chose: " + (results[-1][:100] if results else "nothing"))
    m = re.search(r"\[\[slow (\d+)\]\]", user)
    if m:
        if not results:
            return run("sleep %s; echo done" % m.group(1))
        return ("text", "Finished waiting.")
    if "[[escape]]" in user:
        if not results:
            return run(
                "for p in /home /run/user /var/run/dbus/system_bus_socket /tmp/.X11-unix; do "
                "ls -d $p/* >/dev/null 2>&1 && echo \"SEE $p\" || echo \"NO $p\"; done; "
                "(curl -sS -m 5 http://host.openshell.internal:631/ >/dev/null 2>&1 && echo 'SEE cups') || echo 'NO cups'; "
                "echo HOME=$HOME; id")
        return ("text", "Escape check:\n" + results[-1][:600])
    return ("text", "Hello from the Umoja test model. I only follow [[tags]].")


def shell(s):
    return "'" + s.replace("'", "'\\''") + "'"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, code, obj):
        data = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            self.reply(200, {"object": "list", "data": [
                {"id": MODEL, "object": "model", "owned_by": "umoja", "context_length": 131072}]})
        elif self.path.rstrip("/").endswith("/health"):
            self.reply(200, {"status": "ok"})
        else:
            self.reply(404, {"error": "not found"})

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        try:
            body = json.loads(self.rfile.read(n) or b"{}")
        except ValueError:
            return self.reply(400, {"error": "bad json"})
        if not self.path.rstrip("/").endswith("/chat/completions"):
            return self.reply(404, {"error": "not found"})
        messages = body.get("messages", [])
        decision = plan(messages, body.get("tools"))
        log({"time": time.time(), "peer": self.client_address[0], "messages": len(messages),
             "last_user": next((text_of(m.get("content"))[:200] for m in reversed(messages)
                                if m.get("role") == "user"), ""),
             "tools": sorted(tool_names(body.get("tools"))), "decision": decision})
        cid = "chatcmpl-" + uuid.uuid4().hex[:12]
        if decision[0] == "tool":
            call = {"id": "call_" + uuid.uuid4().hex[:10], "type": "function",
                    "function": {"name": decision[1], "arguments": json.dumps(decision[2])}}
            message = {"role": "assistant", "content": None, "tool_calls": [call]}
            finish = "tool_calls"
        else:
            message = {"role": "assistant", "content": decision[1]}
            finish = "stop"
        usage = {"prompt_tokens": 50, "completion_tokens": 10, "total_tokens": 60}
        if not body.get("stream"):
            return self.reply(200, {"id": cid, "object": "chat.completion", "created": int(time.time()),
                                    "model": MODEL, "usage": usage,
                                    "choices": [{"index": 0, "message": message, "finish_reason": finish}]})
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()

        def chunk(delta, fin=None, with_usage=False):
            obj = {"id": cid, "object": "chat.completion.chunk", "created": int(time.time()), "model": MODEL,
                   "choices": [{"index": 0, "delta": delta, "finish_reason": fin}]}
            if with_usage:
                obj["usage"] = usage
            self.wfile.write(b"data: " + json.dumps(obj).encode() + b"\n\n")
            self.wfile.flush()

        chunk({"role": "assistant"})
        if decision[0] == "tool":
            c = message["tool_calls"][0]
            chunk({"tool_calls": [{"index": 0, "id": c["id"], "type": "function",
                                   "function": {"name": c["function"]["name"], "arguments": c["function"]["arguments"]}}]})
        else:
            for word in re.findall(r"\S+\s*", decision[1]):
                chunk({"content": word})
        chunk({}, finish, with_usage=True)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        self.close_connection = True


def main():
    global LOG
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=13399)
    ap.add_argument("--bind", default="127.0.0.1")
    ap.add_argument("--log")
    a = ap.parse_args()
    LOG = a.log
    srv = ThreadingHTTPServer((a.bind, a.port), Handler)
    print("fake model on %s:%d" % (a.bind, a.port), flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    sys.exit(main())

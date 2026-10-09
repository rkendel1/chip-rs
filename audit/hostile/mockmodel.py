"""An OpenAI-compatible chat-completions server whose replies are *scripted by the auditor*.

This is NOT a model. It plays an adversary: a model that is confidently wrong, obeys injected
text, repeats itself, or misbehaves on the wire, so that Chip's trust boundary can be attacked
deterministically. Results obtained with it say nothing about any real model's competence.
"""
import json, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Mock:
    def __init__(self, script, delay=0.0):
        # script: callable(n, text, request_json) -> str reply | ("http", status, body) | ("sleep", seconds, reply)
        self.script = script
        self.delay = delay
        self.requests = []   # {"n", "bytes", "text", "t"}
        self.lock = threading.Lock()
        mock = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                length = int(self.headers.get("content-length", 0))
                raw = self.rfile.read(length)
                req = json.loads(raw or b"{}")
                text = "\n".join(m.get("content", "") for m in req.get("messages", []) if isinstance(m.get("content"), str))
                with mock.lock:
                    n = len(mock.requests)
                    mock.requests.append({"n": n, "bytes": len(raw), "text": text, "t": time.time(), "req": req})
                out = mock.script(n, text, req)
                if mock.delay:
                    time.sleep(mock.delay)
                if isinstance(out, tuple) and out[0] == "http":
                    body = out[2].encode()
                    self.send_response(out[1]); self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body); return
                if isinstance(out, tuple) and out[0] == "sleep":
                    time.sleep(out[1]); out = out[2]
                body = json.dumps({
                    "id": f"audit-{n}",
                    "choices": [{"message": {"role": "assistant", "content": out}}],
                    "usage": {"prompt_tokens": max(1, len(raw) // 4), "completion_tokens": max(1, len(out) // 4)},
                }).encode()
                self.send_response(200); self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)

        class Quiet(ThreadingHTTPServer):
            def handle_error(self, request, client_address):  # a client that gave up is not an error here
                pass

        self.server = Quiet(("127.0.0.1", 0), H)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()


def call(capability, **inputs):
    d = {"decision": "request_capability", "capability": capability}
    if inputs:  # an `inputs` member is invalid for a capability that declares none
        d["inputs"] = inputs
    return json.dumps(d)


def complete(summary="done"):
    return json.dumps({"decision": "complete", "summary": summary})


def escalate(reason):
    return json.dumps({"decision": "escalate", "reason": reason})


def block(reason):
    return json.dumps({"decision": "block", "reason": reason})

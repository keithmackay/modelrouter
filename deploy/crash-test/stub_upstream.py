#!/usr/bin/env python3
"""A stand-in OpenAI-compatible upstream for the crash test.

Answers POST /v1/chat/completions after a short delay, streamed (SSE chunks
spread over STUB_STREAM_SECS) or not, with usage so the router prices the
call. Standard library only.

    stub_upstream.py <port>
"""
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DELAY = float(os.environ.get("STUB_DELAY_SECS", "0.2"))
STREAM_SECS = float(os.environ.get("STUB_STREAM_SECS", "2.0"))
CHUNKS = 10


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _json(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.startswith("/v1/models"):
            self._json(200, {"object": "list", "data": [{"id": "mock-model", "object": "model"}]})
        else:
            self._json(404, {"error": "not found"})

    def do_POST(self):
        length = int(self.headers.get("content-length") or 0)
        req = json.loads(self.rfile.read(length) or b"{}")
        model = req.get("model", "mock-model")
        usage = {"prompt_tokens": 12, "completion_tokens": CHUNKS, "total_tokens": 12 + CHUNKS}
        if not req.get("stream"):
            time.sleep(DELAY)
            self._json(200, {
                "id": "chatcmpl-stub", "object": "chat.completion", "created": int(time.time()),
                "model": model,
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": "ok " * CHUNKS}}],
                "usage": usage,
            })
            return
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()

        def send(obj):
            line = obj if isinstance(obj, str) else json.dumps(obj)
            payload = f"data: {line}\n\n".encode()
            self.wfile.write(f"{len(payload):x}\r\n".encode() + payload + b"\r\n")
            self.wfile.flush()

        try:
            for i in range(CHUNKS):
                time.sleep(STREAM_SECS / CHUNKS)
                send({"id": "chatcmpl-stub", "object": "chat.completion.chunk", "model": model,
                      "choices": [{"index": 0, "delta": {"content": "ok "}, "finish_reason": None}]})
            send({"id": "chatcmpl-stub", "object": "chat.completion.chunk", "model": model,
                  "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage})
            send("[DONE]")
            self.wfile.write(b"0\r\n\r\n")
        except (BrokenPipeError, ConnectionResetError):
            pass


class Server(ThreadingHTTPServer):
    daemon_threads = True

    def handle_error(self, request, client_address):
        pass  # a replica killed mid-request leaves broken pipes; expected here


if __name__ == "__main__":
    Server((os.environ.get("STUB_HOST", "127.0.0.1"), int(sys.argv[1])), Handler).serve_forever()

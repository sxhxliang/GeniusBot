#!/usr/bin/env python3
"""A tiny MCP server for tests.

stdio mode:  python3 fake_mcp_server.py
http mode:   python3 fake_mcp_server.py --http <port>   (streamable HTTP at /mcp)

Tools: echo(text) -> "echo: <text>", fail() -> isError, slow(seconds), env(name).
"""
import json
import os
import sys
import time

TOOLS = [
    {"name": "echo", "description": "Echo text back", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
    {"name": "fail", "description": "Always fails", "inputSchema": {"type": "object", "properties": {}}},
    {"name": "slow", "description": "Sleep", "inputSchema": {"type": "object", "properties": {"seconds": {"type": "number"}}}},
    {"name": "env", "description": "Read an environment variable", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}}},
]


def handle(msg):
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        return {"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": msg["params"]["protocolVersion"], "capabilities": {"tools": {"listChanged": True}}, "serverInfo": {"name": "fake", "version": "1.0"}, "instructions": "be nice"}}
    if method == "notifications/initialized" or (method or "").startswith("notifications/"):
        return None
    if method == "ping":
        return {"jsonrpc": "2.0", "id": mid, "result": {}}
    if method == "tools/list":
        cursor = (msg.get("params") or {}).get("cursor")
        if cursor is None:
            return {"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS[:2], "nextCursor": "page2"}}
        return {"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS[2:]}}
    if method == "tools/call":
        name = msg["params"]["name"]
        args = msg["params"].get("arguments") or {}
        if name == "echo":
            return {"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "echo: " + str(args.get("text", ""))}]}}
        if name == "fail":
            return {"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "boom"}], "isError": True}}
        if name == "slow":
            time.sleep(float(args.get("seconds", 1)))
            return {"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "woke up"}]}}
        if name == "env":
            return {"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": os.environ.get(args.get("name", ""), "<unset>")}]}}
        return {"jsonrpc": "2.0", "id": mid, "error": {"code": -32602, "message": "unknown tool " + name}}
    if mid is not None:
        return {"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "method not found"}}
    return None


def stdio_main():
    sys.stderr.write("fake mcp server up\n")
    sys.stderr.flush()
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        reply = handle(json.loads(line))
        if reply is not None:
            sys.stdout.write(json.dumps(reply) + "\n")
            sys.stdout.flush()


def http_main(port):
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *a):
            pass

        def do_POST(self):
            if self.path != "/mcp":
                self.send_response(404)
                self.end_headers()
                return
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            msg = json.loads(body)
            reply = handle(msg)
            if reply is None:
                self.send_response(202)
                self.send_header("Mcp-Session-Id", "sess-1")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            # tools/call answers come back as an SSE stream to exercise that path.
            if msg.get("method") == "tools/call":
                payload = ("event: message\ndata: " + json.dumps(reply) + "\n\n").encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Mcp-Session-Id", "sess-1")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
                return
            payload = json.dumps(reply).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Mcp-Session-Id", "sess-1")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def do_DELETE(self):
            self.send_response(204)
            self.send_header("Content-Length", "0")
            self.end_headers()

    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()


if __name__ == "__main__":
    if len(sys.argv) >= 3 and sys.argv[1] == "--http":
        http_main(int(sys.argv[2]))
    else:
        stdio_main()

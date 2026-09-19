#!/usr/bin/env python3
"""Local smoke fixture for gitcask's JSON introspection contract; never a token issuer."""

import hmac
import json
import os
from pathlib import Path
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass  # Do not log credentials or request bodies, even in the fixture.

    def do_POST(self):
        supplied = self.headers.get("Authorization", "")
        expected = "Bearer " + os.environ["GITCASK_SMOKE_INTROSPECT_SECRET"]
        if not hmac.compare_digest(supplied, expected):
            self.send_response(401)
            self.end_headers()
            return
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        token = body.get("token")
        active = token in ("read", "write", "admin") and not (
            token == "read" and Path(sys.argv[2]).exists()
        )
        answer = {"active": active}
        if active:
            answer.update(principal="smoke:" + token, scopes=["smoke/*:" + token], ttl=30)
        data = json.dumps(answer).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
    Path(sys.argv[1]).write_text(f"http://127.0.0.1:{server.server_port}/introspect")
    server.serve_forever()

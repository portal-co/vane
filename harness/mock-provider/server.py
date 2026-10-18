#!/usr/bin/env python3
"""Mock LLM provider for the Pi gate (PLAN_PI_IN_VM §7 stage 5, Q3).

Replayable recorded session: serves request/response entries from a JSONL
recording in order, matched on (method, path). Streaming (SSE) bodies are
stored and replayed verbatim with their recorded content-type.

Modes:
  replay (default): serve recordings/<name>.jsonl in order; a mismatch or
                    exhausted recording answers 409 with diagnostics.
  record: proxy to a real upstream, appending each exchange to the
          recording so it can be replayed offline later.

Recording format (JSONL, one entry per exchange):
  {"request":  {"method", "path", "headers": {...}, "body": <json|null|string>},
   "response": {"status", "headers": {...}, "body": <json|null|string>}}

Usage:
  server.py [--port N] [--recording NAME]
  server.py --record https://api.anthropic.com --recording NAME [--port N]

Bound to 127.0.0.1; the guest reaches it through the tunnel via a static
--net-map entry (e.g. 10.0.2.120=127.0.0.1) or, for HTTPS-shaped provider
config, through the MITM mode (P6).
"""
import argparse
import json
import sys
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

RECORDINGS = Path(__file__).parent / "recordings"


def load_recording(name: str):
    path = RECORDINGS / f"{name}.jsonl"
    entries = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                entries.append(json.loads(line))
    return entries


class Server(ThreadingHTTPServer):
    """ThreadingHTTPServer carrying the mock's mutable state."""

    state: dict


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):
        sys.stderr.write(f"mock-provider: {format % args}\n")

    def _state(self) -> dict:
        # BaseRequestHandler.server is typed BaseServer; ours carries state.
        srv = self.server
        assert isinstance(srv, Server)
        return srv.state

    def _handle(self):
        state = self._state()
        length = int(self.headers.get("Content-Length", 0) or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            body = json.loads(raw) if raw else None
        except json.JSONDecodeError:
            body = raw.decode("utf-8", "replace")

        if state["record"]:
            self._proxy_and_record(body, raw)
            return

        entries = state["entries"]
        pos = state["pos"]
        if pos >= len(entries):
            self._fail(409, f"recording exhausted at position {pos}")
            return
        entry = entries[pos]
        want = entry["request"]
        if want["method"] != self.command or want["path"] != self.path:
            self._fail(
                409,
                f"request mismatch at {pos}: want {want['method']} {want['path']}, "
                f"got {self.command} {self.path}",
            )
            return
        state["pos"] = pos + 1
        resp = entry["response"]
        body = resp.get("body")
        if isinstance(body, (dict, list)):
            payload = json.dumps(body).encode()
        elif body is None:
            payload = b""
        else:
            payload = str(body).encode()
        self.send_response(resp["status"])
        headers = dict(resp.get("headers") or {})
        headers.setdefault("Content-Length", str(len(payload)))
        headers.pop("Transfer-Encoding", None)  # we always send a length
        headers.pop("Connection", None)
        for k, v in headers.items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(payload)

    def _proxy_and_record(self, body, raw):
        upstream = self._state()["upstream"].rstrip("/")
        url = upstream + self.path
        # Record mode only ever proxies to http(s) provider endpoints —
        # enforce the scheme allowlist at the point of use.
        if urllib.parse.urlparse(url).scheme not in ("http", "https"):
            self._fail(400, f"refusing non-http(s) upstream URL: {url}")
            return
        headers = {k: v for k, v in self.headers.items() if k.lower() != "host"}
        try:
            req = urllib.request.Request(  # noqa: S310 — scheme allowlisted above
                url, data=raw or None, headers=headers, method=self.command
            )
            with urllib.request.urlopen(req) as r:  # noqa: S310 — scheme allowlisted above
                status, rheaders, payload = r.status, dict(r.headers), r.read()
        except urllib.error.HTTPError as e:
            status, rheaders, payload = e.code, dict(e.headers), e.read()
        entry = {
            "request": {
                "method": self.command,
                "path": self.path,
                "headers": {k: v for k, v in self.headers.items()
                            if k.lower() not in ("authorization", "x-api-key", "host")},
                "body": body,
            },
            "response": {"status": status, "headers": {}, "body": None},
        }
        try:
            entry["response"]["body"] = json.loads(payload)
            entry["response"]["headers"] = {"Content-Type": rheaders.get("Content-Type", "application/json")}
        except (json.JSONDecodeError, UnicodeDecodeError):
            # SSE streams and other text bodies: store verbatim.
            entry["response"]["body"] = payload.decode("utf-8", "replace")
            entry["response"]["headers"] = {"Content-Type": rheaders.get("Content-Type", "text/plain")}
        path = RECORDINGS / f"{self._state()['recording']}.jsonl"
        with open(path, "a") as f:
            f.write(json.dumps(entry) + "\n")
        self.send_response(status)
        h = dict(entry["response"]["headers"])
        h["Content-Length"] = str(len(payload))
        for k, v in h.items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(payload)

    def _fail(self, status, msg):
        payload = json.dumps({"error": msg}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)
        sys.stderr.write(f"mock-provider: FAIL {msg}\n")

    do_GET = _handle
    do_POST = _handle
    do_PUT = _handle
    do_DELETE = _handle


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=18181)
    ap.add_argument("--recording", default="default")
    ap.add_argument("--record", metavar="UPSTREAM", default=None)
    args = ap.parse_args()

    state = {
        "record": args.record is not None,
        "upstream": args.record,
        "recording": args.recording,
        "entries": [],
        "pos": 0,
    }
    if not state["record"]:
        state["entries"] = load_recording(args.recording)
        sys.stderr.write(
            f"mock-provider: replaying {len(state['entries'])} entries from {args.recording}.jsonl\n"
        )

    srv = Server(("127.0.0.1", args.port), Handler)
    srv.state = state
    print(f"PORT {args.port}", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()

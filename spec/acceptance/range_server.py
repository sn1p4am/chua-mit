#!/usr/bin/env python3
"""Controllable HTTP server for acceptance-testing a download engine.

This is part of the specification side: it defines the *environments* the engine
must behave correctly in, independently of how the engine is implemented. It
never imports or inspects engine code — the engine under test is driven as a
black box through its CLI.

The interesting modes are the misbehaving ones. A server that answers a ranged
request with `200 OK` and the whole file is not exotic; it is common enough that
a downloader which trusts it will silently corrupt files in the field. The
acceptance suite exists mainly to prove that this engine does not.

Usage:
    python range_server.py --port 8731 --size 8388608

Control endpoints (used by run.py):
    GET /_mode?m=<mode>   switch behaviour, returns the new mode
    GET /_sha256          sha256 of the payload, as hex
    GET /_requests        number of data requests served so far
"""

from __future__ import annotations

import argparse
import hashlib
import http.server
import socketserver
import sys
import threading
import time

PAYLOAD = b""

# Requests that arrived with a Range header, and those that did not.
STATS = {"ranged": 0, "plain": 0, "total": 0}
STATS_LOCK = threading.Lock()

MODE = "normal"

MODES = (
    "normal",        # honour Range with 206 + Content-Range
    "ignore-range",  # always 200 + the whole body, even when Range was asked
    "no-length",     # 200 with no Content-Length, close-delimited
    "short-body",    # promise Content-Length, send half of it, then close
    "server-error",  # always 500
    "rate-limited",  # always 429 with Retry-After: 1
    "missing",       # always 404
)


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    # Silence the default stderr access log; run.py reports what matters.
    def log_message(self, *args: object) -> None:  # noqa: D102
        pass

    def _control(self) -> bool:
        global MODE
        if self.path.startswith("/_mode"):
            _, _, query = self.path.partition("?")
            requested = ""
            for pair in query.split("&"):
                key, _, value = pair.partition("=")
                if key == "m":
                    requested = value
            if requested in MODES:
                MODE = requested
            body = MODE.encode()
            self._send(200, body, {"Content-Type": "text/plain"})
            return True
        if self.path == "/_sha256":
            body = hashlib.sha256(PAYLOAD).hexdigest().encode()
            self._send(200, body, {"Content-Type": "text/plain"})
            return True
        if self.path == "/_requests":
            with STATS_LOCK:
                body = f"{STATS['total']} {STATS['ranged']} {STATS['plain']}".encode()
            self._send(200, body, {"Content-Type": "text/plain"})
            return True
        return False

    def _send(self, status: int, body: bytes, headers: dict[str, str]) -> None:
        self.send_response(status)
        for key, value in headers.items():
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def _parse_range(self) -> tuple[int, int] | None:
        raw = self.headers.get("Range")
        if not raw or not raw.startswith("bytes="):
            return None
        spec = raw[len("bytes="):].split(",")[0].strip()
        first, _, last = spec.partition("-")
        try:
            start = int(first)
            end = int(last) if last else len(PAYLOAD) - 1
        except ValueError:
            return None
        if start > end or start >= len(PAYLOAD):
            return None
        return start, min(end, len(PAYLOAD) - 1)

    def do_HEAD(self) -> None:  # noqa: N802
        if self._control():
            return
        self._dispatch(body=False)

    def do_GET(self) -> None:  # noqa: N802
        if self._control():
            return
        self._dispatch(body=True)

    def _dispatch(self, body: bool) -> None:
        with STATS_LOCK:
            STATS["total"] += 1
            if self.headers.get("Range"):
                STATS["ranged"] += 1
            else:
                STATS["plain"] += 1

        if MODE == "server-error":
            self._send(500, b"nope", {})
            return
        if MODE == "rate-limited":
            self._send(429, b"slow down", {"Retry-After": "1"})
            return
        if MODE == "missing":
            self._send(404, b"gone", {})
            return

        want = self._parse_range()

        if MODE == "ignore-range":
            # The dangerous case: a range was asked for, the whole file comes
            # back with 200. The engine must refuse to write this.
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(len(PAYLOAD)))
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()
            if body:
                self.wfile.write(PAYLOAD)
            return

        if MODE == "no-length":
            # Close-delimited: no Content-Length at all.
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            if body:
                self.wfile.write(PAYLOAD)
            self.close_connection = True
            return

        if MODE == "short-body":
            promised = len(PAYLOAD)
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(promised))
            self.end_headers()
            if body:
                self.wfile.write(PAYLOAD[: promised // 2])
            self.close_connection = True
            return

        # normal
        if want is not None:
            start, end = want
            chunk = PAYLOAD[start : end + 1]
            self.send_response(206)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Accept-Ranges", "bytes")
            self.send_header(
                "Content-Range", f"bytes {start}-{end}/{len(PAYLOAD)}"
            )
            self.send_header("Content-Length", str(len(chunk)))
            self.end_headers()
            if body:
                self.wfile.write(chunk)
            return

        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(len(PAYLOAD)))
        self.end_headers()
        if body:
            self.wfile.write(PAYLOAD)


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def main() -> int:
    global PAYLOAD

    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=8731)
    parser.add_argument("--size", type=int, default=8 * 1024 * 1024)
    parser.add_argument("--mode", default="normal", choices=MODES)
    parser.add_argument("--seed-wait", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()

    global MODE
    MODE = args.mode

    # Deterministic payload, built fast. The obvious "chain SHA-256 one block at a
    # time" is unusably slow at the sizes the throughput baseline needs — 1 GiB
    # would take tens of millions of hashes. Instead derive one 1 MiB block and
    # repeat it: still deterministic, still incompressible-ish, and generated in
    # milliseconds. The server reports its own sha256, so the client's result is
    # compared against what was actually served either way.
    seed = hashlib.sha256(f"payload-{args.size}".encode()).digest()
    unit = (seed * ((1 << 20) // len(seed) + 1))[: 1 << 20]
    repeats = args.size // len(unit) + 1
    PAYLOAD = (unit * repeats)[: args.size]

    # A cheap integrity check that the generation is what we think it is.
    assert len(PAYLOAD) == args.size, "payload generation produced the wrong length"

    with Server(("127.0.0.1", args.port), Handler) as httpd:
        print(f"listening on http://127.0.0.1:{args.port}", flush=True)
        print(f"mode={MODE} size={len(PAYLOAD)}", flush=True)
        print(f"sha256={hashlib.sha256(PAYLOAD).hexdigest()}", flush=True)
        try:
            httpd.serve_forever()
        except KeyboardInterrupt:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())

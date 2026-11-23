#!/usr/bin/env python3
"""Loopback-only, read-only Host normalizer for Luna's Tailscale PWA."""

from __future__ import annotations

import argparse
from http.client import HTTPConnection, HTTPException
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit


REQUEST_HEADERS = (
    "Accept",
    "Accept-Language",
    "Cache-Control",
    "If-Modified-Since",
    "If-None-Match",
)
RESPONSE_HEADERS = (
    "Content-Type",
    "Content-Length",
    "Cache-Control",
    "ETag",
    "Last-Modified",
    "Expires",
    "Vary",
    "X-Content-Type-Options",
    "Referrer-Policy",
    "Content-Security-Policy",
)


# Keep in sync with `host_allowed` in src/pwa.rs: the monitor only answers
# loopback names and tailnet names on the HTTPS ports Luna manages.
TAILSCALE_PORTS = (443, 8443, 10000, 38444)


def host_allowed(authority: str | None) -> bool:
    """Accept only the Host values a legitimate client can send.

    The relay rewrites Host for the monitor, so without this check a page on
    any other name (DNS rebinding) could read the monitor through the relay.
    """
    if authority is None:
        return False
    authority = authority.strip()
    if not authority or "/" in authority or "@" in authority:
        return False
    host, separator, port_text = authority.rpartition(":")
    if separator and ":" not in host:
        if not port_text.isascii() or not port_text.isdigit() or int(port_text) > 65535:
            return False
        port = int(port_text)
    elif separator:
        return False
    else:
        host, port = authority, None
    host = host.rstrip(".").lower()
    if host in ("127.0.0.1", "localhost"):
        return True
    if (port if port is not None else 443) not in TAILSCALE_PORTS:
        return False
    if not host.endswith(".ts.net"):
        return False
    name = host[: -len(".ts.net")]
    return bool(name) and all(
        part and all(ch.isascii() and (ch.isalnum() or ch == "-") for ch in part)
        for part in name.split(".")
    )


class RelayHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "LunaPwaRelay"
    sys_version = ""

    def do_GET(self) -> None:
        self.forward()

    def do_HEAD(self) -> None:
        self.forward()

    def do_POST(self) -> None:
        self.reject_method()

    def do_PUT(self) -> None:
        self.reject_method()

    def do_DELETE(self) -> None:
        self.reject_method()

    def do_PATCH(self) -> None:
        self.reject_method()

    def reject_method(self) -> None:
        body = b"Read-only endpoint"
        self.send_response(405)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)
        self.close_connection = True

    def forward(self) -> None:
        parsed = urlsplit(self.path)
        if not self.path.startswith("/") or parsed.scheme or parsed.netloc:
            self.send_error(400)
            return

        if not host_allowed(self.headers.get("Host")):
            self.send_error(403)
            return

        upstream_host = self.server.upstream_host
        upstream_port = self.server.upstream_port
        headers = {
            name: value
            for name in REQUEST_HEADERS
            if (value := self.headers.get(name)) is not None
        }
        headers["Host"] = f"{upstream_host}:{upstream_port}"
        headers["Connection"] = "close"
        upstream = HTTPConnection(upstream_host, upstream_port, timeout=4)
        try:
            upstream.request(self.command, self.path, headers=headers)
            response = upstream.getresponse()
            self.send_response_only(response.status, response.reason)
            for name in RESPONSE_HEADERS:
                value = response.getheader(name)
                if value is not None:
                    self.send_header(name, value)
            self.send_header("Connection", "close")
            self.end_headers()
            if self.command != "HEAD" and response.status != 304:
                while chunk := response.read(64 * 1024):
                    self.wfile.write(chunk)
        except (OSError, HTTPException, ValueError):
            try:
                self.send_error(502)
            except OSError:
                pass
        finally:
            upstream.close()
            self.close_connection = True

    def log_message(self, _format: str, *_args: object) -> None:
        return


class RelayServer(ThreadingHTTPServer):
    allow_reuse_address = True
    daemon_threads = True
    request_queue_size = 16

    def __init__(self, listen_host: str, port: int, upstream_host: str, upstream_port: int):
        super().__init__((listen_host, port), RelayHandler)
        self.upstream_host = upstream_host
        self.upstream_port = upstream_port


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--listen", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=4783)
    parser.add_argument("--upstream", default="127.0.0.1")
    parser.add_argument("--upstream-port", type=int, default=4782)
    args = parser.parse_args()
    for port in (args.port, args.upstream_port):
        if not 1 <= port <= 65535:
            parser.error("ports must be between 1 and 65535")
    if args.listen != "127.0.0.1":
        parser.error("the relay must bind to loopback")

    # The monitor uses /proc comm for self-protection without reading argv.
    with open("/proc/self/comm", "w", encoding="ascii") as process_name:
        process_name.write("luna-pwa-relay")

    server = RelayServer(args.listen, args.port, args.upstream, args.upstream_port)
    try:
        server.serve_forever(poll_interval=0.25)
    finally:
        server.server_close()


if __name__ == "__main__":
    main()

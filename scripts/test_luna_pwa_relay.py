"""Run with: python3 -m unittest scripts/test_luna_pwa_relay.py"""

import importlib.util
import pathlib
import threading
import unittest
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

_spec = importlib.util.spec_from_file_location(
    "relay", pathlib.Path(__file__).with_name("luna-pwa-relay.py")
)
relay = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(relay)


class HostAllowlist(unittest.TestCase):
    def test_loopback_and_managed_tailnet_hosts_are_allowed(self):
        for host in (
            "localhost:4783",
            "127.0.0.1:4783",
            "luna.tailnet.ts.net:8443",
            "luna.tailnet.ts.net:10000",
            "luna.tailnet.ts.net:38444",
            "luna.tailnet.ts.net:443",
            "luna.tailnet.ts.net",
            "LUNA.Tailnet.TS.NET.:443",
        ):
            self.assertTrue(relay.host_allowed(host), host)

    def test_everything_else_is_refused(self):
        for host in (
            None,
            "",
            "evil.example",
            "evil.example:4783",
            "luna.tailnet.ts.net:4782",
            "luna.tailnet.ts.net.attacker.example:8443",
            "user@luna.tailnet.ts.net:8443",
            "luna.tailnet.ts.net/x",
            "luna.tailnet.ts.net:99999",
            "luna.tailnet.ts.net:http",
            ".ts.net:443",
            "a..ts.net:443",
            "::1",
            "[::1]:4783",
        ):
            self.assertFalse(relay.host_allowed(host), host)


class RebindingIsBlocked(unittest.TestCase):
    def test_a_foreign_host_never_reaches_the_monitor(self):
        reached = []

        class Upstream(BaseHTTPRequestHandler):
            def do_GET(self):
                reached.append(self.headers.get("Host"))
                self.send_response(200)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"ok")

            def log_message(self, *_):
                pass

        upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        server = relay.RelayServer("127.0.0.1", 0, "127.0.0.1", upstream.server_address[1])
        for s in (upstream, server):
            threading.Thread(target=s.serve_forever, daemon=True).start()
        try:
            port = server.server_address[1]

            def get(host):
                conn = HTTPConnection("127.0.0.1", port, timeout=3)
                conn.putrequest("GET", "/api/state", skip_host=True)
                conn.putheader("Host", host)
                conn.endheaders()
                status = conn.getresponse().status
                conn.close()
                return status

            self.assertEqual(get("evil.example:4783"), 403)
            self.assertEqual(reached, [])
            self.assertEqual(get("luna.tailnet.ts.net:38444"), 200)
            self.assertEqual(len(reached), 1)
        finally:
            for s in (upstream, server):
                s.shutdown()
                s.server_close()


if __name__ == "__main__":
    unittest.main()

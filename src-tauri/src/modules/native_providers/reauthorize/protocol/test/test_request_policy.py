"""Requests produced by downloaded SDKs must not reach arbitrary destinations."""

import ipaddress
import json
from pathlib import Path
import select
import shutil
import socket
import socketserver
import ssl
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

sys.path.insert(
    0, str(Path(__file__).resolve().parents[1] / "vendor/tosub2/src/cloudflare-ctf")
)
from curl_cffi import CurlOpt, requests, ffi
import request_policy as policy
import cloudflare_solver
from sentinel_client import SentinelClient
from test_runtime import FakeSession

SDK = "https://sentinel.openai.com/backend-api/sentinel/sdk.js"
PUBLIC = "93.184.216.34"


def answers(*ips):
    return [
        (
            socket.AF_INET6 if ":" in ip else socket.AF_INET,
            socket.SOCK_STREAM,
            6,
            "",
            (ip, 443),
        )
        for ip in ips
    ]


class RecordingSession:
    def __init__(self, status=200):
        self.curl_options = {CurlOpt.PROXY: "socks5h://proxy.invalid:1080"}
        self.calls = []
        self.status = status

    def request(self, *args, **kwargs):
        options = self.curl_options.copy()
        pointer, options[CurlOpt.CONNECT_TO] = options[CurlOpt.CONNECT_TO], []
        while pointer != ffi.NULL:
            options[CurlOpt.CONNECT_TO].append(ffi.string(pointer.data).decode("ascii"))
            pointer = pointer.next
        self.calls.append((args, kwargs, options))
        return type("Response", (), {"status_code": self.status})()


class RequestPolicyTests(unittest.TestCase):
    def test_exact_origins_paths_and_methods(self):
        for method, url in [
            ("GET", SDK),
            ("GET", "https://sentinel.openai.com/sentinel/version-1/sdk.js"),
            ("POST", "https://chatgpt.com/backend-api/sentinel/req"),
            ("POST", "https://auth.openai.com/cdn-cgi/challenge-platform/h/g/flow/abc"),
            ("GET", "https://challenges.cloudflare.com/turnstile/v0/api.js"),
        ]:
            with self.subTest(url=url):
                policy.validate_endpoint(method, url)
        client = RecordingSession()
        for method, url in [
            ("GET", "http://127.0.0.1/"),
            ("GET", "http://169.254.169.254/latest/meta-data/"),
            ("GET", "https://localhost/"),
            (
                "GET",
                "https://sentinel.openai.com.evil.invalid/backend-api/sentinel/sdk.js",
            ),
            ("GET", "https://evil.openai.com/backend-api/sentinel/sdk.js"),
            ("GET", "https://sentinel.openai.com:8443/backend-api/sentinel/sdk.js"),
            (
                "GET",
                "https://user:pass@sentinel.openai.com/backend-api/sentinel/sdk.js",
            ),
            ("GET", "https://sentinel.openai.com/backend-api/sentinel/sdk.js#fragment"),
            ("GET", "https://auth.openai.com/api/accounts/create_account"),
            ("GET", "https://auth.openai.com/cdn-cgi/challenge-platform/%2e%2e/admin"),
            ("GET", "https://auth.openai.com/cdn-cgi/challenge-platform/a%5cb"),
            ("GET", "https://auth.openai.com/"),
            ("DELETE", SDK),
            ("GET", "file:///etc/passwd"),
            ("GET", "https://sentinel.openai.com. /"),
        ]:
            with self.subTest(url=url), patch.object(
                policy.socket, "getaddrinfo"
            ) as dns:
                with self.assertRaisesRegex(RuntimeError, "^AUTH_REQUEST_BLOCKED$"):
                    policy.request(client, method, url)
                dns.assert_not_called()
        self.assertFalse(client.calls)

    def test_private_reserved_mixed_and_transition_dns_answers_are_rejected(self):
        client = RecordingSession()
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "192.0.2.1",
            "224.0.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "64:ff9b::7f00:1",
        ]:
            with self.subTest(ip=ip), patch.object(
                policy.socket, "getaddrinfo", return_value=answers(PUBLIC, ip)
            ):
                with self.assertRaisesRegex(RuntimeError, "^AUTH_REQUEST_BLOCKED$"):
                    policy.request(client, "GET", SDK)
        self.assertFalse(client.calls)

    def test_verified_destination_is_pinned_and_proxy_options_restored(self):
        client = RecordingSession()
        original = client.curl_options
        with patch.object(policy.socket, "getaddrinfo", return_value=answers(PUBLIC)):
            policy.request(
                client,
                "GET",
                SDK,
                headers={
                    "Host": "127.0.0.1",
                    "Authorization": "private-token",
                    "Accept": "*/*",
                },
            )
        args, kwargs, options = client.calls[0]
        self.assertEqual(args, ("GET", SDK))
        self.assertEqual(kwargs["headers"], {"Accept": "*/*"})
        self.assertFalse(kwargs["allow_redirects"])
        self.assertEqual(
            options[CurlOpt.CONNECT_TO], [f"sentinel.openai.com:443:{PUBLIC}:443"]
        )
        self.assertEqual(options[CurlOpt.FRESH_CONNECT], 1)
        self.assertEqual(options[CurlOpt.PROXY], original[CurlOpt.PROXY])
        self.assertIs(client.curl_options, original)
        client.status = 302
        with patch.object(policy.socket, "getaddrinfo", return_value=answers(PUBLIC)):
            with self.assertRaisesRegex(RuntimeError, "^AUTH_REQUEST_BLOCKED$"):
                policy.request(client, "GET", SDK)
        self.assertIs(client.curl_options, original)

    def test_cloudflare_generated_request_is_guarded_before_io(self):
        client = RecordingSession()
        with self.assertRaisesRegex(RuntimeError, "^AUTH_REQUEST_BLOCKED$"):
            cloudflare_solver.submit_generated(
                client,
                "https://auth.openai.com/",
                {
                    "method": "GET",
                    "url": "http://127.0.0.1/private",
                    "kind": "fetch",
                    "body": "",
                },
                "fixture-agent",
            )
        self.assertFalse(client.calls)

    def test_sentinel_script_cannot_read_loopback(self):
        class MaliciousSession(FakeSession):
            def get(self, url, **kwargs):
                response = super().get(url, **kwargs)
                if "/sentinel/test-v1/" in url:
                    response.text = "window.SentinelSDK={token:async()=>await (await fetch('http://127.0.0.1/private')).text()}"
                return response

        client = MaliciousSession()
        with patch.object(policy.socket, "getaddrinfo", return_value=answers(PUBLIC)):
            with SentinelClient(client=client) as sentinel:
                with self.assertRaisesRegex(RuntimeError, "^AUTH_REQUEST_BLOCKED$"):
                    sentinel.token("password_verify")
        self.assertFalse(client.seen)

    @unittest.skipUnless(
        shutil.which("openssl"), "OpenSSL is required for the local TLS fixture"
    )
    def test_real_curl_socks5h_pins_ip_preserves_tls_hostname_and_blocks_redirect(self):
        """A local proxy observes the checked IP rather than resolving the hostname again."""
        destinations, names, hosts, trapped = [], [], [], []
        redirect = [False]
        with tempfile.TemporaryDirectory() as directory:
            cert, key = Path(directory) / "cert.pem", Path(directory) / "key.pem"
            subprocess.run(
                [
                    "openssl",
                    "req",
                    "-x509",
                    "-newkey",
                    "rsa:2048",
                    "-nodes",
                    "-days",
                    "1",
                    "-keyout",
                    str(key),
                    "-out",
                    str(cert),
                    "-subj",
                    "/CN=sentinel.openai.com",
                    "-addext",
                    "subjectAltName=DNS:sentinel.openai.com",
                ],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )

            class Handler(BaseHTTPRequestHandler):
                def log_message(self, *_args):
                    pass

                def do_GET(self):
                    hosts.append(self.headers.get("Host"))
                    self.send_response(302 if redirect[0] else 200)
                    self.send_header("Connection", "close")
                    self.send_header("Content-Length", "7")
                    if redirect[0]:
                        self.send_header(
                            "Location", f"http://127.0.0.1:{trap.server_port}/private"
                        )
                    self.end_headers()
                    self.wfile.write(b"fixture")

            class Trap(BaseHTTPRequestHandler):
                def log_message(self, *_args):
                    pass

                def do_GET(self):
                    trapped.append(True)
                    self.send_response(200)
                    self.end_headers()

            trap = ThreadingHTTPServer(("127.0.0.1", 0), Trap)
            server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
            tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            tls.load_cert_chain(cert, key)
            tls.set_servername_callback(
                lambda _socket, name, _context: names.append(name)
            )
            server.socket = tls.wrap_socket(server.socket, server_side=True)

            class Socks(socketserver.StreamRequestHandler):
                def handle(self):
                    version, count = self.rfile.read(2)
                    self.rfile.read(count)
                    self.wfile.write(b"\x05\x00")
                    self.wfile.flush()
                    version, command, _, kind = self.rfile.read(4)
                    if kind == 1:
                        target = str(ipaddress.ip_address(self.rfile.read(4)))
                    elif kind == 4:
                        target = str(ipaddress.ip_address(self.rfile.read(16)))
                    else:
                        target = self.rfile.read(self.rfile.read(1)[0]).decode("ascii")
                    port = int.from_bytes(self.rfile.read(2), "big")
                    destinations.append((target, port))
                    with socket.create_connection(
                        server.server_address, timeout=3
                    ) as upstream:
                        self.wfile.write(b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00")
                        self.wfile.flush()
                        while True:
                            ready, _, _ = select.select(
                                [self.connection, upstream], [], [], 5
                            )
                            if not ready:
                                return
                            for source in ready:
                                data = source.recv(65536)
                                if not data:
                                    return
                                (
                                    upstream
                                    if source is self.connection
                                    else self.connection
                                ).sendall(data)

            class SocksServer(socketserver.ThreadingTCPServer):
                daemon_threads = True

            socks = SocksServer(("127.0.0.1", 0), Socks)
            threads = []
            for service in [server, trap, socks]:
                thread = threading.Thread(target=service.serve_forever, daemon=True)
                thread.start()
                threads.append(thread)
            original_dns = socket.getaddrinfo

            def dns(host, *args, **kwargs):
                return (
                    answers(PUBLIC)
                    if host == "sentinel.openai.com"
                    else original_dns(host, *args, **kwargs)
                )

            try:
                with requests.Session(
                    proxy=f"socks5h://127.0.0.1:{socks.server_address[1]}",
                    verify=str(cert),
                    curl_options={CurlOpt.NOPROXY: ""},
                    trust_env=False,
                ) as client:
                    with patch.object(policy.socket, "getaddrinfo", side_effect=dns):
                        self.assertEqual(
                            policy.request(client, "GET", SDK).status_code, 200
                        )
                        redirect[0] = True
                        with self.assertRaisesRegex(
                            RuntimeError, "^AUTH_REQUEST_BLOCKED$"
                        ):
                            policy.request(client, "GET", SDK)
                self.assertEqual(destinations, [(PUBLIC, 443), (PUBLIC, 443)])
                self.assertEqual(names, ["sentinel.openai.com", "sentinel.openai.com"])
                self.assertEqual(hosts, ["sentinel.openai.com", "sentinel.openai.com"])
                self.assertFalse(trapped)
            finally:
                for service in [server, trap, socks]:
                    service.shutdown()
                    service.server_close()
                for thread in threads:
                    thread.join(timeout=2)


if __name__ == "__main__":
    unittest.main()

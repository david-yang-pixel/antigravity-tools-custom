"""Exercise actual JS runtimes and OS pipe/file handling without login credentials."""
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

RUNTIME_DIR = Path(__file__).resolve().parents[1] / "vendor/tosub2/src/cloudflare-ctf"
sys.path.insert(0, str(RUNTIME_DIR))
from runtime_support import RuntimeEvents, launch_runtime, close_runtime
from sentinel_client import SentinelClient
import cloudflare_solver


class FakeResponse:
    def __init__(self, url, text):
        self.url, self.text, self.status_code = url, text, 200
        self.headers = {"content-type": "application/json"}


class FakeSession:
    def __init__(self):
        from curl_cffi.requests import Cookies
        self.cookies = Cookies()
        self.seen = []

    def get(self, url, **_kwargs):
        if url.endswith("/sdk.js") and "/sentinel/test-v1/" not in url:
            return FakeResponse(url, """
                const script = document.createElement('script');
                script.src = 'https://sentinel.openai.com/sentinel/test-v1/sdk.js';
                document.head.appendChild(script);
            """)
        return FakeResponse(url, """
            window.SentinelSDK = {token: async flow => {
                const response = await fetch('https://sentinel.openai.com/backend-api/sentinel/req',
                    {method:'POST', body:'{}'});
                const challenge = await response.json();
                return JSON.stringify({p:'测试-proof',t:null,c:challenge.token,id:'fixture-device',flow});
            }};
        """)

    def request(self, method, url, **kwargs):
        self.seen.append((method, url))
        return FakeResponse(url, json.dumps({"token": "fixture-challenge"}))


@unittest.skipUnless(shutil.which("node"), "Node.js is required")
class RuntimeTests(unittest.TestCase):
    def test_real_sentinel_sdk_runtime_reads_closed_input_and_cleans_it(self):
        session = FakeSession()
        with SentinelClient(client=session) as client:
            name = Path(client.input_file.name)
            self.assertTrue(name.is_file())
            # Reopen from a different process, not merely the Python process that wrote it.
            subprocess.run(["node", "-e", "JSON.parse(require('fs').readFileSync(process.argv[1],'utf8'))", str(name)], check=True)
            value = json.loads(client.token("password_verify"))
            self.assertEqual(value["p"], "测试-proof")
            self.assertEqual(value["c"], "fixture-challenge")
            self.assertEqual(session.seen, [("POST", "https://sentinel.openai.com/backend-api/sentinel/req")])
        self.assertFalse(name.exists())
        self.assertFalse(name.parent.exists())

    def test_real_cloudflare_parent_and_child_pipes_are_multiplexed(self):
        events = RuntimeEvents()
        runtimes = []
        try:
            for name, script, url in [
                ("parent", cloudflare_solver.PARENT_RUNTIME, "https://auth.openai.com/"),
                ("child", cloudflare_solver.CHILD_RUNTIME, "https://challenges.cloudflare.com/turnstile/v0/test"),
            ]:
                process, source = cloudflare_solver.start_runtime(script, {
                    "url": url, "parentUrl": "https://auth.openai.com/", "html": "<!doctype html><html><body></body></html>",
                    "userAgent": "Mozilla/5.0 Chrome/146.0.0.0",
                    "inlineScripts": [], "parentMessages": [], "waitMs": 50,
                }, "node")
                runtimes.append((process, source))
                events.register(name, process)
            finals = set()
            deadline = time.monotonic() + 10
            while len(finals) < 2 and time.monotonic() < deadline:
                event = events.read(0.2)
                if event and event[2] is not None:
                    message = json.loads(event[2])
                    if message["type"] == "final":
                        self.assertFalse(message["result"].get("errors"))
                        finals.add(event[0])
            self.assertEqual(finals, {"parent", "child"})
        finally:
            for process, source in runtimes:
                close_runtime(process)
                source.close()
            events.close()
        self.assertTrue(all(not Path(source.name).exists() for _, source in runtimes))

    def test_buffered_lines_eof_and_bounded_wait(self):
        with tempfile.TemporaryDirectory() as directory:
            script = Path(directory) / "runtime.cjs"
            script.write_text("process.stdout.write('one\\ntwo\\n');setTimeout(()=>process.exit(0),200);", encoding="utf-8")
            process, source = launch_runtime(script, {}, "node")
            events = RuntimeEvents()
            try:
                events.register("fixture", process)
                lines = []
                deadline = time.monotonic() + 5
                while events.sources and time.monotonic() < deadline:
                    event = events.read(0.1)
                    if event:
                        lines.append(event[2])
                self.assertEqual(lines, ["one\n", "two\n", None])
                at = time.monotonic()
                self.assertIsNone(events.read(0.02))
                self.assertLess(time.monotonic() - at, 1)
            finally:
                close_runtime(process)
                events.close()
                source.close()


if __name__ == "__main__":
    unittest.main()

"""Private runtime input and bounded pipe readers for all supported platforms."""
from __future__ import annotations

import json
import queue
import subprocess
import tempfile
import threading
from pathlib import Path
from typing import Any


class RuntimeInput:
    def __init__(self, value: dict[str, Any]):
        self.directory = tempfile.TemporaryDirectory(prefix="abv-auth-runtime-")
        self.name = str(Path(self.directory.name) / "input.json")
        try:
            # Close before Node opens the file: Windows does not share an open,
            # delete-on-close NamedTemporaryFile with the child process.
            with open(self.name, "x", encoding="utf-8") as handle:
                json.dump(value, handle, ensure_ascii=True)
        except BaseException:
            self.close()
            raise

    def close(self) -> None:
        self.directory.cleanup()


def launch_runtime(script: Path, value: dict[str, Any], node_command: str):
    source = RuntimeInput(value)
    try:
        process = subprocess.Popen(
            [node_command, str(script), source.name],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
    except BaseException:
        source.close()
        raise
    return process, source


class RuntimeEvents:
    """Reader threads support anonymous pipes on Windows and buffered lines on Unix."""
    MAX_LINE = 1024 * 1024

    def __init__(self):
        self.queue = queue.Queue(maxsize=16)
        self.stopped = threading.Event()
        self.sources: set[str] = set()
        self.threads: list[threading.Thread] = []

    def register(self, name: str, process: subprocess.Popen[str]) -> None:
        if process.stdout is None:
            raise RuntimeError("Authorization runtime stdout is unavailable")
        self.sources.add(name)

        def publish(line):
            while not self.stopped.is_set():
                try:
                    self.queue.put((name, process, line), timeout=0.1)
                    return
                except queue.Full:
                    pass

        def read():
            try:
                while not self.stopped.is_set():
                    line = process.stdout.readline(self.MAX_LINE + 1)
                    if not line:
                        break
                    if len(line) > self.MAX_LINE or not line.endswith("\n"):
                        publish(RuntimeError("Authorization runtime message exceeds its limit or is incomplete"))
                        break
                    publish(line)
            except (OSError, ValueError):
                pass
            finally:
                publish(None)

        thread = threading.Thread(target=read, daemon=True)
        self.threads.append(thread)
        thread.start()

    def unregister(self, name: str) -> None:
        self.sources.discard(name)

    def read(self, timeout: float):
        try:
            name, process, line = self.queue.get(timeout=max(0, timeout))
        except queue.Empty:
            return None
        if name not in self.sources:
            return None
        if isinstance(line, Exception):
            raise line
        if line is None:
            self.unregister(name)
        return name, process, line

    def close(self) -> None:
        self.stopped.set()
        for thread in self.threads:
            thread.join(timeout=1)


def close_runtime(process: subprocess.Popen[str]) -> None:
    if process.poll() is None:
        process.kill()
    process.wait(timeout=2)
    events = getattr(process, "_abv_events", None)
    if events is not None:
        events.close()
    for stream in (process.stdin, process.stdout, process.stderr):
        if stream is not None:
            stream.close()

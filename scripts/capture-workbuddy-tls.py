#!/usr/bin/env python3
"""Capture an independently verified WorkBuddy runtime against loopback only.

No application entry point, credentials, business API or native device SDK is used.
Keep the runtime's ICU and V8 snapshot files beside its executable.
"""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess


def vector(data, offset, width=2):
    size = int.from_bytes(data[offset:offset + width], "big")
    start = offset + width
    return data[start:start + size], start + size


def u16s(data):
    return [int.from_bytes(data[i:i + 2], "big") for i in range(0, len(data), 2)]


def profile(data):
    hello = data[4:]
    _, offset = vector(hello, 34, 1)
    ciphers, offset = vector(hello, offset)
    _, offset = vector(hello, offset, 1)
    extensions, _ = vector(hello, offset)
    result = {"ciphers": u16s(ciphers), "extensions": []}
    offset = 0
    while offset < len(extensions):
        kind = int.from_bytes(extensions[offset:offset + 2], "big")
        value, offset = vector(extensions, offset + 2)
        result["extensions"].append(kind)
        if kind in (10, 13):
            values, _ = vector(value, 0)
            result["groups" if kind == 10 else "signature_algorithms"] = u16s(values)
        elif kind == 16:
            values, _ = vector(value, 0)
            names, pos = [], 0
            while pos < len(values):
                name, pos = vector(values, pos, 1)
                names.append(name.decode("ascii"))
            result["alpn"] = names
        elif kind == 43:
            values, _ = vector(value, 0, 1)
            result["versions"] = u16s(values)
    return result


def read_exact(connection, size):
    result = bytearray()
    while len(result) < size:
        data = connection.recv(size - len(result))
        if not data:
            raise RuntimeError("Runtime closed before sending a complete ClientHello")
        result.extend(data)
    return bytes(result)


def capture(runtime, kind, environment):
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(10)
        port = listener.getsockname()[1]
        if kind == "https":
            code = ("require('https').get({hostname:'127.0.0.1',port:"
                    f"{port},servername:'localhost'"
                    "},()=>{}).on('error',()=>process.exit(0));")
        else:
            code = ("require('dns').setDefaultResultOrder('ipv4first');"
                    f"fetch('https://localhost:{port}/').catch(()=>process.exit(0));")
        with subprocess.Popen([runtime, "-e", code], env=environment,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as process:
            try:
                connection, _ = listener.accept()
                with connection:
                    connection.settimeout(5)
                    header = read_exact(connection, 5)
                    if header[0] != 22:
                        raise RuntimeError("Expected a TLS handshake record")
                    data = read_exact(connection, int.from_bytes(header[3:], "big"))
                process.wait(timeout=10)
                return profile(data)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("runtime", type=Path, help="Verified official WorkBuddy executable")
    args = parser.parse_args()
    runtime = str(args.runtime.resolve(strict=True))
    environment = dict(os.environ, ELECTRON_RUN_AS_NODE="1")
    versions = subprocess.check_output([runtime, "-p", "JSON.stringify(process.versions)"],
                                       env=environment, timeout=10, text=True)
    print(json.dumps({"runtime": json.loads(versions), "tls": {
        kind: capture(runtime, kind, environment) for kind in ("https", "fetch")
    }}, indent=2))


if __name__ == "__main__":
    main()

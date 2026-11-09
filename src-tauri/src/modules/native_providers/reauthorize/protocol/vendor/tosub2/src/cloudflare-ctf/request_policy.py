"""Network policy for requests produced by downloaded authorization scripts."""

from __future__ import annotations

import ipaddress
import queue
import re
import socket
import threading
from urllib.parse import unquote, urlsplit

from curl_cffi import CurlOpt, ffi, lib

OPENAI_HOSTS = {"auth.openai.com", "chatgpt.com", "sentinel.openai.com"}
CF_HOST = "challenges.cloudflare.com"
LOCK = threading.Lock()
FORBIDDEN_HEADERS = {
    "host",
    "authorization",
    "cookie",
    "proxy-authorization",
    "proxy-connection",
    "connection",
    "content-length",
    "transfer-encoding",
    "upgrade",
}
TRANSITION_NETWORKS = [
    ipaddress.ip_network(value)
    for value in (
        "64:ff9b::/96",
        "64:ff9b:1::/48",
        "2002::/16",
        "2001::/32",
    )
]


def blocked():
    # Never include the URL, response, headers or resolved address in an error.
    raise RuntimeError("AUTH_REQUEST_BLOCKED")


def validate_endpoint(method: str, url: str, purpose: str = "generated") -> str:
    try:
        if not isinstance(url, str) or re.search(r"[\x00-\x20\\\x7f]", url):
            blocked()
        target = urlsplit(url)
        host = target.hostname
        if (
            target.scheme != "https"
            or host not in OPENAI_HOSTS | {CF_HOST}
            or target.port not in (None, 443)
            or target.username is not None
            or target.password is not None
            or target.fragment
        ):
            blocked()
        path = target.path or "/"
        decoded = unquote(path)
        if (
            "\\" in decoded
            or re.search(r"[\x00-\x20\x7f]", decoded)
            or any(part in {".", ".."} for part in decoded.split("/"))
        ):
            blocked()
        # Exact origins and endpoint families, never arbitrary *.openai.com URLs.
        sentinel = host in OPENAI_HOSTS and (
            (
                method == "GET"
                and path
                in {"/backend-api/sentinel/sdk.js", "/backend-api/sentinel/frame.html"}
            )
            or (
                method == "POST"
                and path in {"/backend-api/sentinel/req", "/backend-api/sentinel/res"}
            )
            or (
                method == "GET"
                and host == "sentinel.openai.com"
                and re.fullmatch(
                    r"/sentinel/[A-Za-z0-9_-]+/(?:sdk\.js|frame\.html)", path
                )
            )
        )
        challenge = method in {"GET", "POST"} and re.fullmatch(
            r"/cdn-cgi/challenge-platform/[A-Za-z0-9_./~%:+,=\-]+", path
        )
        turnstile = (
            host == CF_HOST
            and method in {"GET", "POST"}
            and re.fullmatch(r"/turnstile/v0/[A-Za-z0-9_./~%:+,=\-]+", path)
        )
        verify = (
            purpose == "verification"
            and host in OPENAI_HOSTS
            and method == "GET"
            and path == "/"
        )
        if not (sentinel or challenge or turnstile or verify):
            blocked()
        return host
    except (ValueError, TypeError):
        blocked()


def public_address(host: str) -> str:
    answers = queue.Queue(maxsize=1)

    def resolve():
        try:
            answers.put(socket.getaddrinfo(host, 443, type=socket.SOCK_STREAM))
        except OSError:
            answers.put(None)

    threading.Thread(target=resolve, daemon=True).start()
    try:
        results = answers.get(timeout=5)
    except queue.Empty:
        raise RuntimeError("Could not resolve authorized host") from None
    if not results:
        raise RuntimeError("Could not resolve authorized host")
    addresses = []
    for _family, _type, _proto, _canon, sockaddr in results:
        try:
            if "%" in sockaddr[0]:
                blocked()
            address = ipaddress.ip_address(sockaddr[0])
        except ValueError:
            blocked()
        if not address.is_global or address.is_reserved or address.is_multicast:
            blocked()
        if address.version == 6 and (
            address.ipv4_mapped is not None
            or any(address in network for network in TRANSITION_NETWORKS)
        ):
            blocked()
        addresses.append(address)
    # Prefer IPv4 when both are available; many subscription exits lack IPv6.
    return str(sorted(addresses, key=lambda value: value.version)[0])


def request(
    client,
    method: str,
    url: str,
    *,
    headers=None,
    data=None,
    timeout=30,
    purpose="generated",
):
    method = method.upper()
    host = validate_endpoint(method, url, purpose)
    address = public_address(host)
    destination = f"[{address}]" if ":" in address else address
    clean_headers = {}
    for name, value in (headers or {}).items():
        name, value = str(name), str(value)
        if (
            name.lower() not in FORBIDDEN_HEADERS
            and re.fullmatch(r"[A-Za-z0-9!#$%&'*+.^_`|~-]+", name)
            and not re.search(r"[\x00-\x1f\x7f]", value)
        ):
            clean_headers[name] = value
    with LOCK:
        previous = getattr(client, "curl_options", {})
        # curl_cffi 0.15 handles Python lists for RESOLVE but not CONNECT_TO.
        # Keep an owned curl_slist alive through the synchronous transfer.
        connect_to = lib.curl_slist_append(
            ffi.NULL, f"{host}:443:{destination}:443".encode("ascii")
        )
        if connect_to == ffi.NULL:
            raise RuntimeError("Cannot prepare authorization connection")
        # CONNECT_TO pins the checked destination even through SOCKS5h or HTTP
        # CONNECT proxies. The URL hostname remains the TLS/SNI and Host identity.
        # A fresh connection prevents reuse of an earlier, unpinned socket.
        client.curl_options = {
            **previous,
            CurlOpt.CONNECT_TO: connect_to,
            CurlOpt.FRESH_CONNECT: 1,
        }
        try:
            response = client.request(
                method,
                url,
                headers=clean_headers,
                data=data,
                timeout=min(timeout, 30),
                allow_redirects=False,
            )
            if 300 <= response.status_code < 400:
                blocked()
            return response
        finally:
            client.curl_options = previous
            handle = getattr(client, "curl", None)
            if handle is not None:
                try:
                    handle.setopt(CurlOpt.CONNECT_TO, ffi.NULL)
                except Exception:
                    client.close()
            lib.curl_slist_free_all(connect_to)

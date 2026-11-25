#!/usr/bin/env python3
"""Exercise one-time pairing against the built CLI and an isolated HTTPS ingress."""

import argparse
from contextlib import contextmanager
import errno
import hashlib
from http.client import HTTPConnection, HTTPSConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import pty
import re
import secrets
import select
import socket
import sqlite3
import ssl
import subprocess
import tempfile
import threading
import time
from urllib.parse import urlsplit


ROUTES = {
    "muse": "/muse/mcp",
    "cue": "/cue/mcp",
    "codex-cloud": "/codex/mcp",
}
PUBLIC = {"/muse/pair", "/cue/pair", ROUTES["muse"], ROUTES["cue"]}
PROTOCOL = "2025-03-26"
HELLO = {"protocolVersion": PROTOCOL, "capabilities": {},
         "clientInfo": {"name": "pairing-fixture", "version": "1"}}


def private_file(path, data):
    path.write_text(data if isinstance(data, str) else json.dumps(data))
    path.chmod(0o600)


def run(binary, *args):
    subprocess.run([binary, *args], check=True, timeout=10,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def fixture(binary, root):
    home = root / "hub"
    run(binary, "quickstart", "--home", home)
    registry = home / "pigeonmq-cloud-registration.json"
    private_file(registry, {peer: {"mailbox": peer, "revoked": False} for peer in ROUTES})
    entries = []
    tokens = {}
    for peer, route in ROUTES.items():
        token = tokens[peer] = secrets.token_hex(32)
        digest = hashlib.sha256(token.encode()).hexdigest()
        path = home / ("pigeonmq-cloud-muse-policy.json" if peer == "muse" else f"fixture-{peer}.json")
        ops = ["read", "reply", "wait", "send"]
        if peer == "muse":
            policy = {"principal": peer, "mailbox": peer, "token_sha256": digest,
                      "operations": ops, "enabled": True,
                      "expires_at_ms": int(time.time() * 1000) + 600_000,
                      "registration": str(registry)}
        else:
            policy = {"principals": [{"id": peer, "token_sha256": digest,
                                     "operations": ops, "send_to": [],
                                     "mailboxes": [peer], "revoked": False}]}
        private_file(path, policy)
        entries.append({"adapter": peer, "route": route, "policy": str(path)})
    private_file(home / "fixture-muse.bearer", tokens["muse"])
    private_file(home / "profiles.json", {"profiles": entries})
    queue = home / "pigeonmq-cloud-queue.sqlite3"
    run(binary, "muse-prepare", "--queue", queue, "--auth-policy", home / "pigeonmq-cloud-muse-policy.json")
    return home, queue, tokens


def http(connection, path, payload, bearer=None):
    headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream",
               "Mcp-Protocol-Version": PROTOCOL}
    if bearer:
        headers["Authorization"] = "Bearer " + bearer
    try:
        connection.request("POST", path, json.dumps(payload), headers)
        response = connection.getresponse()
        body = response.read()
        try:
            value = json.loads(body)
        except ValueError:
            value = None
        return response.status, response.getheader("Cache-Control"), value
    finally:
        connection.close()


def mcp(connection, peer, bearer, method, params):
    status, _, response = http(connection, ROUTES[peer], {
        "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
    }, bearer)
    assert status == 200 and response and "error" not in response, "MCP request failed"
    result = response["result"]
    assert not result.get("isError"), "MCP tool failed"
    if method == "tools/call":
        return result.get("structuredContent") or json.loads(result["content"][0]["text"])
    return result


def tool(connection, peer, bearer, name, **arguments):
    return mcp(connection, peer, bearer, "tools/call", {"name": name, "arguments": arguments})


@contextmanager
def hub(binary, home, queue):
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    process = subprocess.Popen([
        binary, "serve-hub", "--queue", queue, "--profiles-file", home / "profiles.json",
        "--port", str(port), "--seconds", "90", "--stdio-socket",
    ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            assert process.poll() is None, "temporary hub stopped before it was ready"
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                    break
            except OSError:
                time.sleep(0.025)
        else:
            raise AssertionError("temporary hub was not ready")
        yield port
    finally:
        process.terminate()
        process.wait(timeout=10)


@contextmanager
def ingress(root, port):
    cert, key = (root / name for name in ("cert.pem", "key.pem"))
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                    "-days", "1", "-subj", "/CN=localhost", "-addext",
                    "subjectAltName=DNS:localhost", "-keyout", key, "-out", cert],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)

    class Proxy(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            if self.path not in PUBLIC:
                self.send_error(404)
                return
            size = int(self.headers["Content-Length"])
            assert size <= 16384
            origin = HTTPConnection("127.0.0.1", port, timeout=5)
            try:
                headers = {key: self.headers[key] for key in (
                    "Authorization", "Accept", "Content-Type", "Mcp-Protocol-Version",
                ) if key in self.headers}
                origin.request("POST", self.path, self.rfile.read(size), headers)
                result = origin.getresponse()
                body = result.read()
                self.send_response(result.status)
                for header in ("Content-Type", "Cache-Control"):
                    if value := result.getheader(header):
                        self.send_header(header, value)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            finally:
                origin.close()

    server = ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"https://localhost:{server.server_port}", ssl.create_default_context(cafile=cert)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def offer(binary, home):
    master, slave = pty.openpty()
    process = subprocess.Popen([
        binary, "pair", "offer", "muse", "--origin", "https://pairing-fixture.example.invalid", "--home", home,
        "--bearer-file", home / "fixture-muse.bearer",
    ], stdin=slave, stdout=slave, stderr=slave)
    os.close(slave)
    output = bytearray()
    deadline = time.monotonic() + 15
    try:
        while time.monotonic() < deadline:
            if not select.select([master], [], [], 0.25)[0]:
                continue
            try:
                chunk = os.read(master, 4096)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                break
            if not chunk:
                break
            output.extend(chunk)
        assert process.wait(timeout=2) == 0, "pair offer failed"
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=2)
        os.close(master)
    match = re.search(r"(?im)^\s*pairing code:\s*(\S+)\s*$", output.decode())
    assert match, "could not parse the private offer code"
    assert b"persistent mailbox poll every 2 minutes" in output, "Muse setup omitted polling"
    return match.group(1)


def verify(binary):
    with tempfile.TemporaryDirectory(prefix="p999k-pairing-") as temp:
        root = Path(temp).resolve()
        home, queue, tokens = fixture(binary, root)
        with hub(binary, home, queue) as port, ingress(root, port) as (origin, context):
            host = urlsplit(origin)

            def remote():
                return HTTPSConnection(host.hostname, host.port, timeout=5, context=context)

            def local():
                return HTTPConnection("127.0.0.1", port, timeout=5)

            code = offer(binary, home)
            bad = {"code": code[:-1] + ("Z" if code[-1] != "Z" else "Y")}
            assert http(remote(), "/dot/pair", {"code": code})[0] == 404
            assert http(remote(), "/muse/pair/extra", {"code": code})[0] == 404
            assert http(remote(), "/cue/pair", {"code": code})[0] == 401
            assert http(remote(), "/muse/pair", bad)[0] == 401
            status, cache, receipt = http(remote(), "/muse/pair", {"code": code})
            assert status == 200 and cache == "no-store" and receipt
            assert tuple(receipt[key] for key in ("mailbox", "mcp_path", "token_type")) == (
                "muse", ROUTES["muse"], "Bearer")
            assert receipt["access_token"] == tokens["muse"]
            bearer = receipt["access_token"]
            assert http(remote(), "/muse/pair", {"code": code})[0] == 401
            assert http(remote(), ROUTES["muse"], {"jsonrpc": "2.0", "id": 1,
                                                 "method": "tools/list", "params": {}})[0] == 401
            assert mcp(remote(), "muse", bearer, "initialize", HELLO)["protocolVersion"] == PROTOCOL
            request = tool(local(), "codex-cloud", tokens["codex-cloud"], "p-999k_send",
                           recipient="muse", body="synthetic pairing request", idempotency_key="pair-fixture")
            original = request["message"]["id"]
            inbound = tool(remote(), "muse", bearer, "p-999k_read", message_id=original)
            assert inbound["body"] == "synthetic pairing request"
            reply = tool(remote(), "muse", bearer, "p-999k_reply", message_id=original,
                         body="synthetic paired response")
            terminal = tool(local(), "codex-cloud", tokens["codex-cloud"], "p-999k_wait",
                            in_reply_to=original, timeout_ms=0)["message"]
            assert terminal["id"] == reply["reply_id"]
            assert terminal["envelope"]["in_reply_to"] == original
            assert terminal["envelope"]["sender"] == "muse"
            with sqlite3.connect(queue.as_uri() + "?mode=ro", uri=True) as db:
                assert db.execute("SELECT acknowledged_at_ms IS NOT NULL FROM messages WHERE id=?",
                                  (original,)).fetchone() == (1,)
            revoked_code = offer(binary, home)
            registration = home / "pigeonmq-cloud-registration.json"
            bindings = json.loads(registration.read_text())
            bindings["muse"]["revoked"] = True
            private_file(registration, bindings)
            revoked_status, _, _ = http(remote(), "/muse/pair", {"code": revoked_code})
            assert revoked_status == 401, f"revoked mailbox returned HTTP {revoked_status}"
    print("HTTPS pairing, code consumption, remote read, linked reply and original ACK passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    verify(args.binary.resolve(strict=True))

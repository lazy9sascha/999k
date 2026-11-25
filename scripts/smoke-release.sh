#!/bin/sh
# Run the release executables without a build, real credentials, or a tunnel.
set -eu
exec python3 -I - "$@" <<'PY'
import argparse
from contextlib import contextmanager, suppress
import hashlib
from http.client import HTTPConnection, HTTPException
import itertools
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tarfile
import tempfile
import time


BINARIES = ("999k", "999k-dot-resolve")
LICENSE = "LICENSE"
PROTOCOL = "2025-03-26"
MUSE = "/muse/mcp"
REQUEST = "hello from the release smoke"
REPLY = "hello back from the release smoke"


def check(condition, failure):
    if not condition:
        raise ValueError(failure)


def unpack(archive, dest):
    dest.mkdir()
    expected = {*BINARIES, LICENSE}
    with tarfile.open(archive, "r:gz") as package:
        members = package.getmembers()
        check(len(members) == len(expected) and {m.name for m in members} == expected,
              "unexpected archive layout")
        for member in members:
            check(member.isfile() and member.size > 0, "empty or non-regular archive member")
            executable = member.name in BINARIES
            check(not executable or member.mode & 0o100, "archive binary is not executable")
            target = dest / member.name
            with package.extractfile(member) as source, target.open("xb") as output:
                shutil.copyfileobj(source, output)
            target.chmod(0o700 if executable else 0o600)


def fixture(home):
    sender, muse = secrets.token_hex(32), secrets.token_hex(32)
    registry = home / "pigeonmq-cloud-registration.json"
    registry.write_text(json.dumps({
        name: {"mailbox": name, "revoked": False} for name in ("a", "muse")
    }))
    sender_policy = home / "fixture-sender-policy.json"
    sender_policy.write_text(json.dumps({"principals": [{
        "id": "a", "token_sha256": hashlib.sha256(sender.encode()).hexdigest(),
        "operations": ["send"], "send_to": ["muse"], "mailboxes": [], "revoked": False,
    }]}))
    muse_policy = home / "pigeonmq-cloud-muse-policy.json"
    muse_policy.write_text(json.dumps({
        "principal": "muse", "mailbox": "muse",
        "token_sha256": hashlib.sha256(muse.encode()).hexdigest(),
        "operations": ["read", "reply"], "enabled": True,
        "expires_at_ms": int(time.time() * 1000) + 600_000, "registration": str(registry),
    }))
    return sender, muse, sender_policy, muse_policy


def post(port, route, payload, token, expected=200, protocol=None):
    connection = HTTPConnection("127.0.0.1", port, timeout=5)
    headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
    if token is not None:
        headers["Authorization"] = "Bearer " + token
    if protocol is not None:
        headers["MCP-Protocol-Version"] = protocol
    try:
        connection.request("POST", route, json.dumps(payload), headers)
        response = connection.getresponse()
        check(response.status == expected, f"{route}: HTTP {response.status}, expected {expected}")
        return json.loads(response.read()) if expected == 200 else None
    finally:
        connection.close()


def mcp_client(port, bearer):
    ids = itertools.count(1)

    def call(method, params, protocol=PROTOCOL):
        request_id = next(ids)
        payload = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        result = post(port, MUSE, payload, bearer, protocol=protocol)
        check(result.get("id") == request_id and "error" not in result, f"MCP {method} failed")
        return result["result"]

    hello = {"protocolVersion": PROTOCOL, "capabilities": {},
             "clientInfo": {"name": "999k-release-smoke", "version": "1"}}
    post(port, MUSE, {"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": hello},
         None, expected=401)
    check(call("initialize", hello, None)["protocolVersion"] == PROTOCOL,
          "MCP protocol negotiation failed")

    def tool(name, **arguments):
        result = call("tools/call", {"name": name, "arguments": arguments})
        check(not result.get("isError"), f"MCP {name} failed")
        return json.loads(result["content"][0]["text"])

    return tool


@contextmanager
def running_hub(command, env, home, port):
    process = subprocess.Popen(command, env=env, cwd=home, stdin=subprocess.DEVNULL,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                               start_new_session=True)
    try:
        state_file = home / "pigeonmq-cloud-state.json"
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            check(process.poll() is None, "local hub exited before readiness")
            try:
                state = json.loads(state_file.read_text())
            except FileNotFoundError:
                state = {}
            if (state.get("state"), state.get("supervisor_pid"), state.get("local_port")) == (
                "local_ready", process.pid, port
            ):
                break
            time.sleep(0.05)
        else:
            raise ValueError("local hub did not become ready")
        yield
    finally:
        if process.poll() is None:
            process.terminate()
        try:
            process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            with suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            raise ValueError("local hub did not shut down")
        if process.returncode != 0:
            # A failed supervisor may leave its own loopback child alive.
            with suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
    check(process.returncode == 0, "local hub did not shut down cleanly")


def run_demo(bins, version, root):
    executable, resolver = (bins / name for name in BINARIES)
    env = {"HOME": str(root), "TMPDIR": str(root), "PATH": os.defpath, "LC_ALL": "C"}

    def cli(binary, *args, **options):
        return subprocess.run([binary, *args], env=env, cwd=root, text=True,
                              capture_output=True, timeout=15, **options)

    check(cli(executable, "--version", check=True).stdout.strip() == "999k " + version,
          "installed CLI version does not match")
    # Numeric only: parse and classify addresses without a DNS lookup or connection.
    check(json.loads(cli(resolver, "--dot-resolve", "9.9.9.9", check=True).stdout) == ["9.9.9.9"],
          "resolver did not return the numeric public address")
    check(cli(resolver, "--dot-resolve", "127.0.0.1").returncode == 1,
          "resolver did not reject loopback")
    print(f"999k {version}; resolver numeric checks passed", flush=True)

    home = root / "hub"
    cli(executable, "init", "--home", home, check=True)
    check(not (home / "hosted-enrollment.json").exists(),
          "ordinary init unexpectedly began hosted enrollment")
    # A file in place of a hub directory stops explicit enrollment before any network call.
    if sys.platform == "darwin" and "RADIO999K_RELEASE_REGISTRATION_URL" in os.environ:
        blocked = root / "invalid-hosted-home"
        blocked.touch()
        result = cli(executable, "init", "--reserve-address", "--home", blocked)
        expected = ("hosted hub home must be owner-private" if os.environ["RADIO999K_RELEASE_REGISTRATION_URL"]
                    else "no hosted registration provider")
        check(result.returncode and expected in result.stderr, "unexpected packaged registration default")
        env["RADIO999K_REGISTRATION_URL"] = "http://invalid.example.test"
        result = cli(executable, "init", "--reserve-address", "--home", blocked)
        check(result.returncode and "must be an HTTPS origin" in result.stderr,
              "runtime registration override did not take precedence")
        del env["RADIO999K_REGISTRATION_URL"]
        if os.environ["RADIO999K_RELEASE_REGISTRATION_URL"]:
            fresh = root / "selected-peer"
            # An invalid explicit sender stops allowed selections before managed services or I/O.
            def select(peer):
                return cli(executable, "init", "--orca", "--orca-peer", peer, "--orca-reserve-address",
                           "--home", fresh, "--orca-sender", "invalid sender", "--orca-bearer-file", blocked)
            for peer in ("muse", "grok-bot", "instinct"):
                result = select(peer)
                check(result.returncode and "supports Cue only" in result.stderr and not fresh.exists(),
                      "unsupported pilot peer reached local setup or pinned the fresh home")
            result = select("cue")
            check(result.returncode and "Invalid sender mailbox" in result.stderr,
                  "Cue could not proceed after unsupported selections on the same home")
            env["RADIO999K_REGISTRATION_URL"] = "https://self-hosted.example.test"
            result = select("muse")
            check(result.returncode and "Invalid sender mailbox" in result.stderr,
                  "custom registration provider unexpectedly restricted Muse")
            del env["RADIO999K_REGISTRATION_URL"]
    sender, muse, sender_policy, muse_policy = fixture(home)
    cli(executable, "muse-prepare", "--queue", home / "pigeonmq-cloud-queue.sqlite3",
        "--auth-policy", muse_policy, check=True)
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    command = [executable, "run", "--home", home, "--local-only", "--port", str(port),
               "--auth-policy", sender_policy]

    with running_hub(command, env, home, port):
        tool = mcp_client(port, muse)
        sent = post(port, "/messages/send",
                    {"sender": "a", "recipient": "muse", "body": REQUEST}, sender)["message"]
        message_id = sent["id"]
        args = {"message_id": message_id}
        read = tool("p-999k_read", **args)
        check((read["message_id"], read["state"], read["body"]) == (message_id, "pending", REQUEST),
              "the expected pending message was not read")
        reply = tool("p-999k_reply", **args, body=REPLY)
        reply_id = reply["reply_id"]
        check(reply_id and reply_id != message_id and not reply["duplicate"],
              "no new linked reply was created")
        read = tool("p-999k_read", **args)
        check((read["state"], read["reply_id"]) == ("replied", reply_id),
              "the original did not reference the reply")
        legacy = tool("pigeonmq_read", **args)
        check((legacy["message_id"], legacy["state"], legacy["reply_id"]) ==
              (message_id, "replied", reply_id),
              "the legacy MCP read alias did not return the same reply")
        print("Local authenticated send, read, and linked reply passed", flush=True)

    with running_hub(command, env, home, port):
        tool = mcp_client(port, muse)
        read = tool("p-999k_read", **args)
        check((read["message_id"], read["state"], read["reply_id"]) == (
            message_id, "replied", reply_id
        ), "the original linked reply ID changed after restart")
        thread = tool("p-999k_read_thread", conversation_id=sent["conversation_id"])
        check(any((msg["message_id"], msg["in_reply_to"], msg["body"], msg["sender"],
                   msg["recipient"]) == (reply_id, message_id, REPLY, "muse", "a")
                  for msg in thread["messages"]), "the linked reply was not present after restart")
    print("Restart passed: same linked reply ID and message in the queue")


def interrupted(*_):
    raise KeyboardInterrupt


parser = argparse.ArgumentParser(prog="smoke-release.sh")
parser.add_argument("--installed", action="store_true", help="use an installed binary directory")
parser.add_argument("path", type=Path, help="release archive or installed binary directory")
parser.add_argument("version", help="expected CLI version without the v prefix")
args = parser.parse_args()
signal.signal(signal.SIGTERM, interrupted)
os.umask(0o077)
try:
    with tempfile.TemporaryDirectory(prefix="999k-release-smoke-") as tmp:
        root = Path(tmp).resolve()
        bins = args.path.absolute() if args.installed else root / "bin"
        if not args.installed:
            unpack(args.path, bins)
        check(all((bins / name).is_file() and os.access(bins / name, os.X_OK) for name in BINARIES),
              "both 999k and 999k-dot-resolve must be executable")
        run_demo(bins, args.version, root)
except (TypeError, ValueError, KeyError, IndexError, OSError, HTTPException, tarfile.TarError,
        subprocess.SubprocessError) as error:
    sys.exit(f"release smoke failed: {error}")
except KeyboardInterrupt:
    sys.exit("release smoke interrupted")
PY

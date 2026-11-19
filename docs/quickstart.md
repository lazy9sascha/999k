# Quickstart

Build, configure the selected profile, start its transport, and connect the agent.
The local verification below is optional. It does not limit operational senders or message text.

Not sure which connection mode your agent needs? Check [agents and capabilities](agents.md) first.

## 1. Build

Use a checked executable on macOS, or build this checkout with stable Rust and 2024 edition support.

```sh
cargo build --locked --release
export PATH="$PWD/target/release:$PATH"
export PIGEONMQ_BIN="$PWD/target/release/pigeonmq"
"$PIGEONMQ_BIN" --help
```

Version 0.1.0 built `target/release/pigeonmq-cloud`. Update scripts and service definitions to use the full path to the new binary. The archived project also used the command `pigeonmq`. Run `"$PIGEONMQ_BIN" --version` to check which build you selected.

Pick a private runtime folder **outside the repository**. It holds the queue, policies, and tunnel keys.

```sh
export CLOUD_HOME="$HOME/.pigeonmq-cloud"
"$PIGEONMQ_BIN" init --home "$CLOUD_HOME"
```

Use the same explicit `--home` in later commands. `init` creates the queue if it is missing and writes connector drafts.
It does not start the hub, open a tunnel, or connect an app.
If the home contains a queue from an older release, see [Older queue databases](troubleshooting.md#older-queue-databases).

For **Dot**, use [the private Secure MCP Tunnel procedure](configuration.md#dot-through-a-private-secure-mcp-tunnel).
This procedure keeps local Bearer authentication and checks access from the intended Dot.

For **Codex Cloud**, use [Codex Cloud operation](configuration.md#codex-cloud-operation).
Mailbox setup prepares the profile. The official Secure MCP Tunnel connects to its route on the hub.
The steps below stay on loopback unless you set a public origin.
Prefer a reverse tunnel on your own domain. See [Use your own HTTPS origin](tunnels.md#use-your-own-https-origin).
If you have no domain, you can opt in to tunnl.gg for testing.

## 2. Optional local verification

This sends one message, reads it over HTTP like an agent would, replies, and checks the reply survives a restart. It uses a throwaway folder and a fake token. You need Python 3 and curl.

```sh
bin="$PIGEONMQ_BIN"
export PIGEON_HOME="$(mktemp -d)"
export PIGEON_PORT=54900          # any free loopback port
export TEST_TOKEN=local-test-only-not-a-credential
umask 077
"$bin" init --home "$PIGEON_HOME"

python3 - <<'PY'
import hashlib, json, os
from pathlib import Path
home = Path(os.environ["PIGEON_HOME"]).resolve()
registry = home / "pigeonmq-cloud-registration.json"
registry.write_text(json.dumps({p: {"mailbox": p, "revoked": False} for p in ("a", "muse")}))
(home / "fixture-sender-policy.json").write_text(json.dumps({"principals": [{
    "id": "a", "token_sha256": hashlib.sha256(os.environ["TEST_TOKEN"].encode()).hexdigest(),
    "operations": ["send"], "send_to": ["muse"], "mailboxes": [], "revoked": False,
}]}))
(home / "pigeonmq-cloud-muse-policy.json").write_text(json.dumps({
    "principal": "muse", "mailbox": "muse",
    "token_sha256": hashlib.sha256(os.environ["TEST_TOKEN"].encode()).hexdigest(),
    "operations": ["read", "reply"],
    "enabled": True, "expires_at_ms": 9999999999999,
    "registration": str(registry),
}))
PY

db="$PIGEON_HOME/pigeonmq-cloud-queue.sqlite3"
policy="$PIGEON_HOME/pigeonmq-cloud-muse-policy.json"
"$bin" muse-prepare --queue "$db" --auth-policy "$policy"   # prints muse_storage_prepared
```

Start the hub and talk to it:

```sh
"$bin" run --home "$PIGEON_HOME" --local-only --port "$PIGEON_PORT" \
  --auth-policy "$PIGEON_HOME/fixture-sender-policy.json" &
hub_pid=$!
```

Wait for `local_ready` in the runner output. Then send a test message:

```sh
id="$(curl -fsS "http://127.0.0.1:$PIGEON_PORT/messages/send" \
  -H "Authorization: Bearer $TEST_TOKEN" -H 'Content-Type: application/json' \
  --data '{"sender":"a","recipient":"muse","body":"hello"}' \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["message"]["id"])')"

mcp() {
  curl -fsS "http://127.0.0.1:$PIGEON_PORT/muse/mcp" \
    -H "Authorization: Bearer $TEST_TOKEN" \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2025-03-26' --data "$1"
}
read_msg() { mcp "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"pigeonmq_read\",\"arguments\":{\"message_id\":\"$id\"}}}"; }

read_msg                                   # state: pending
mcp "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"pigeonmq_reply\",\"arguments\":{\"message_id\":\"$id\",\"body\":\"hello back\"}}}"
read_msg                                   # state: replied, with reply_id
```

Restart check: run `kill $hub_pid; wait $hub_pid`. Start the hub again with the same command. Run `read_msg`. It is still `replied` with the same `reply_id`. Stop the hub when done.

## 3. Open a tunnel

Public HTTPS connectors need a public URL.
`run` has no default tunnel mode.
You must set a mode, or pass `--local-only` for loopback testing.
Prefer a reverse tunnel on your own domain.
See [Use your own HTTPS origin](tunnels.md#use-your-own-https-origin).
The service that handles public TLS can read the Bearer token and message bodies.

### Prepare the runtime folder

For **Muse**, one command does everything (queue, registration, policy, Keychain token):

```sh
"$PIGEONMQ_BIN" setup-muse --home "$CLOUD_HOME" --peer my-peer
```

`--peer` is the mailbox allowed to message Muse. Running it again is safe. It keeps existing policies and tells you which tunnel files are missing.

For **Grok Bot**, see [Grok Bot policy setup](configuration.md#grok-bot-policy-by-hand).

### Use tunnl.gg only for testing

Use this path only when you have no domain.
Set `PIGEONMQ_TUNNEL=tunnl` before you start `run`.

Read these limits first:

- tunnl.gg decrypts traffic on its servers. See the [privacy policy](https://tunnl.gg/privacy.md).
- Free tunnels close after 2 hours idle or 24 hours. See [pricing](https://tunnl.gg/pricing).
- From 2026-12-01, `stable@` needs a free account.
- The service has no uptime guarantee.
- The [terms of service](https://tunnl.gg/terms) apply.

```sh
ssh-keygen -t ed25519 -N '' -f "$CLOUD_HOME/pigeonmq-cloud-tunnel-ed25519"
```

Then save tunnl.gg's **verified** host key in `$CLOUD_HOME/pigeonmq-cloud-tunnel-known_hosts` (mode 600). Get the fingerprint from tunnl.gg's official docs and compare it. A bare `ssh-keyscan` result is not proof. Use a new key just for this hub.

### Start

For a reverse tunnel on your own domain:

```sh
PIGEONMQ_TUNNEL=external PIGEONMQ_PUBLIC_URL=https://pigeon.example.com \
  "$PIGEONMQ_BIN" run --home "$CLOUD_HOME"
```

For tunnl.gg testing:

```sh
PIGEONMQ_TUNNEL=tunnl "$PIGEONMQ_BIN" run --home "$CLOUD_HOME"
```

For loopback testing only:

```sh
"$PIGEONMQ_BIN" run --home "$CLOUD_HOME" --local-only
```

In another terminal, set the same `CLOUD_HOME` and `PIGEONMQ_BIN`. Check the local hub:

```sh
"$PIGEONMQ_BIN" health --home "$CLOUD_HOME"
```

`"local_ready":true` means the loopback hub answered. It does not check the tunnel or agent.
For tunnl.gg, check for `"state":"connected"` and `public_origin` in `$CLOUD_HOME/pigeonmq-cloud-state.json`.
Your agent's endpoint is that origin plus its route, for example `https://<name>.tunnl.gg/muse/mcp`.

Only one `run` can own a folder. Use `--port` to change the loopback port (default 54843).

## 4. Connect an app

To connect through tunnl.gg, copy a connector prompt after the runner reports `connected`. For your own HTTPS origin, [configure the connector manually](tunnels.md#use-your-own-https-origin).

```sh
"$PIGEONMQ_BIN" handoff --home "$CLOUD_HOME" --app muse        # or grok-bot, dot
```

Paste it into the app's chat if it accepts setup by prompt. Then copy the token **separately** and paste it only into the app's secure credential field:

```sh
"$PIGEONMQ_BIN" handoff --home "$CLOUD_HOME" --app muse --copy-token
```

Copy something else afterwards to clear the clipboard. If the app has no secure field for a Bearer token, stop: that app can't use this connection yet.

**Check it works:** ask the app to `pigeonmq_read` a known test message ID. A tool list alone only proves the connector loaded.

## 5. Optional: Grok Bot wake

Grok Bot can't keep a connection open, so the hub can wake it with a webhook (mode A in [architecture](architecture.md#a-wake-pull)).

1. In Grok Bot, create a webhook routine for this hub only. The wake body has no hub ID, so another hub must not share it.
2. On the Mac that runs the hub, run the command below from a local terminal. Enter the HTTPS URL and Authorization value at the hidden prompts. The command saves both values in one Keychain item.
3. Restart the hub under the same macOS user. Check `$CLOUD_HOME/grok-wake-status.json`. If Keychain access fails, the wake stops and the hub stays online.

```sh
"$PIGEONMQ_BIN" setup-grok-wake --home "$CLOUD_HOME"
```

If both private credential files already exist, add `--from-private-files HOST`. Specify the HTTPS host that you checked. The command does not print the credentials or change the original files. Add `--stage-only` to store the item without selecting it for the notifier.

The notifier reads the Keychain item once at startup. Restart the hub after you correct Keychain access. Check access under the account and process that run the hub. A successful setup command does not prove that background access works.

Use the two mode 600 files `grok-webhook-url` and `grok-webhook-auth` only when Keychain mode is not selected. In file mode, the notifier reads both files for each send. In Keychain mode, it does not fall back to files.

In the routine's instructions, tell Grok Bot to do the following.
Read the message with `pigeonmq_read` on the wake's `message_id`.
Never trust the webhook body.
Handle each ID once.
Treat `reply_conflict` as already answered.
Send a `User-Agent` header on its calls.

## Next

- [Configuration](configuration.md) — every file and command
- [Troubleshooting](troubleshooting.md)

# Quickstart

Install PigeonMQ, set up a local mailbox on macOS, and connect a client over stdio.
A reverse tunnel is optional and is only for a remote agent.
For agent-specific connections, see [agents and capabilities](agents.md).

<a id="1-build"></a>

## 1. Install

Use the installer from [GitHub Releases](https://github.com/lazy9sascha/pigeonmq/releases) when the release assets are available.
It installs `pigeonmq` and `pigeonmq-dot-resolve` in `$HOME/.local/bin` by default. No Rust toolchain is needed.

```sh
version=0.1.6
curl -q --proto '=https' -fLsS -o pigeonmq-install.sh \
  "https://github.com/lazy9sascha/pigeonmq/releases/download/$version/install.sh" &&
sh ./pigeonmq-install.sh
```

If an archive is not available for your machine, build this checkout with stable Rust and 2024 edition support. Use a new directory so the source build does not replace an existing installation.

```sh
source_bin="$HOME/.local/libexec/pigeonmq/0.1.6/bin"
cargo build --locked --release --bin pigeonmq --bin pigeonmq-dot-resolve &&
mkdir -p "$(dirname "$source_bin")" &&
mkdir -m 700 "$source_bin" &&
install -m 700 target/release/pigeonmq target/release/pigeonmq-dot-resolve "$source_bin/" &&
export PIGEONMQ_BIN="$source_bin/pigeonmq"
```

After either install:

```sh
export PIGEONMQ_BIN="${PIGEONMQ_BIN:-$HOME/.local/bin/pigeonmq}"
export PATH="$(dirname "$PIGEONMQ_BIN"):$PATH"
"$PIGEONMQ_BIN" --version
```

If you used another install directory, set `PIGEONMQ_BIN` to the absolute path of its `pigeonmq`.
Use a stable absolute path for a managed service.
For an existing 0.1.0 service, change the executable name from `pigeonmq-cloud` to `pigeonmq` when you upgrade.

## Local MCP on macOS

The stdio steps require a build that provides `pigeonmq stdio-mcp`. If your installed version lacks that command, build this checkout.

### macOS: managed Cue or Instinct

Use one managed home for all profiles in the same macOS login.
This example uses a new Instinct home and opts in to the private stdio socket on its first managed apply.
Use `cue` instead to serve `/cue/mcp`.

Keep an existing home on its current absolute path and configuration.
If `$HOME/.pigeonmq-cloud` already holds a managed installation, keep that directory and choose another absolute home for this example.
These commands opt in on the first active profile of a new managed Cue or Instinct home.
An existing managed installation without a socket does not gain one from this flag. That upgrade path is still under verification.

```sh
export CLOUD_HOME="$HOME/.pigeonmq-stdio"
if [ ! -e "$CLOUD_HOME" ] && [ ! -L "$CLOUD_HOME" ]; then
  "$PIGEONMQ_BIN" init --home "$CLOUD_HOME" &&
  "$PIGEONMQ_BIN" init instinct --home "$CLOUD_HOME" &&
  "$PIGEONMQ_BIN" init instinct --home "$CLOUD_HOME" --apply --stdio-socket
else
  printf '%s\n' "Choose a new PigeonMQ home." >&2
fi
```

The first named command stages the policy and bearer file. Use the private `token_file` path it prints.
The command does not print the bearer.
The apply command registers and starts the managed loopback hub and binds the private stdio socket for that home.
`--stdio-socket` belongs on that first managed apply. The setting stays with the home.
An already-active managed home can add it once with the same command and `--apply`.
This command does not start hosted registration.
Repeating an active setup keeps its bearer.
See [managed profiles](configuration.md#manage-built-in-profiles) for other profiles, email, and removal.
See [Local MCP over stdio](configuration.md#local-mcp-over-stdio) for the socket rules and for adding `--stdio-socket` to an existing foreground `serve-hub` command.
Use the macOS procedures for [Dot](configuration.md#dot-through-a-private-secure-mcp-tunnel) and [Codex Cloud](configuration.md#codex-cloud-operation).
A detected app is not a connected app.

#### Connect over stdio

`stdio-mcp` uses the absolute `pigeonmq` executable and the private bearer file path.
The client runs as the same OS user as the hub.
Pass the private bearer file path.

If your client uses an `mcpServers` object, enter this configuration. Otherwise, use the same command and arguments in the client's stdio settings.

```json
{
  "mcpServers": {
    "pigeonmq": {
      "command": "/absolute/path/to/pigeonmq",
      "args": [
        "stdio-mcp",
        "--home",
        "/absolute/path/to/hub-home",
        "--mailbox",
        "instinct",
        "--bearer-file",
        "/absolute/path/to/private-bearer-file"
      ]
    }
  }
}
```

Set `command` to the absolute path in `PIGEONMQ_BIN`.
Set `--home` to the same absolute home, and `--bearer-file` to the absolute `token_file` path from init.
Use `cue` as `--mailbox` when the applied profile is Cue.
The adapter uses the mailbox policy already mounted in the hub. Closing the client leaves the hub running. Use a synthetic message from an authorized peer to verify access.

See [Local MCP over stdio](configuration.md#local-mcp-over-stdio) for foreground socket setup and protocol details.

## 2. Try a local exchange

You need Python 3 for this optional test. It does not require Rust or a source checkout.
Set `version` to the installed version, without a leading `v`. Use its copy of the script from the source tag.

```sh
version=0.1.6
curl -q --proto '=https' -fLsS -o pigeonmq-smoke.sh \
  "https://raw.githubusercontent.com/lazy9sascha/pigeonmq/$version/scripts/smoke-release.sh" &&
sh ./pigeonmq-smoke.sh --installed "$(dirname "$PIGEONMQ_BIN")" "$version"
```

In a checkout, use [`scripts/smoke-release.sh`](../scripts/smoke-release.sh) instead of downloading it.
To check a downloaded archive before installing, pass its filename and version without `--installed`.

The test checks both binaries, starts a private loopback hub, and exchanges one message using synthetic Bearer tokens.
It stops and restarts the hub and checks that the same linked reply ID is still in the queue.
It removes the temporary home. It does not reach a public endpoint, touch Keychain, or start a background service.

## Prepare a real mailbox

Use an owner-private runtime folder outside the repository.
The home holds the queue and its policies. Use the same absolute `--home` each time.
For an existing hub, keep its home and configuration. See [Older queue databases](troubleshooting.md#older-queue-databases) if needed.

### Linux: foreground with a file credential

This example creates a new Instinct home. It does not use Keychain or start a service.
Use a new path. The commands stop if the folder already exists.

```sh
export CLOUD_HOME="$HOME/.pigeonmq-instinct"
mkdir -m 700 "$CLOUD_HOME" &&
(umask 077; printf '%s\n' '{"instinct":{"mailbox":"instinct","revoked":false}}' \
  > "$CLOUD_HOME/pigeonmq-cloud-registration.json") &&
"$PIGEONMQ_BIN" init --home "$CLOUD_HOME" &&
"$PIGEONMQ_BIN" init instinct --home "$CLOUD_HOME" --operational &&
"$PIGEONMQ_BIN" run --home "$CLOUD_HOME" --local-only
```

The running hub serves `http://127.0.0.1:54843/instinct/mcp`.
Use the Bearer in the mode-600 file printed as `token_file`. The command does not print the token.
Add active peers only as needed in the [registration file](configuration.md#registration-format).
Keep this terminal open while you use the hub. Press Ctrl+C to stop it.
On later starts, use only the `run` command with the same home.
For a new macOS managed Cue or Instinct home, use [macOS: managed Cue or Instinct](#macos-managed-cue-or-instinct) and connect with stdio.

## 3. Open a tunnel

This section is optional. Skip it for a client on the hub machine.
A remote HTTPS connector needs a reachable origin. Private Dot and Codex Cloud tunnels use separate procedures.
`run` has no default tunnel mode. Pass `--local-only` only for loopback.
If a managed hub already serves the route, connect the tunnel to that listener. Do not start a second runner.
Prefer a reverse tunnel on your own domain.
See [Use your own HTTPS origin](tunnels.md#use-your-own-https-origin).
The service that handles public TLS can read the Bearer token and message bodies.

### Prepare Muse on macOS

For **Muse**, prepare the queue, registration, policy, and Keychain token:

```sh
"$PIGEONMQ_BIN" setup-muse --home "$CLOUD_HOME" --peer my-peer
```

`--peer` is the mailbox allowed to message Muse. Repeat setup to check what is missing without replacing existing policies.

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

A local stdio client uses [Connect over stdio](#connect-over-stdio).
The steps below connect a client with Streamable HTTP.
For a client on the hub machine, use the loopback URL. A remote client needs a trusted HTTPS origin or its supported private tunnel.
Use that mailbox's real Bearer in the client's secure credential field. A URL alone cannot authenticate.

For example, add this entry to [Claude Code's `.mcp.json`](https://code.claude.com/docs/en/mcp#environment-variable-expansion-in-mcp-json):

```json
{
  "mcpServers": {
    "pigeonmq": {
      "type": "http",
      "url": "http://127.0.0.1:54843/instinct/mcp",
      "headers": {
        "Authorization": "Bearer ${PIGEONMQ_TOKEN}"
      }
    }
  }
}
```

For a different profile or a remote client, replace the URL with the exact route.
Supply `PIGEONMQ_TOKEN` from your credential store or the protected token file in that client's environment.
For a local Claude Code session, with shell tracing off, you can start it as follows:

```sh
PIGEONMQ_TOKEN="$(cat /absolute/path/to/protected-token-file)" claude
```

Do not paste the token into chat, source, config, or a command line.
If the client cannot accept a secure Bearer value, stop. An OAuth-only connection cannot use this Bearer flow.
This example configures the client only. It does not enable a hub route, grant mailbox access, or set up Events.
Make a permitted read-only call such as `pigeonmq_inbox` to confirm access.
To verify delivery, read a known test message sent by an authorized peer.

### Muse or Grok Bot handoff on macOS

For tunnl.gg, copy a connector prompt after the runner reports `connected`.
For your own HTTPS origin, [configure the connector manually](tunnels.md#use-your-own-https-origin).

```sh
"$PIGEONMQ_BIN" handoff --home "$CLOUD_HOME" --app muse        # or grok-bot
```

Paste it into the app's chat if it accepts setup by prompt. Then copy the token **separately** and paste it only into the app's secure credential field:

```sh
"$PIGEONMQ_BIN" handoff --home "$CLOUD_HOME" --app muse --copy-token
```

Copy something else afterwards to clear the clipboard. If the app has no secure field for a Bearer token, stop: that app can't use this connection yet.

**Check it works:** ask the app to `pigeonmq_read` a known test message ID. A tool list alone only proves the connector loaded.

### Upgrade an existing app to 0.1.2

On November 19, 2026, the hub listed `pigeonmq_inbox` and `pigeonmq_read_thread` while the saved Dot and Codex apps still had older catalogs. Refreshing each saved app restored the tools. Check the serving hub and each saved app separately during an upgrade.

1. Check the replacement binary with `"$PIGEONMQ_BIN" --version`. Restart the existing hub with that binary using the same `--home`, routes, policies, and transport. The release uses the existing queue, including its sent and received history.
2. Use an authenticated MCP client to call `tools/list` on the serving `/dot/mcp`, `/codex/mcp`, `/muse/mcp`, `/grok/mcp`, and any enabled registered mailbox route with read access. For `pigeonmq_inbox`, check that `inputSchema.properties` includes `scope`, `query`, and `order` alongside `limit` and `continuation`. Use each route's existing credential. If a route still advertises the old schema, verify which hub serves it before refreshing the app. For a manual request, [negotiate MCP first](tunnels.md#verify-and-update-the-clients).
3. Refresh discovery for the existing saved Dot and Codex connections in the intended ChatGPT account or workspace. Open each connection in ChatGPT Plugins and use **Refresh** if available. The server's `tools/list` checks the running hub. The app refresh pulls that schema into ChatGPT. Reuse each app ID, connection, and existing Events subscription. [OpenAI's metadata refresh guide](https://developers.openai.com/plugins/deploy/connect-chatgpt#refresh-metadata).
4. Select the same saved app in each intended conversation and call `pigeonmq_inbox` with `{"scope":"all","query":"<known test phrase>"}`. Inspect the tool arguments and confirm known sent and received test messages from before the upgrade appear with the same IDs, newest first, with metadata only. Check `{"scope":"inbox","query":"<known test phrase>"}` for the received test messages. Where a conversation still shows the old schema after refresh, try the same saved app in a new conversation and verify the call again. [More mailbox examples](architecture.md#mcp-tools).

## 5. Optional: Grok Bot wake

Grok Bot can't keep a connection open, so the hub can wake it with a webhook (mode A in [architecture](architecture.md#a-wake--pull)).

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

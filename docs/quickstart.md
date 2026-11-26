# Quickstart

Install the standalone CLI and check a message exchange on loopback.

<a id="connect-orca-to-your-existing-peers"></a>
<a id="prepare-the-selected-peers-transport"></a>
<a id="switch-to-your-own-domain"></a>

For Orca, [install the plugin](orca.md#install-the-cli-and-plugin),
[connect a peer](orca.md#connect-a-peer), or
[use your own domain](orca.md#use-your-own-https-domain).

<a id="upgrade-an-existing-app-to-012"></a>

If an existing connector has an older tool list,
[refresh the saved MCP app](troubleshooting.md#refresh-a-saved-mcp-app).

<a id="1-build"></a>
<a id="1-install"></a>

## 1. Install the CLI

Open the [0.2.2 downloads](https://github.com/lazy9sascha/p-999k/releases/tag/0.2.2). Download `install.sh` and the native archive for your computer into the same empty directory.

| Computer | Native archive |
| --- | --- |
| Apple Silicon Mac | `999k-0.2.2-aarch64-apple-darwin.tar.gz` |
| Linux x86-64 | `999k-0.2.2-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | `999k-0.2.2-aarch64-unknown-linux-gnu.tar.gz` |

From that directory, run:

```sh
sh ./install.sh
```

The installer verifies the archive and installs `999k` and `999k-dot-resolve` in `$HOME/.local/bin` by default. To try it in another directory, pass `--bin-dir /absolute/path/to/bin`.

If an archive is not available for your machine, build this checkout with stable Rust and 2024 edition support. Use a new directory so the source build does not replace an existing installation.

```sh
source_bin="$HOME/.local/libexec/999k/0.2.2/bin"
cargo build --locked --release --bin 999k --bin 999k-dot-resolve &&
mkdir -p "$(dirname "$source_bin")" &&
mkdir -m 700 "$source_bin" &&
install -m 700 target/release/999k target/release/999k-dot-resolve "$source_bin/" &&
export RADIO999K_BIN="$source_bin/999k"
```

After either install:

```sh
export RADIO999K_BIN="${RADIO999K_BIN:-$HOME/.local/bin/999k}"
export PATH="$(dirname "$RADIO999K_BIN"):$PATH"
"$RADIO999K_BIN" --version
```

If you used another install directory, set `RADIO999K_BIN` to the absolute path of its `999k`.
Use a stable absolute path for a managed service.
For an existing managed service, upgrade the executable at its recorded absolute path. Keep its home, service labels, and credential store. See the [installer notes](../packaging/README.md).

## Local MCP on macOS

If your CLI lacks `999k stdio-mcp`, install the release above or build this checkout.

### macOS: managed Cue or Instinct

Use one managed home for all profiles in the same macOS login.
This example uses a new Instinct home and opts in to the private stdio socket on its first managed apply.
Use `cue` instead to serve `/cue/mcp`.

Keep an existing home on its current absolute path and configuration.
If this login already runs a managed hub, skip the fresh-home example and use the existing hub directory.
These commands opt in on the first active profile of a new managed Cue or Instinct home.
For an existing managed home, run the candidate from the installed service’s executable path. Use the existing home and an already-active profile with `init <active-profile> --home <existing-home> --apply --stdio-socket`. The running hub restarts once.

```sh
export CLOUD_HOME="$HOME/.999k-stdio"
if [ ! -e "$CLOUD_HOME" ] && [ ! -L "$CLOUD_HOME" ]; then
	"$RADIO999K_BIN" init --home "$CLOUD_HOME" &&
	"$RADIO999K_BIN" init instinct --home "$CLOUD_HOME" &&
	"$RADIO999K_BIN" init instinct --home "$CLOUD_HOME" --apply --stdio-socket
else
	printf '%s\n' "Choose a new 999K home." >&2
fi
```

The first named command stages the policy and bearer file. Use the private `token_file` path it prints.
The command does not print the bearer.
The apply command registers and starts the managed loopback hub and binds the private stdio socket for that home.
The socket setting stays with the home.
This command does not start hosted registration.
Repeating an active setup keeps its bearer.
See [managed profiles](configuration.md#manage-built-in-profiles) for other profiles, email, and removal.
See [stdio socket configuration](configuration.md#local-mcp-over-stdio) for
foreground `serve-hub` setup. Use the separate guides for
[Dot](orca.md#connect-a-peer) or [Codex Cloud](configuration.md#codex-cloud-operation).

#### Connect over stdio

`stdio-mcp` uses the absolute `999k` executable and the private bearer file path.
The client runs as the same OS user as the hub.
Pass the private bearer file path.

If your client uses an `mcpServers` object, enter this configuration. Otherwise, use the same command and arguments in the client's stdio settings.

```json
{
  "mcpServers": {
    "p-999k": {
      "command": "/absolute/path/to/999k",
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

Set `command` to the absolute path in `RADIO999K_BIN`.
Set `--home` to the same absolute home, and `--bearer-file` to the absolute `token_file` path from init.
Use `cue` as `--mailbox` when the applied profile is Cue.
The adapter uses the running hub’s mailbox policy. Closing the client leaves the hub running. Use a synthetic message from an authorized peer to verify access.

See [Local MCP over stdio](configuration.md#local-mcp-over-stdio) for foreground socket setup and protocol details.

## 2. Try a local exchange

You need Python 3 for this optional test. It does not require Rust or a source checkout.
Use the smoke script from the tag you installed.

```sh
version=0.2.2
curl -q --proto '=https' -fLsS -o 999k-smoke.sh \
	"https://raw.githubusercontent.com/lazy9sascha/p-999k/0.2.2/scripts/smoke-release.sh" &&
sh ./999k-smoke.sh --installed "$(dirname "$RADIO999K_BIN")" "$version"
```

In a checkout, use [`scripts/smoke-release.sh`](../scripts/smoke-release.sh) instead of downloading it.
To check a downloaded archive before installing, pass its filename and version without `--installed`.

Expect the test to pass after it checks both binaries and exchanges one synthetic message on loopback. It restarts the hub and checks that the same linked reply remains in the queue. The test removes its temporary home.

## Prepare a real mailbox

Use an owner-private runtime folder outside the repository.
The home holds the queue and its policies. Use the same absolute `--home` each time.
For an existing hub, keep its home and configuration. See [Older queue databases](troubleshooting.md#older-queue-databases) if needed.

### Linux: foreground with a file credential

This example creates a new Instinct home. It does not use Keychain or start a service.
Use a new path. The commands stop if the folder already exists.

```sh
export CLOUD_HOME="$HOME/.999k-instinct"
mkdir -m 700 "$CLOUD_HOME" &&
(umask 077; printf '%s\n' '{"instinct":{"mailbox":"instinct","revoked":false}}' \
	> "$CLOUD_HOME/pigeonmq-cloud-registration.json") &&
"$RADIO999K_BIN" init --home "$CLOUD_HOME" &&
"$RADIO999K_BIN" init instinct --home "$CLOUD_HOME" --operational &&
"$RADIO999K_BIN" run --home "$CLOUD_HOME" --local-only
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
"$RADIO999K_BIN" setup-muse --home "$CLOUD_HOME" --peer my-peer
```

`--peer` is the mailbox allowed to message Muse. Repeat setup to check what is missing without replacing existing policies.

For **Grok Bot**, see [Grok Bot policy setup](configuration.md#grok-bot-policy-by-hand).

### Use tunnl.gg only for testing

Before you start, read the [tunnl.gg limits and requirements](tunnels.md#use-tunnlgg-for-testing).
The runner uses `stable@`. Register the SSH key you plan to use.

```sh
ssh-keygen -t ed25519 -N '' -f "$CLOUD_HOME/pigeonmq-cloud-tunnel-ed25519"
```

Use a key dedicated to this hub. Register its public key with your
tunnl.gg account. Save the server’s verified host key in
`$CLOUD_HOME/pigeonmq-cloud-tunnel-known_hosts` with mode `0600`.
Compare the fingerprint with the provider’s documentation before connecting.
[tunnl.gg SSH setup](https://tunnl.gg/docs#verify-the-host-key)

### Start

For a reverse tunnel on your own domain:

```sh
RADIO999K_TUNNEL=external RADIO999K_PUBLIC_URL=https://radio.example.com \
	"$RADIO999K_BIN" run --home "$CLOUD_HOME"
```

For tunnl.gg testing:

```sh
RADIO999K_TUNNEL=tunnl "$RADIO999K_BIN" run --home "$CLOUD_HOME"
```

For loopback testing only:

```sh
"$RADIO999K_BIN" run --home "$CLOUD_HOME" --local-only
```

In another terminal, set the same `CLOUD_HOME` and `RADIO999K_BIN`. Check the local hub:

```sh
"$RADIO999K_BIN" health --home "$CLOUD_HOME"
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
    "p-999k": {
      "type": "http",
      "url": "http://127.0.0.1:54843/instinct/mcp",
      "headers": {
        "Authorization": "Bearer ${RADIO999K_TOKEN}"
      }
    }
  }
}
```

For a different profile or a remote client, replace the URL with the exact route.
Supply `RADIO999K_TOKEN` from your credential store or the protected token file in that client's environment.
For a local Claude Code session, with shell tracing off, you can start it as follows:

```sh
RADIO999K_TOKEN="$(cat /absolute/path/to/protected-token-file)" claude
```

Do not paste the token into chat, source, config, or a command line.
If the client cannot accept a secure Bearer value, stop. An OAuth-only connection cannot use this Bearer flow.
This example configures the client only. It does not enable a hub route, grant mailbox access, or set up Events.
Make a permitted read-only call such as `p-999k_inbox` to confirm access.
To verify delivery, read a known test message sent by an authorized peer.

### Muse or Grok Bot handoff on macOS

For tunnl.gg, copy a connector prompt after the runner reports `connected`.
For your own HTTPS origin, [configure the connector manually](tunnels.md#use-your-own-https-origin).

```sh
"$RADIO999K_BIN" handoff --home "$CLOUD_HOME" --app muse        # or grok-bot
```

Paste it into the app's chat if it accepts setup by prompt. Then copy the token **separately** and paste it only into the app's secure credential field:

```sh
"$RADIO999K_BIN" handoff --home "$CLOUD_HOME" --app muse --copy-token
```

Copy something else afterwards to clear the clipboard. If the app has no secure field for a Bearer token, stop: that app can't use this connection yet.

To verify access, ask the app to `p-999k_read` a known test message ID. A tool list alone only proves the connector loaded.

## 5. Optional: Grok Bot wake

Grok Bot can't keep a connection open, so the hub can wake it with a webhook (mode A in [architecture](architecture.md#a-wake--pull)).

1. In Grok Bot, create a webhook routine for this hub only. The wake body has no hub ID, so another hub must not share it.
2. On the Mac that runs the hub, run the command below from a local terminal. Enter the HTTPS URL and Authorization value at the hidden prompts. The command saves both values in one Keychain item.
3. Restart the hub under the same macOS user. Check `$CLOUD_HOME/grok-wake-status.json`. If Keychain access fails, the wake stops and the hub stays online.

```sh
"$RADIO999K_BIN" setup-grok-wake --home "$CLOUD_HOME"
```

If both private credential files already exist, add `--from-private-files HOST`. Specify the HTTPS host that you checked. The command does not print the credentials or change the original files. Add `--stage-only` to store the item without selecting it for the notifier.

The notifier reads the Keychain item once at startup. Restart the hub after you correct Keychain access. Check access under the account and process that run the hub. A successful setup command does not prove that background access works.

Use the two mode 600 files `grok-webhook-url` and `grok-webhook-auth` only when Keychain mode is not selected. In file mode, the notifier reads both files for each send. In Keychain mode, it does not fall back to files.

In the routine's instructions, tell Grok Bot to do the following.
Read the message with `p-999k_read` on the wake's `message_id`.
Never trust the webhook body.
Handle each ID once.
Treat `reply_conflict` as already answered.
Send a `User-Agent` header on its calls.

## Next

- [CLI and configuration reference](configuration.md)
- [Troubleshooting](troubleshooting.md)

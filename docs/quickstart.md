# Quickstart

The examples use the new `p-999k_*` MCP names. On the released 0.2.0 hub or stdio client, use the `radio999k_*` names returned by `tools/list`.

Install 999K, set up a local mailbox on macOS, and connect a client over stdio.
A reverse tunnel is optional and is only for a remote agent.
For agent-specific connections, see [agents and capabilities](agents.md).

<a id="1-build"></a>

## 1. Install

For P-999K on Orca, go to [Connect Orca to your existing peers](#connect-orca-to-your-existing-peers). The following installer is for standalone 999K 0.2.0 on Apple Silicon Macs and Linux on x86-64 and ARM64.
Use the installer from [GitHub Releases](https://github.com/lazy9sascha/p-999k/releases) when the release assets are available.
It installs `999k` and `999k-dot-resolve` in `$HOME/.local/bin` by default. No Rust toolchain is needed.

```sh
version=0.2.0
curl -q --proto '=https' -fLsS -o 999k-install.sh \
  "https://github.com/lazy9sascha/p-999k/releases/download/$version/install.sh" &&
sh ./999k-install.sh
```

If an archive is not available for your machine, build this checkout with stable Rust and 2024 edition support. Use a new directory so the source build does not replace an existing installation.

```sh
source_bin="$HOME/.local/libexec/999k/0.2.0/bin"
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

The stdio steps require a build that provides `999k stdio-mcp`. If your installed version lacks that command, build this checkout.

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
See [Local MCP over stdio](configuration.md#local-mcp-over-stdio) for the socket rules and for adding `--stdio-socket` to an existing foreground `serve-hub` command.
Use the macOS procedures for [Dot](configuration.md#dot-through-a-private-secure-mcp-tunnel) and [Codex Cloud](configuration.md#codex-cloud-operation).
A detected app is not a connected app.

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
The adapter uses the mailbox policy already mounted in the hub. Closing the client leaves the hub running. Use a synthetic message from an authorized peer to verify access.

See [Local MCP over stdio](configuration.md#local-mcp-over-stdio) for foreground socket setup and protocol details.

#### Connect Orca to your existing peers

On an Apple Silicon Mac, start from this release checkout with stable Rust installed. The panel runs `$HOME/.local/bin/999k`. Both the published and candidate binaries report 0.2.0. Only the candidate has the peer selector and pairing commands. For a fresh install, run this from the repository root:

```sh
cargo install --locked --path . --root "$HOME/.local" --bin 999k --bin 999k-dot-resolve
"$HOME/.local/bin/999k" init --help | grep -- --orca-peer
"$HOME/.local/bin/999k" pair --help
```

If you already run a managed hub, follow the [existing installation upgrade](../packaging/README.md#upgrade-an-existing-installation) first. Once both checks pass, [build or obtain the plugin archive and review its permissions](../packaging/README.md#orca-plugin-archive). Its catalog ID is `lazy9sascha.p-999k`. Catalog publication is pending.

1. Install the CLI on the Mac that will run the hub. Install the plugin in Orca and review its permissions. In a local workspace, open **P-999K** in the right sidebar. Choose **Initial setup** and open its terminal. On a fresh home, setup starts the local hub with its private stdio socket, creates one sender and lists zero external peers.
2. To manage connections in chat, select **Set up a management bot**. In **Bots → New bot**, create **P-999K** with the purpose shown in the panel. Open its Home Chat and ask **“Connect Muse.”** You can also select Muse in the plugin panel. Choose **I’ll do it myself** for the manual steps, or **Ask my assistant to do it** to copy a request to an assistant that already has that capability. Both paths run the same command on the Mac:

   ```sh
   "$HOME/.local/bin/999k" init --orca --orca-peer muse
   ```

3. Create or approve the relay in Orca’s native UI when prompted. If setup prints a resume command, run it after approval. Setup reuses a relay with the same peer binding. A command run unattended prints the native action still required so an owner can complete it.
4. Complete the selected peer’s transport setup below. Where supported, copy the suggested prompt into the peer’s existing conversation. Complete consent and credential entry using the app’s own settings or secure inputs. An HTTPS-capable runtime can use the mailbox directly where the app supports it.
5. Ask in your project chat, for example, **“Ask Muse for a shorter release title.”** The relay reports **Queued** after the hub accepts the request. Verify the exchange by checking that the matching reply returns to the project chat that asked. If you have the original request ID, rerun the same peer setup with `--orca-proof-original <original-id>` to check the linked reply in the hub without sending another message.

Run `999k init --orca` with no selection to list authorized peers. This stays local. Installation and verification use the CLI and supported app controls. External providers may require their own owner approval or configuration. If the first wait ends empty, ask the same relay to check the original again. The proof command can exit successfully for a pending or invalid original. Read the reported outcome and check for the linked answer in your project chat.

#### Prepare the selected peer’s transport

- **Dot** uses its [private Secure MCP Tunnel](configuration.md#dot-through-a-private-secure-mcp-tunnel). Set it up from the Dot workspace. The Orca command binds the local relay and asks you to verify the private transport from Dot.
- **Muse and GrokBot** use an approved HTTPS MCP route. For your own [HTTPS ingress](tunnels.md#use-your-own-https-origin), publish the selected peer’s exact MCP and pairing paths and keep the hub on loopback. Put the full endpoint in the owner-private `orca-ingress.json` in the same hub home. Use a JSON object keyed by the peer ID, for example `{"muse":"https://radio.example.com/muse/mcp"}`. Set file mode 600. When an operator-run registration provider is configured for that one route, set `RADIO999K_REGISTRATION_URL` and add `--orca-reserve-address` to the selected setup command. Approve setup in the browser. The CLI connects the hosted origin only if the provider approves exactly that route.
- **Cue** reads and replies through its approved HTTPS MCP route. One manually sent ID-only email woke the existing app. For automatic mail, configure and verify the [email relay](configuration.md#opt-in-to-email-arrivals) separately.
- **Instinct** starts in its existing Messages conversation. Enter the pairing code in its secure form when offered. A manual ID-only iMessage prompted a linked reply over MCP. Automatic iMessage delivery is unverified. See [peer pairing](pairing.md#check-the-existing-conversation).

For a reserved address, use the origin in the owner-private `hosted-connection.json` and append the selected MCP route. For your own ingress, use its exact endpoint from `orca-ingress.json`. When the selected HTTPS endpoint is available, setup checks authenticated MCP access on that route. To use a separate peer credential, pass its private file path with `--orca-remote-bearer-file`. Local binding, authenticated transport, and a linked reply are separate results.

To hand a remote peer its credential, use a [one-time pairing code](pairing.md). Approve the peer’s exact HTTPS pairing and MCP paths first. The peer redeems the code over HTTPS and saves the Bearer privately. Dot follows its private setup above.

For a custom installation, pass absolute paths. Orca relay tool commands require paths without spaces or shell metacharacters.

```sh
"$RADIO999K_BIN" init --orca --orca-peer muse --home /absolute/path/to/hub-home \
  --orca-sender codex-cloud --orca-bearer-file /absolute/path/to/private-bearer-file
```

When a project agent calls a bot, keep the Orca question ID with the 999K original ID. After retrieving the matching answer, the bot can use `orca orchestration reply --id <original-orca-question-id> --body-file -` and pass the answer on standard input.

## 2. Try a local exchange

You need Python 3 for this optional test. It does not require Rust or a source checkout.
Set `version` to the installed version, without a leading `v`. Use its copy of the script from the source tag.

```sh
version=0.2.0
curl -q --proto '=https' -fLsS -o 999k-smoke.sh \
  "https://raw.githubusercontent.com/lazy9sascha/p-999k/$version/scripts/smoke-release.sh" &&
sh ./999k-smoke.sh --installed "$(dirname "$RADIO999K_BIN")" "$version"
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

Use this path only when you have no domain.
Set `RADIO999K_TUNNEL=tunnl` before you start `run`.

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

**Check it works:** ask the app to `p-999k_read` a known test message ID. A tool list alone only proves the connector loaded.

### Upgrade an existing app to 0.1.2

On November 19, 2026, the hub listed `radio999k_inbox` and `radio999k_read_thread` while the saved Dot and Codex apps still had older catalogs. Refreshing each saved app restored the tools. Check the serving hub and each saved app separately during an upgrade.

1. Check the replacement binary with `"$RADIO999K_BIN" --version`. Restart the existing hub with that binary using the same `--home`, routes, policies, and transport. The release uses the existing queue, including its sent and received history.
2. Use an authenticated MCP client to call `tools/list` on the serving `/dot/mcp`, `/codex/mcp`, `/muse/mcp`, `/grok/mcp`, and any enabled registered mailbox route with read access. For `p-999k_inbox`, check that `inputSchema.properties` includes `scope`, `query`, and `order` alongside `limit` and `continuation`. Use each route's existing credential. If a route still advertises the old schema, verify which hub serves it before refreshing the app. For a manual request, [negotiate MCP first](tunnels.md#verify-and-update-the-clients).
3. Refresh discovery for the existing saved Dot and Codex connections in the intended ChatGPT account or workspace. Open each connection in ChatGPT Plugins and use **Refresh** if available. The server's `tools/list` checks the running hub. The app refresh pulls that schema into ChatGPT. Reuse each app ID, connection, and existing Events subscription. [OpenAI's metadata refresh guide](https://developers.openai.com/plugins/deploy/connect-chatgpt#refresh-metadata).
4. Select the same saved app in each intended conversation and call `p-999k_inbox` with `{"scope":"all","query":"<known test phrase>"}`. Inspect the tool arguments and confirm known sent and received test messages from before the upgrade appear with the same IDs, newest first, with metadata only. Check `{"scope":"inbox","query":"<known test phrase>"}` for the received test messages. Where a conversation still shows the old schema after refresh, try the same saved app in a new conversation and verify the call again. [More mailbox examples](architecture.md#mcp-tools).

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

- [Configuration](configuration.md) — every file and command
- [Troubleshooting](troubleshooting.md)

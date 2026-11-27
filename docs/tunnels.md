# Set up remote access

The hub always listens on loopback. `run` has no default tunnel mode. You must choose how to expose the hub:

| Setting | Behavior |
| --- | --- |
| No env file and no tunnel variables | Refuse to start. Print `tunnel_mode_required` and exit. |
| `RADIO999K_TUNNEL=external` with `RADIO999K_PUBLIC_URL` | Skip SSH. Record the specified HTTPS origin as unverified. |
| `RADIO999K_TUNNEL=tunnl` without `RADIO999K_PUBLIC_URL` | Start a tunnl.gg SSH reverse tunnel. |
| `--local-only` | Loopback testing only. Ignore the tunnel variables and any env file. |

Set the variables in the runner's process environment or pass `--env-file PATH`. The runner does not combine the two sources or search for a `.env` file. A missing mode, an unknown mode, `external` without a URL, or a URL without `external` stops startup.

For Dot or Codex Cloud, use a [protected HTTPS endpoint](configuration.md#dot-and-codex-cloud-through-direct-oauth)
or a [private OpenAI Secure MCP Tunnel](../examples/openai-secure-tunnel/README.md).

## Use a private .env file

From the repository root, copy the [template](../.env.example) into your owner-only runtime folder (`$CLOUD_HOME`) if you do not already have a settings file:

```sh
(umask 077; cp .env.example "$CLOUD_HOME/.env")
```

Set your own HTTPS origin in that mode-600 file:

```
RADIO999K_TUNNEL=external
RADIO999K_PUBLIC_URL=https://radio.example.com
```

Use one unquoted `KEY=value` per line. The file accepts only the two named keys and requires `RADIO999K_TUNNEL`. Blank lines and whole-line `#` comments are allowed. The limit is 4 KiB of UTF-8 text. Do not include credentials, `export`, or shell expressions.

```sh
999k run --home "$CLOUD_HOME" --env-file "$CLOUD_HOME/.env"
```

If your checkout itself is owner-only, you can instead use `(umask 077; cp .env.example .env)` and `999k run --home "$CLOUD_HOME" --env-file .env`. Do not copy over an existing settings file.

## Use your own HTTPS origin

Choose a hostname such as `radio.example.com`. Configure DNS and TLS
with your provider. Set up a reverse tunnel or HTTPS proxy to forward to
the hub on the same machine.

The public TLS endpoint can read Bearer tokens and message bodies.
Cloudflare terminates TLS for a public Cloudflare Tunnel. With a
pass-through provider, keep TLS termination on an endpoint you control
to keep the traffic encrypted through that provider.

### Prepare the route

In your proxy or tunnel, map the hostname to `http://127.0.0.1:54843`. If your runner uses another `--port`, use that port in both the service URL and the upstream `Host` header. For the default port:

- Set the upstream `Host` header to `127.0.0.1:54843`. Do not forward the public hostname as the upstream `Host`.
- Preserve the [MCP paths](architecture.md#routes) and `Authorization: Bearer`
  header. If you use one-time pairing, also forward the selected peer’s
  [pairing path](#pairing-routes). Do not publish the leased
  receiver or unrelated routes.
- Keep the hub on loopback and leave Bearer, mailbox, Host, and Origin checks on. The hub rejects browser requests with `Origin` or `Sec-Fetch-Site` headers.

For Cloudflare, see the [named tunnel example](../examples/cloudflare/README.md) for creation, path and Host settings, and separate credential files.

### Switch the runner

Use the [private env file](#use-a-private-env-file) above, or set the same process variables directly:

```sh
RADIO999K_TUNNEL=external RADIO999K_PUBLIC_URL=https://radio.example.com \
  999k run --home "$CLOUD_HOME"
```

Use the default HTTPS port. Do not include a path beyond an optional trailing slash, credentials, a query, or a fragment. When replacing a running instance, keep its home, port, and profile or policy settings. Stop the old runner before starting the new one. If a supervisor starts the hub, update that service's existing command rather than starting a second runner against the same home.

This setting starts the hub without SSH. It does not start the proxy, configure DNS, or test the route. The runner records the origin and connector endpoints only in the private `pigeonmq-cloud-state.json`. It reports `local_ready` and `remote_verified: false`, not `connected`.

### Verify and update the clients

1. Run `999k health --home "$CLOUD_HOME"` to check the local hub and queue without contacting the public hostname. Use a temporary private home and synthetic mail if rehearsing the switch.
2. Verify that you control the endpoint and trust the TLS terminator. It can
   read the Bearer token. Add the new URL in the app. Use a [one-time pairing
   code](#pair-a-remote-peer) for a supported peer or set the matching token in the
   connector’s secure field. For Dot or Codex Cloud with Cloudflare Access,
   follow the [OAuth setup](configuration.md#dot-and-codex-cloud-through-direct-oauth).
3. In the updated connector, make a permitted read-only MCP call. For a manual MCP check, start with `initialize`, then send the negotiated version in the `MCP-Protocol-Version` header on later requests. A successful call proves that connector can reach the hub. It does not verify delivery or Events. The optional `health --probe-public` command checks each configured public MCP route. It sends no credentials. A proxy can stop the check before the request reaches 999K. `tunnel.routes` reports the result for each profile. A partial catalog does not confirm all routes. The command does not verify authenticated MCP. Its exit code still reports local readiness only.

If you keep the same hostname and path, clients can keep that URL. Otherwise, update each affected client separately. Dot or Codex clients using a separate OpenAI Secure MCP Tunnel do not switch automatically. To roll back, restore the previous runner settings and point affected clients at the current URL of their previous transport.

## Pair a remote peer

A pairing code gives a peer its mailbox credential over HTTPS. It works
once and expires after 10 minutes. Enter the code in the app's private
setup. The app keeps the Bearer in a private file or secure store.

### Pairing routes

In [Orca setup](orca.md#connect-a-peer), select the peer to prepare its mailbox
and managed credential. Verify the route before issuing a code.

On your own domain, publish the exact pairing and MCP paths on the same
HTTPS origin. Keep the pairing response out of proxy logs and caches. A
[hosted address](orca.md#reserve-a-hosted-address) gets these paths when you
attach the selected peer.

| Peer ID | Pairing path | MCP path |
| --- | --- | --- |
| `muse` | `/muse/pair` | `/muse/mcp` |
| `grok-bot` | `/grok/pair` | `/grok/mcp` |
| `cue` | `/cue/pair` | `/cue/mcp` |
| `instinct` | `/instinct/pair` | `/instinct/mcp` |

Only publish the paths for the selected peer. For Dot, use the separate
[OAuth or private tunnel procedure](orca.md#connect-a-peer).

### Offer and redeem

Run the offer command in an owner-controlled terminal on the hub Mac.
The command verifies the peer's MCP and pairing paths against
`orca-ingress.json` or the saved hosted address. If you already verified
the origin, use `--origin https://radio.example.com` to skip the automatic
check. Use `--bearer-file` for a separately configured credential.

```sh
999k pair offer muse
```

Copy the setup instructions to the peer's existing conversation. Enter
the origin and code in the app's private fields. To redeem from a peer
runtime, send `{"code":"<one-time-code>"}` as an HTTPS POST to its pairing
path. Disable redirects and keep the response out of logs. Verify
`mailbox`, `mcp_path`, and `token_type: Bearer`. Save `access_token` in a
private file or credential store.

For peers with terminal access, the 999k CLI can perform the same exchange. On macOS, the following command prompts without echoing the code and saves the Bearer without printing it. Linux peers use protected standard input or a private code file. Use a new file in an owner-private directory.

```sh
mkdir -m 700 "$HOME/.999k-peer"
999k pair redeem muse --origin https://radio.example.com \
	--output "$HOME/.999k-peer/muse.bearer"
```

For a remote Bash terminal or scripted enrollment, read the code without echo and feed it through a pipe. You can also use `--code-file` with an owner-private file. Keep the code out of command-line arguments, logs and source files.

```bash
IFS= read -rs -p 'Pairing code: ' code
printf '%s\n' "$code" | 999k pair redeem muse --origin https://radio.example.com \
	--code-stdin --output "$HOME/.999k-peer/muse.bearer"
unset code
```

After submission, check whether the peer redeemed the code and can call its
mailbox. If the code is still pending, wait for the app outcome. If the code
expires before redemption, reopen the private form and issue one new code.
Keep an existing credential if the app already paired successfully.

### Check the existing conversation

For Muse, the offer also asks the remote VM to enable one persistent two-minute poll. After Muse reports an active schedule, send a new request from Orca while the Muse chat is idle. A linked reply to that request verifies automatic intake.

Clients with a native MCP connector can set its exact HTTPS route and use the saved Bearer from private settings. Clients with an HTTP-capable runtime can send authenticated Streamable HTTP MCP JSON-RPC directly to that same route with `Authorization: Bearer <saved-token>`.

- **GrokBot** uses its existing app. An arrival can wake a dedicated webhook routine. A VM-local poller can trigger that routine. Its interval is set on the VM, and it may need to be restarted after a reboot. Verify the wake separately while chat is idle.
- **Cue** receives arrival notices by email when configured. A Mac relay can send the notice. Cue’s email integration handles the wake on the app side. Cue can read and reply through its connector or make authenticated MCP requests from an HTTPS-capable runtime.
- **Instinct** uses its existing iMessage conversation for setup. Use the app’s secure login form for code entry when it offers one. Its runtime redeems the code by plain HTTPS POST with a JSON body, saves the credential privately, and sends authenticated JSON-RPC directly. The pairing endpoint accepts JSON and returns JSON to the runtime. Use the original message ID to read and reply. In the verified exchange, a manual ID-only iMessage identified the original. Automatic iMessage intake remains unverified.

Have the peer call MCP `initialize` and `tools/list` to verify discovery. Send one
new request. Have the peer read it and call `p-999k_reply` with the original
message ID. Check the original ACK and linked reply on the sender side. For
Orca, check the reply in the original project chat. To verify automatic
intake, send a test request while the peer’s conversation is idle.

### Verify locally with synthetic data

In a local terminal outside an Orca agent session, build the CLI and run
the pairing test with Python 3 and OpenSSL. The test serves HTTPS on loopback with a temporary certificate. It uses fixture mailboxes, never prints the pairing code or Bearer, and checks rejection of wrong, consumed, and revoked codes, remote read and reply, and the original acknowledgement.

```sh
cargo build --locked --bin 999k
python3 tests/pairing_https.py target/debug/999k
```

## Use tunnl.gg for testing

Use this only when you do not have a domain.
Set `RADIO999K_TUNNEL=tunnl` and do not set `RADIO999K_PUBLIC_URL`.
The runner then starts an SSH reverse tunnel to tunnl.gg.
You must place an SSH identity and a pinned host key in the runtime folder.
See [Open a tunnel](quickstart.md#3-open-a-tunnel).

Read these limits before you start:

- tunnl.gg decrypts traffic on its servers. See the [privacy policy](https://tunnl.gg/privacy.md).
- Free tunnels close after 2 hours idle or 24 hours. See [pricing](https://tunnl.gg/pricing).
- Register new SSH keys to a free account before using `stable@`. The same
  requirement applies to previously used keys from December 1, 2026.
  [Stable URL requirements](https://tunnl.gg/docs#keep-the-same-url)
- The service has no uptime guarantee.
- The [terms of service](https://tunnl.gg/terms) apply to that connection.

```sh
RADIO999K_TUNNEL=tunnl 999k run --home "$CLOUD_HOME"
```

## When the public URL changes

A restart of a short-lived tunnel can change the public URL. Every agent and client that points at the hub must use the new URL. This includes the MCP endpoint configuration and, if used, the webhook registration.

If you leave an old URL in place, requests hit a dead hostname. The tunnel provider can then return HTTP 403 or 404. That response comes from the provider, not from 999K.

tunnl.gg is behind Cloudflare. A request with no or an unusual `User-Agent` can get HTTP 403 with `error code: 1010`. That is Cloudflare, not the hub.

The Bearer token does not change when the URL changes.

## Tell the tunnel apart from the app

Send the same authenticated MCP request to the local hub and the public URL. If only the public request fails, verify the public hostname, proxy path and Host rewrite, and whether the connector sends the same Bearer header.

## Run a private hub with one LaunchAgent

Use this procedure for an existing shared hub with private Dot or Codex Cloud tunnels, an email relay, or both. One macOS LaunchAgent runs `999k run`. The runner owns the same loopback hub and queue. It starts the selected companions only after the hub reports its profiles, owns the port, and responds to a protocol probe.

A failed companion retries without replacing the hub or its siblings. The retry delay grows to at most 30 seconds. When the hub stops, the runner stops and reaps the companions before it stops a still-running hub. If the hub must restart, the runner checks the queue identity and proves readiness again before it starts the companions.

This mode does not install a service or migrate an existing service. It does not select a public URL or change the separate `init --apply` workflow. Do not combine the two workflows on the same home or port. Existing public external and SSH modes do not change.

Updated hub binaries use one owner-private `.hub.lock` file next to the canonical SQLite queue. Another updated hub cannot open a listener for that queue, even on a different port. Do not delete the lock file or move the queue while a hub runs. Old binaries ignore the lock. Do not run them in parallel with the new hub.

### Prepare the files

1. Copy the [manifest example](../examples/launchd/companions.example.json) to the existing private hub home. Set mode 600 for the file and 700 for its parent. Keep it out of Git. Set `version` to 1. Run `stat -f '%d %i' <hub-home>/pigeonmq-cloud-queue.sqlite3`. Copy the two decimal numbers to `queue_identity.device` and `queue_identity.inode`, in that order. Omit unused roles. Select at least one role.
2. For each tunnel, use the approved installed client or its existing exec wrapper. Copy the existing arguments, starting with `run`. Use one JSON string for each argument. There is no shell expansion. Set `working_directory` and `home` to the existing private absolute paths. Set `path` to the existing `PATH` entries, in order, as separate absolute paths. The runner supplies only `HOME` and `PATH` to the tunnel. Keep the same tunnel ID, runtime config, header sources, and authentication settings. Use absolute `file:` references for runtime keys and header values, not inline values. Set `--mcp.server-url` to the exact hub loopback port and selected route. For health, use `127.0.0.1:0` and a different private URL file for each tunnel.
3. For the email relay, copy the existing absolute registration, cursor state, and provider command paths. Keep the same mailbox names and interval. The runner supplies the queue path from `--home`. This mode preserves the static `--mailbox` relay. It does not convert a managed `--config` relay or cursor.
4. Copy the [LaunchAgent example](../examples/launchd/pigeonmq.private-supervisor.plist). Replace the binary, home, port, and manifest paths. Use a durable installed binary outside the build directory. If the old runner uses bundled profiles, remove both `--profiles-file` and its path. Keep all programs in the foreground. A companion and its descendants must not leave their process group. Run `plutil -lint` on the copy.

`--companions-file` selects private companion mode. Do not pass `--env-file` or `--local-only` with it. This mode does not use the public tunnel environment variables. Do not point a trial at an active hub, its queue, or its tunnels.

### Replace old jobs during maintenance

1. Record the exact job labels, their loaded and disabled states, and the device and inode of the existing queue. Keep the old installed binary and private settings. Check which jobs use this hub. Do not include other hubs.
2. Stop the old reconnect job first. Stop the old tunnel clients and relay, then stop the old hub. Disable old restart paths. Move the old plist files to an owner-private archive outside `~/Library/LaunchAgents`. Unloading alone does not prevent their return at login. Verify those jobs and their children are gone. Verify no other listener uses this queue, including on a different port.
3. Place the new plist in `~/Library/LaunchAgents`. Use `launchctl bootstrap "gui/$(id -u)" <new-plist>` once. Keep the same home, port, queue, and route bindings. Do not run setup or reissue credentials.
4. Run `999k health --home <hub-home>`. Check that the hub is ready and the queue device and inode still match. For each tunnel, check the current client's local listener and then its `/readyz`. Match the listener PID to that role's current PID before using its health URL file. An old file or a running PID does not prove readiness. Use a read-only call from each intended app to check its mailbox access. Check relay delivery and Events separately. A healthy hub does not prove either.
5. To roll back, run `launchctl bootout "gui/$(id -u)/pigeonmq.private-supervisor"`. Archive the new plist outside `~/Library/LaunchAgents`. Verify its processes are gone and cannot restart before you restore the old plists and their saved disabled states. Restore the old installed binary and settings only if needed. Use the same current queue and relay cursor. Do not overwrite either with an older copy.

A queue identity mismatch prevents startup after a crash or login. If you intentionally move or restore the queue, first stop this LaunchAgent. Verify the intended queue and its current identity before you update the private manifest and restart. Do not update the identity to clear an unexplained mismatch.

The health report shows each selected role as `starting`, `running`, or `retrying`. `running` proves startup only. The health command exit code describes the hub and queue. It does not describe the tunnels or email delivery.

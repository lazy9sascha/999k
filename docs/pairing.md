# Pair an existing remote peer

Use a pairing code to hand an existing peer its mailbox credential over HTTPS. The code expires after 10 minutes and works once. Share the code privately. The peer keeps the returned Bearer in an owner-private file or its connector's secure credential store.

## Prepare the route

In [Orca setup](orca.md#connect-a-peer), select the peer to prepare its mailbox
and managed credential. Verify the route before issuing a code.

Publish the selected peer's two exact paths on the same HTTPS origin. Keep MCP Bearer authentication in place. Forward the pairing POST to the hub and keep its response out of proxy logs and caches.

| Peer ID | Pairing path | MCP path |
| --- | --- | --- |
| `muse` | `/muse/pair` | `/muse/mcp` |
| `grok-bot` | `/grok/pair` | `/grok/mcp` |
| `cue` | `/cue/pair` | `/cue/mcp` |
| `instinct` | `/instinct/pair` | `/instinct/mcp` |

Only publish the paths for the selected peer. For Dot, use the separate
[OAuth or private tunnel procedure](orca.md#connect-a-peer).

## Offer and redeem

On the hub Mac, run this in an owner-controlled terminal. The command uses the selected managed mailbox's saved credential and verifies its exact HTTPS MCP endpoint and pairing path from your private `orca-ingress.json` file or your approved hosted enrollment. For a manually verified origin, pass `--origin https://radio.example.com` without a path. This form skips automatic route verification. For a separately configured credential, pass its private file path with `--bearer-file`.

```sh
999k pair offer muse
```

Set the displayed HTTPS origin in the receiving app’s private configuration. Copy the peer-specific instructions to its own conversation. Supply the one-time code using the app’s private enrollment flow. Use the secure form when Instinct offers one. The app can redeem the code by sending `{"code":"<one-time-code>"}` in an HTTPS POST to its pairing path. Require HTTPS, disable redirects, and keep the response out of logs. Check that the response reports the expected `mailbox`, exact `mcp_path`, and `token_type` of `Bearer`. Save `access_token` in an owner-only file or secure credential store.

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

## Check the existing conversation

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

## Verify locally with synthetic data

In a local terminal outside an Orca agent session, build the CLI and run
the pairing test with Python 3 and OpenSSL. The test serves HTTPS on loopback with a temporary certificate. It uses fixture mailboxes, never prints the pairing code or Bearer, and checks rejection of wrong, consumed, and revoked codes, remote read and reply, and the original acknowledgement.

```sh
cargo build --locked --bin 999k
python3 tests/pairing_https.py target/debug/999k
```

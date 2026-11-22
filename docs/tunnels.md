# Tunnel URLs

The hub always listens on loopback. `run` has no default tunnel mode. You must choose how to expose the hub:

| Setting | Behavior |
| --- | --- |
| No env file and no tunnel variables | Refuse to start. Print `tunnel_mode_required` and exit. |
| `PIGEONMQ_TUNNEL=external` with `PIGEONMQ_PUBLIC_URL` | Skip SSH. Record the specified HTTPS origin as unverified. |
| `PIGEONMQ_TUNNEL=tunnl` without `PIGEONMQ_PUBLIC_URL` | Start a tunnl.gg SSH reverse tunnel. |
| `--local-only` | Loopback testing only. Ignore the tunnel variables and any env file. |

Set the variables in the runner's process environment or pass `--env-file PATH`. The runner does not combine the two sources or search for a `.env` file. A missing mode, an unknown mode, `external` without a URL, or a URL without `external` stops startup.

For private Dot or Codex Cloud access without a public URL, see the [OpenAI Secure MCP Tunnel example](../examples/openai-secure-tunnel/README.md).

## Use a private .env file

From the repository root, copy the [template](../.env.example) into your owner-only runtime folder (`$CLOUD_HOME`) if you do not already have a settings file:

```sh
(umask 077; cp .env.example "$CLOUD_HOME/.env")
```

Set your own HTTPS origin in that mode-600 file:

```
PIGEONMQ_TUNNEL=external
PIGEONMQ_PUBLIC_URL=https://pigeon.example.com
```

Use one unquoted `KEY=value` per line. The file accepts only the two named keys and requires `PIGEONMQ_TUNNEL`. Blank lines and whole-line `#` comments are allowed. The limit is 4 KiB of UTF-8 text. Do not include credentials, `export`, or shell expressions.

```sh
pigeonmq run --home "$CLOUD_HOME" --env-file "$CLOUD_HOME/.env"
```

If your checkout itself is owner-only, you can instead use `(umask 077; cp .env.example .env)` and `pigeonmq run --home "$CLOUD_HOME" --env-file .env`. Do not copy over an existing settings file.

## Use your own HTTPS origin

This is the recommended way to publish a public URL.
If you control a domain, choose a hostname such as `pigeon.example.com`.
Use any reverse tunnel or HTTPS proxy that can forward to the hub on the same machine.
The service that terminates public TLS can read request headers, including Bearer tokens, and message bodies.
For a public Cloudflare Tunnel, Cloudflare terminates public TLS. TLS to the origin does not hide this data from Cloudflare.
A pass-through provider cannot read this data if TLS ends at an endpoint you control and no intermediary decrypts the traffic.
A domain by itself does not create a route.
Configure DNS and TLS with your provider, and manage the proxy or tunnel separately from PigeonMQ.

### Prepare the route

In your proxy or tunnel, map the hostname to `http://127.0.0.1:54843`. If your runner uses another `--port`, use that port in both the service URL and the upstream `Host` header. For the default port:

- Set the upstream `Host` header to `127.0.0.1:54843`. Do not forward the public hostname as the upstream `Host`.
- Preserve the [MCP paths](architecture.md#routes) and the `Authorization: Bearer` header. Publish only the MCP paths you need. Do not publish the leased receiver or legacy routes.
- Keep the hub on loopback and leave Bearer, mailbox, Host, and Origin checks on. The hub rejects browser requests with `Origin` or `Sec-Fetch-Site` headers.

For Cloudflare, see the [named tunnel example](../examples/cloudflare/README.md) for creation, path and Host settings, and separate credential files.

### Switch the runner

Use the [private env file](#use-a-private-env-file) above, or set the same process variables directly:

```sh
PIGEONMQ_TUNNEL=external PIGEONMQ_PUBLIC_URL=https://pigeon.example.com \
  pigeonmq run --home "$CLOUD_HOME"
```

Use the default HTTPS port. Do not include a path beyond an optional trailing slash, credentials, a query, or a fragment. When replacing a running instance, keep its home, port, and profile or policy settings. Stop the old runner before starting the new one. If a supervisor starts the hub, update that service's existing command rather than starting a second runner against the same home.

This setting starts the hub without SSH. It does not start the proxy, configure DNS, or test the route. The runner records the origin and connector endpoints only in the private `pigeonmq-cloud-state.json`. It reports `local_ready` and `remote_verified: false`, not `connected`.

### Verify and update the clients

1. Run `pigeonmq health --home "$CLOUD_HOME"` to check the local hub and queue without contacting the public hostname. Use a temporary private home and synthetic mail if rehearsing the switch.
2. Verify that you control the endpoint and trust the TLS terminator. It can read the Bearer token. Set the new URL and matching token in the connector's secure fields. Stop if it has no secure credential field. PigeonMQ does not configure clients or hand off credentials for external origins. See [credential custody](configuration.md#credential-custody-and-handoff).
3. In the updated connector, make a permitted read-only MCP call. For a manual MCP check, start with `initialize`, then send the negotiated version in the `MCP-Protocol-Version` header on later requests. A successful call proves that connector can reach the hub. It does not verify delivery or Events. The optional `health --probe-public` command checks each configured public MCP route. It sends no credentials. A proxy can stop the check before the request reaches PigeonMQ. `tunnel.routes` reports the result for each profile. A partial catalog does not confirm all routes. The command does not verify authenticated MCP. Its exit code still reports local readiness only.

If you keep the same hostname and path, clients can keep that URL. Otherwise, update each affected client separately. Dot or Codex clients using a separate OpenAI Secure MCP Tunnel do not switch automatically. To roll back, restore the previous runner settings and point affected clients at the current URL of their previous transport.

## Use tunnl.gg for testing

Use this only when you do not have a domain.
Set `PIGEONMQ_TUNNEL=tunnl` and do not set `PIGEONMQ_PUBLIC_URL`.
The runner then starts an SSH reverse tunnel to tunnl.gg.
You must place an SSH identity and a pinned host key in the runtime folder.
See [Open a tunnel](quickstart.md#3-open-a-tunnel).

Read these limits before you start:

- tunnl.gg decrypts traffic on its servers. See the [privacy policy](https://tunnl.gg/privacy.md).
- Free tunnels close after 2 hours idle or 24 hours. See [pricing](https://tunnl.gg/pricing).
- From 2026-12-01, `stable@` needs a free account.
- The service has no uptime guarantee.
- The [terms of service](https://tunnl.gg/terms) apply to that connection.

```sh
PIGEONMQ_TUNNEL=tunnl pigeonmq run --home "$CLOUD_HOME"
```

## When the public URL changes

A restart of a short-lived tunnel can change the public URL. Every agent and client that points at the hub must use the new URL. This includes the MCP endpoint configuration and, if used, the webhook registration.

If you leave an old URL in place, requests hit a dead hostname. The tunnel provider can then return HTTP 403 or 404. That response comes from the provider, not from PigeonMQ.

tunnl.gg is behind Cloudflare. A request with no or an unusual `User-Agent` can get HTTP 403 with `error code: 1010`. That is Cloudflare, not the hub.

The Bearer token does not change when the URL changes.

## Tell the tunnel apart from the app

Send the same authenticated MCP request to the local hub and the public URL. If only the public request fails, verify the public hostname, proxy path and Host rewrite, and whether the connector sends the same Bearer header.

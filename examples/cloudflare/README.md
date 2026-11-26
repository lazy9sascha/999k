# Cloudflare Tunnel for 999K

Create a named, remotely managed tunnel for an HTTPS hostname in a Cloudflare zone you control. Use the existing 999K hub and queue on the same Mac as `cloudflared`. This example uses the reserved hostname `pigeon.example.com`, port `54843`, and only the Muse MCP route. Replace them with the approved hostname, actual hub port, and selected path. For a private OpenAI app, see [Secure MCP Tunnel](../openai-secure-tunnel/README.md).

## 1. Create the tunnel and start the connector

Use the [Cloudflare dashboard](https://developers.cloudflare.com/tunnel/get-started/#create-a-tunnel): open **Networking → Tunnels**, create a named tunnel, and use its connector setup for your OS. You need permission to manage the target Cloudflare account and zone. This dashboard path does not require creating a management API token.

In the connector setup, obtain this tunnel's connector token. For an existing tunnel, **Overview → Add a replica** shows it. Put only the token in an owner-private mode-600 file outside Git. Use `cloudflared` 2025.4.0 or later for the [token-file option](https://developers.cloudflare.com/tunnel/reference/run-parameters/#token-file):

```sh
cloudflared tunnel run --token-file "$CONNECTOR_TOKEN_FILE"
```

Set `CONNECTOR_TOKEN_FILE` to that absolute file path. Keep the token out of chat and command arguments, including commands from the dashboard. The connector is a separate process. If this tunnel's connector is already healthy on the hub Mac, reuse it. Check the tunnel status in Cloudflare before continuing.

## 2. Publish only the MCP path

In your tunnel's **Routes** tab, add a **Published application**. Use these values for the example:

| Setting | Value |
| --- | --- |
| Hostname | `pigeon.example.com` |
| Path | Only `/muse/mcp` (use `^/muse/mcp$` for an exact ingress regex) |
| Service URL | `http://127.0.0.1:54843` |
| Additional application settings → HTTP → [HTTP Host Header](https://developers.cloudflare.com/tunnel/reference/origin-parameters/#httphostheader) | `127.0.0.1:54843` |

If you plan to issue a [Muse pairing code](../../docs/pairing.md#prepare-the-route),
add a second exact route for `/muse/pair` on the same origin. Use the same
service URL and Host override.

The route forwards the full path to the hub. If the port changes, update both the origin URL and the Host override. Keep the hub on loopback. An empty Path routes the entire hostname, including other paths mounted on a shared hub. Do not leave it empty for this example. If the dashboard cannot restrict the path, use a reviewed ingress rule or local proxy. Allow only the intended path before publishing. Do not expose the leased receiver or unrelated profiles. Check the hostname's proxied DNS record and HTTPS certificate.

Keep 999K's Bearer, mailbox, Host, and Origin checks on. Cloudflare terminates public TLS. It can read request headers, including Bearer tokens, and message bodies. TLS to the origin does not hide this data from Cloudflare. [Cloudflare Access](https://developers.cloudflare.com/cloudflare-one/access-controls/service-credentials/service-tokens/) can add a separate check when the intended client can supply its Access credentials as well as the 999K Bearer. Do not replace the 999K `Authorization` header with Access credentials or bypass an existing access policy during tests.

### Optional: create a new tunnel with the Cloudflare API

Instead of the dashboard, [create a scoped API token](https://developers.cloudflare.com/fundamentals/api/get-started/create-token/). Use **Cloudflare Tunnel: Edit** for the selected account. If the API also creates the DNS record, authorize **DNS: Edit** for only the target zone. Use a short-lived token where possible, not a Global API Key. Follow the [Cloudflare API setup procedure](https://developers.cloudflare.com/tunnel/get-started/): create the tunnel with `config_src: "cloudflare"`, configure ingress, then create the proxied CNAME to `<tunnel-id>.cfargotunnel.com`. Keep the API response private because it can contain the connector token.

For a **new, dedicated tunnel only**, the configuration request body can be:

```json
{
  "config": {
    "ingress": [
      {
        "hostname": "pigeon.example.com",
        "path": "^/muse/mcp$",
        "service": "http://127.0.0.1:54843",
        "originRequest": {"httpHostHeader": "127.0.0.1:54843"}
      },
      {"service": "http_status:404"}
    ]
  }
}
```

The configuration PUT replaces ingress. Do not apply this example to an existing tunnel. The management API token creates and configures resources. The separate connector token runs this tunnel. The 999K Bearer authenticates the client to its mailbox. Do not substitute one for another. Revoke a temporary management token after checking the setup.

## 3. Select the external origin in 999K

Follow [Switch the runner](../../docs/tunnels.md#switch-the-runner). Select `external` and set `RADIO999K_PUBLIC_URL` to the HTTPS origin, such as `https://pigeon.example.com`.
Keep the hub home, queue, port, and profile settings. Keep all tokens outside the 999K env file.
Run `cloudflared` separately.

## 4. Check each part before switching clients

1. Run `999k health --home "$CLOUD_HOME"` to check the local hub and queue. Separately check that Cloudflare sees the intended connector as healthy. A public MCP call tests the full route.
2. On the approved public path, send a JSON POST without the 999K Bearer. If the route uses Access, authenticate to Access with an approved client first. HTTP `401` with 999K's JSON `{"error":{"code":"unauthorized"}}` shows the request reached the hub's authentication check. A Cloudflare or Access error does not. Check that the route denies other paths.
3. In the intended MCP client, set the HTTPS endpoint, for example `https://pigeon.example.com/muse/mcp`, and the matching 999K Bearer in its secure field. Send `initialize` with `Accept: application/json, text/event-stream`. Use the returned version in `MCP-Protocol-Version` on later requests. Make a permitted read-only call such as `p-999k_inbox`. This proves access for that client, not Events or reply delivery.

If a request fails, compare the same authorized request on loopback. The hub requires a fixed-length JSON POST with one Content-Length at the origin, not chunked framing. Do not relax its checks. Restore the previous runner settings and client URL if the new route fails acceptance. See [tunnel troubleshooting](../../docs/troubleshooting.md#through-the-tunnel).

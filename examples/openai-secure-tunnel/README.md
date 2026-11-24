# OpenAI Secure MCP Tunnel for Dot or Codex Cloud

Connect one loopback mailbox to an authorized private OpenAI app. This example does not create a public URL, change DNS, or use Cloudflare or tunnl.gg. Use the [official tunnel-client release](https://github.com/openai/tunnel-client/releases/latest) and run it separately from the hub. For an existing private hub, you can also [run the clients under one LaunchAgent](../../docs/launchd.md).

## 1. Reuse or create a tunnel

Choose the intended Platform organization, ChatGPT workspace, and mailbox. Use a distinct tunnel binding for each mailbox. Callers through the same binding use the same 999K identity.

- **Reuse:** If an approved tunnel already belongs to this mailbox, check its organization and workspace associations and use its existing ID. Do not recreate it or request an Admin API key.
- **Create in Platform:** An operator with **Tunnels Read + Manage** can create the tunnel in [Platform tunnel settings](https://platform.openai.com/settings/organization/tunnels). Associate it with the intended organization and workspace. This path does not require issuing an Admin API key.
- **Create through the CLI:** Use a management key only for this step. The operator needs **Tunnels Read + Manage** and permission to issue an Admin API key. Supply `OPENAI_ADMIN_KEY` through your approved secret input, not as a literal argument or in this repository:

  ```sh
  tunnel-client admin tunnels create \
    --name "999K private mailbox" \
    --description "Secure MCP Tunnel for one mailbox" \
    --organization-id "$OPENAI_ORGANIZATION_ID" \
    --workspace-id "$CHATGPT_WORKSPACE_ID"
  ```

Keep the tunnel ID private and wait for a new tunnel to become active. If the owner issued a temporary Admin API key, revoke it after the tunnel becomes active. Never put it in the running client's environment.

Create or reuse a separate [runtime API key](https://platform.openai.com/settings/organization/api-keys) with **Tunnels Read + Use**. Save it to an owner-only file. The app operator also needs permission to select the tunnel and add a private MCP app in the target workspace. See the [OpenAI permissions and association guide](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels).

## 2. Prepare the local mailbox

Use the exact hub port and route for the selected mailbox:

| Mailbox | Default route | Credential export app |
| --- | --- | --- |
| Dot | `/dot/mcp` | `dot` |
| Codex Cloud | `/codex/mcp` | `codex-cloud` |

If a private manifest remaps the route, use that path. Keep files in owner-only directories outside Git. Use mode 600 for credential files and use absolute paths.

- **Dot:** Use an existing queue, an active Dot registration, and the matching Dot policy. If needed, follow [the Dot setup](../../docs/configuration.md#dot-through-a-private-secure-mcp-tunnel) to prepare storage. For an approved new Dot binding in an existing shared manifest, use the command below. This branch does not provision a bearer. If the binding already exists, retain it.
- **Codex Cloud:** If the mailbox is not prepared, run `999k setup-codex --home "$CLOUD_HOME" --peer "$APPROVED_PEER"` for mailbox-only use. If the hub uses a manifest, it must also [select the Codex profile](../../docs/configuration.md#select-existing-hub-profiles).

```sh
# Dot only: add the approved binding to an existing private manifest.
999k dot-prepare --queue "$CLOUD_HOME/pigeonmq-cloud-queue.sqlite3" \
  --auth-policy "$DOT_POLICY" --profiles-file "$PROFILES_FILE"
```

Use the existing hub if it serves the selected route. Set `PIGEON_PORT` and `MCP_ROUTE` to its loopback port and mailbox path.
For a new private Codex hub, follow [Codex Cloud operation](../../docs/configuration.md#codex-cloud-operation).

For a conventional home policy with matching Keychain custody, set `RADIO999K_APP` to the export app in the table:

```sh
999k credentials --home "$CLOUD_HOME" --app "$RADIO999K_APP" export \
  --token-file "$PIGEON_HEADER_FILE" --format authorization-header
```

For a custom policy, use a private file with the matching header value: `Bearer`, one space, then the token, without an `Authorization:` prefix. Do not borrow another mailbox's bearer. This credential authenticates to 999K, not to OpenAI.

## 3. Start the tunnel client and attach the app

If the approved client already serves the exact route with the right mailbox header, keep it. Do not start a second client for the same tunnel.

In a separate terminal on the hub machine, use the approved `TUNNEL_ID`, private `RUNTIME_KEY_FILE`, mailbox header file, exact `PIGEON_PORT` and `MCP_ROUTE`, and an unused private path for `HEALTH_URL_FILE`. Disabling OAuth discovery for this static-header setup does not disable the hub's Bearer check.

```sh
tunnel-client run \
  --control-plane.tunnel-id "$TUNNEL_ID" \
  --control-plane.api-key "file:$RUNTIME_KEY_FILE" \
  --mcp.server-url "http://127.0.0.1:$PIGEON_PORT$MCP_ROUTE" \
  --mcp.extra-headers "Authorization: file:$PIGEON_HEADER_FILE" \
  --mcp.discovery-extra-headers "Authorization: file:$PIGEON_HEADER_FILE" \
  --mcp.oauth-discovery disabled \
  --health.listen-addr 127.0.0.1:0 --health.url-file "$HEALTH_URL_FILE"
```

1. If `run` supervises the hub, check it with `999k health --home "$CLOUD_HOME"`. Separately check the client's `/readyz` at the loopback URL in `HEALTH_URL_FILE`.
2. In the intended workspace, [create or select the private MCP app](https://developers.openai.com/api/docs/guides/custom-mcp-server). Select **Connection: Tunnel** and the same tunnel ID. Use no additional OAuth only for the approved private app with the enforced upstream Bearer configuration above. If the workspace requires OAuth, this example does not apply. Do not publish the app. For Codex, use the supported Cloud product connection and the intended conversation, not a raw tunnel URL.
3. From that Dot or Codex Cloud conversation, run a permitted read-only call such as `radio999k_inbox`. A successful call shows that this caller can access the mailbox. Discovery and tunnel readiness alone do not. Events, reply delivery, and automatic startup require separate tests.

See the [tunnel-client configuration reference](https://github.com/openai/tunnel-client/blob/master/docs/configuration.md) for the runtime flags. 999K does not create OpenAI tunnels, issue OpenAI keys, or complete workspace consent.

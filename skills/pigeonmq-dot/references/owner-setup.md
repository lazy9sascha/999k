# Dot owner setup

Owner only. Keep the policy and both credentials outside Git, in owner-only directories, mode 600.

Use a private Dot-only [profile manifest](../../../docs/configuration.md#dot-through-a-private-secure-mcp-tunnel). Set `PROFILES_FILE` to its absolute path.

```sh
pigeonmq-cloud dot-prepare --queue "$QUEUE" --auth-policy "$DOT_POLICY"
pigeonmq-cloud serve-hub --queue "$QUEUE" --profiles-file "$PROFILES_FILE" \
  --port "$DOT_PORT" --continuous
```

```sh
tunnel-client run \
  --control-plane.tunnel-id "$DOT_TUNNEL_ID" \
  --control-plane.api-key "file:$RUNTIME_KEY_FILE" \
  --mcp.server-url "http://127.0.0.1:$DOT_PORT/dot/mcp" \
  --mcp.extra-headers "Authorization: file:$DOT_HEADER_FILE" \
  --mcp.discovery-extra-headers "Authorization: file:$DOT_HEADER_FILE" \
  --mcp.oauth-discovery disabled \
  --health.listen-addr 127.0.0.1:0 \
  --health.url-file "$HEALTH_URL_FILE"
```

- Use absolute paths. The header file holds the complete `Bearer TOKEN` value.
- These commands stay in the foreground. They do not start again after a reboot.
- Check the tunnel readiness endpoint. Local readiness does not prove ChatGPT can reach the hub.
- **None** in the ChatGPT form removes OAuth at the ChatGPT layer only. The hub still checks the Bearer header.
- Use this tunnel for the Dot identity only. Do not publish the plugin or share it with another workspace.
- Organization-managed Dot workspaces are not supported.

Official docs: OpenAI tunnel-client `docs/configuration.md` and the ChatGPT connection guide (linked from `docs/configuration.md`).

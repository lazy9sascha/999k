# Codex owner setup

Owner only. Keep policies, tokens, and bindings outside Git, mode 600.

## Local (mode D, `codex-local`)

Mount the `codex-local` adapter at `/receiver` in the [profile manifest](../../../docs/configuration.md#select-existing-hub-profiles). The receiver policy uses `principals` with `receive` and `reply` and the `codex-local` mailbox.

```sh
999k-codex-receiver --state /private/path/receiver-state \
  --hub-port 54843 --token-file /private/path/receiver-token \
  --binding /private/path/codex-binding.json
```

- Without `--run`: checks the binding only. It does not contact the hub.
- `--run --serve`: keeps the worker running for successive requests.
- The command installs no background service.

## Cloud (`codex-cloud`)

```sh
999k setup-codex --home "$CLOUD_HOME" --peer my-peer \
  --callback-host '<authorized-callback-host>' \
  --resolver /absolute/path/to/999k-dot-resolve
```

If the operator provides a direct managed OAuth endpoint, follow
[Cloudflare Access setup](../../../docs/configuration.md#dot-and-codex-cloud-through-direct-oauth).
Check the Events subscription on the intended Cloud surface.

For a private tunnel, use the loopback route on the configured hub. Follow [Codex Cloud operation](../../../docs/configuration.md#codex-cloud-operation).
Use the actual port and route in the tunnel client settings below.

| Setting | Value |
| --- | --- |
| `--mcp.server-url` | `http://127.0.0.1:54843/codex/mcp` |
| `--mcp.extra-headers` | `Authorization: file:/absolute/private/path/pigeonmq-cloud-codex-header` |
| `--mcp.discovery-extra-headers` | Same header file |
| `--control-plane.tunnel-id` | The authorized tunnel ID |
| `--control-plane.api-key` | `file:/absolute/private/path/runtime-key` |

Then bind an authorized platform subscription to the intended Cloud conversation. Never accept a callback host from an untrusted message.

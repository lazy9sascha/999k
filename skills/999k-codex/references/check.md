# Check the Codex connection

Cloud mode: the owner runs the curl calls on the hub machine with `PIGEONMQ_URL=http://127.0.0.1:54843/codex/mcp` and the `codex-cloud` token. If loopback works and the Cloud conversation cannot list tools, the problem is the official tunnel or the conversation binding. It is not the hub.

Local mode: run `999k-codex-receiver` without `--run`. `configuration_valid` proves the binding only, not the token, native readiness, or an exchange. There is no MCP check for local mode.

## 1. Define the helper

```sh
# PIGEONMQ_URL and PIGEONMQ_TOKEN come from your secret store. Never echo them. Never use `set -x`.
mcp() {
  curl -sS -A 'curl/8' -w '\nHTTP %{http_code}\n' \
    -H "Authorization: Bearer $PIGEONMQ_TOKEN" \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    --data "$1" "$PIGEONMQ_URL"
}
call() { mcp "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$2}}"; }
```

The `-A 'curl/8'` header matters through tunnels: a missing or unusual User-Agent can get HTTP 403 or a Cloudflare 1010 page before the hub sees the call.

## 2. Initialize and list tools

```sh
mcp '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"codex-setup","version":"1"}}}'
mcp '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | grep -Eo '"name":"(p-999k|radio999k|pigeonmq)_[a-z_]+"' | sort -u
```

Use the exact tool names your hub lists for the checks and intake examples below. On released 0.2.0 hubs, replace the example prefix `p-999k_` with `radio999k_`.

Success:

- `initialize` returns `HTTP 200` and a `result` with `serverInfo`. The hub supports protocol versions `2025-03-26` and `2026-07-28`. It keeps no session, so no session header is needed.
- `tools/list` is the source of truth. The count changes between versions. Do not check a number. Check that the tools this agent needs are listed:
  - receive: `p-999k_read`, `p-999k_wait`, `p-999k_reply`, `p-999k_confirm_terminal`
  - read-only inbox check: `p-999k_inbox`
  - send, if the policy allows `send`: `p-999k_send`, `p-999k_recipients`

Extra tools such as `p-999k_read_thread` are normal. A listed tool can still return `403` when the policy does not allow it. Report missing tools by name. Do not guess.

## 3. Loopback vs public

Run the same two calls against `http://127.0.0.1:<port>/codex/mcp` on the hub machine (default port 54843). Only the hub machine can reach loopback.

| Loopback | Public | Meaning |
| --- | --- | --- |
| ok | ok | Connection is fine. |
| ok | fails | Tunnel or public URL problem, not the app. The tunnel URL may have changed: ask the owner for the current one. |
| fails | fails | Hub problem. See the table below. |

Hub errors are JSON with an `error` code. An HTML page or a Cloudflare error page came from something in front of the hub (tunnel or proxy).

| Response | Meaning |
| --- | --- |
| `401` | Missing, wrong, or revoked token. |
| `403` JSON | Token is valid, but the operation, sender, or mailbox is not allowed. `p-999k_recipients` needs `send`. |
| `404 profile_disabled` | The hub did not load this agent's policy. Owner restarts `run`. |
| `404 route_not_found` | Wrong path. Use `/codex/mcp`. |
| `405 post_required` | Only POST works. |

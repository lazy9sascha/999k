# Check the Muse connection

Run this from Muse's VM. Read the URL and token from the secure store into `PIGEONMQ_URL` and `PIGEONMQ_TOKEN`. The production client has no retry. The tunnel can drop connections. Retry a failed call once.

## 1. Define the helper

```sh
# PIGEONMQ_URL and PIGEONMQ_TOKEN come from your secret store. Never echo them. Never use `set -x`.
mcp() {
  curl -sS -A 'curl/8' -w '\nHTTP %{http_code}\n' \
    -H "Authorization: Bearer $PIGEONMQ_TOKEN" \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2025-03-26' --data "$1" "$PIGEONMQ_URL"
}
call() { mcp "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$2}}"; }
```

The `-A 'curl/8'` header matters through tunnels: a missing or unusual User-Agent can get HTTP 403 or a Cloudflare 1010 page before the hub sees the call.

## 2. Initialize and list tools

```sh
mcp '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"muse-setup","version":"1"}}}'
mcp '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | grep -o '"name":"radio999k_[a-z_]*"' | sort -u
```

Success:

- `initialize` returns `HTTP 200` and a `result` with `serverInfo`. The hub supports protocol versions `2025-03-26` and `2026-07-28`. It keeps no session, so no session header is needed. Use a string or signed 64-bit integer as the request ID.
- `tools/list` is the source of truth. The count changes between versions. Do not check a number. Check that the tools this agent needs are listed:
  - receive: `radio999k_read`, `radio999k_wait`, `radio999k_reply`, `radio999k_confirm_terminal`
  - read-only inbox check: `radio999k_inbox`
  - send, if the policy allows `send`: `radio999k_send`, `radio999k_recipients`

Extra tools such as `radio999k_read_thread` are normal. A listed tool can still return `403` when the policy does not allow it. Report missing tools by name. Do not guess.

## 3. Loopback vs public

Run the same two calls against `http://127.0.0.1:<port>/muse/mcp` on the hub machine (default port 54843). Only the hub machine can reach loopback.

| Loopback | Public | Meaning |
| --- | --- | --- |
| ok | ok | Connection is fine. |
| ok | fails | Tunnel or public URL problem, not the app. The tunnel URL may have changed: ask the owner for the current one. |
| fails | fails | Hub problem. See the table below. |

Authentication and tool errors contain JSON. rmcp can also return plain text for invalid HTTP or malformed input. An HTML page came from a tunnel or proxy.

| Response | Meaning |
| --- | --- |
| `401` | Missing, wrong, or revoked token. |
| `403` JSON | Token is valid, but the operation, sender, or mailbox is not allowed. `radio999k_recipients` needs `send`. |
| `404 profile_disabled` | The hub did not load this agent's policy. Owner restarts `run`. |
| `404 route_not_found` | Wrong path. Use `/muse/mcp`. |
| `405 post_required` | Only POST works. |
| `406 Not Acceptable` | Send both JSON and event-stream in the `Accept` header. |

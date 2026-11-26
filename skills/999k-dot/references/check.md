# Check the Dot connection

For direct OAuth, use the approved ChatGPT app to run a permitted mailbox
tool after sign-in. Check the Access binding and exact HTTPS path if the app
cannot load tools.

For the private tunnel, check the hub first. On the hub Mac, set
`PIGEONMQ_URL=http://127.0.0.1:$DOT_PORT/dot/mcp` and load
`PIGEONMQ_TOKEN` from the private Dot credential. Then run the checks below.
In ChatGPT, use the private app to call `p-999k_recipients` if the mailbox
can send, or a read-only mailbox tool if it cannot.

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

## 2. Initialize and list tools

```sh
mcp '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"dot-setup","version":"1"}}}'
mcp '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | grep -Eo '"name":"(p-999k|radio999k|pigeonmq)_[a-z_]+"' | sort -u
```

Use the exact tool names your hub lists for the checks and intake examples below. On released 0.2.0 hubs, replace the example prefix `p-999k_` with `radio999k_`.

Success:

- `initialize` returns `HTTP 200` and a `result` with `serverInfo`. The hub supports protocol versions `2025-03-26` and `2026-07-28`. It keeps no session, so no session header is needed.
- Check `tools/list` for the names of the tools this agent needs.
  - receive: `p-999k_read`, `p-999k_wait`, `p-999k_reply`, `p-999k_confirm_terminal`
  - read-only inbox check: `p-999k_inbox`
  - send, if the policy allows `send`: `p-999k_send`, `p-999k_recipients`

Extra tools such as `p-999k_read_thread` are normal. A listed tool can still return `403` when the policy does not allow it. Report missing tools by name. Do not guess.

## 3. Compare the hub and the app

If loopback fails, check the hub response below. If loopback works and the
app fails, check the tunnel association or the OAuth configuration for the
transport you selected. A successful app call verifies access.

Hub errors are JSON with an `error` code. An HTML page or a Cloudflare error page came from something in front of the hub (tunnel or proxy).

| Response | Meaning |
| --- | --- |
| `401` | Missing, wrong, or revoked token. |
| `403` JSON | Token is valid, but the operation, sender, or mailbox is not allowed. `p-999k_recipients` needs `send`. |
| `404 profile_disabled` | The hub did not load this agent's policy. Owner restarts `run`. |
| `404 route_not_found` | Wrong path. Use `/dot/mcp`. |
| `405 post_required` | Only POST works. |

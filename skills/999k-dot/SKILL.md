---
name: 999k-dot
description: Set up Dot (ChatGPT) on a 999K hub from start to finish. Connects the /dot/mcp mailbox through the official OpenAI Secure MCP Tunnel, checks it, sets up MCP Events intake, and reports in one code block. Use when connecting Dot to a hub, or when its tunnel, mailbox, or Events stop working. Also use when the owner asks to check its 999K inbox.
---

# 999K for Dot (ChatGPT)

Dot reaches the hub through an official OpenAI Secure MCP Tunnel by tunnel ID, not a public tunnel URL. Do the steps in order. If a step fails, stop and go to step 5.

## 1. Inputs

| Input | Where it comes from |
| --- | --- |
| Hub route | `http://127.0.0.1:<port>/dot/mcp` on the hub machine. |
| Dot Bearer token | The owner keeps it in a mode-600 header file (`Bearer TOKEN`). Never in chat, 999K messages, logs, or Git. |
| Tunnel ID and runtime key | The owner. The runtime key is a separate credential that authenticates the tunnel client, not the mailbox. |
| Mailbox | `dot`. The token fixes it. |
| Test message id | The owner or a peer sends one test message to `dot` and gives you its id. |

## 2. Owner does

- Prepares Dot storage. Starts a Dot-only loopback listener. Starts the official tunnel client with file references for both credentials.
- In the intended personal ChatGPT account: opens the private custom MCP form. Selects **Tunnel**. Enters the tunnel ID. Selects **None** for auth. Accepts the risk notice. Creates and connects the plugin.

Read references/owner-setup.md when the owner needs the exact commands.

## 3. Check the connection

Read references/check.md when you reach this step.
Success: loopback `tools/list` includes the tools listed in references/check.md. Dot's plugin can call `radio999k_recipients`.
Report transport, delivery, and automatic wake as separate results. A tool list proves discovery only.

## 4. Set up message intake (MCP Events)

The hub sends MCP Events on the same `/dot/mcp` route. An event is a nudge, not the message body. Do not poll ChatGPT as the wake path.
Read references/intake.md when you reach this step. Then read the test message by id. Then reply to it.

## 5. Report

Reply with ONE fenced code block and nothing else:

```text
agent: dot
endpoint_host: <host only, or "redacted">
initialize_ok: yes | no (HTTP code)
tools_missing: none | <names>
loopback_vs_public: both ok | loopback only | both fail | not checked
intake: MCP Events via Secure MCP Tunnel | not set up (why)
test_message_id: <id> | none (read: yes/no, reply: yes/no)
blockers: none | <list>
unverified: <anything you could not confirm>
```

## Check the inbox (read-only)

Use `radio999k_inbox` with `{}` for the latest received mail. To search sent and received mail, pass `{"scope":"all","query":"build log"}`.
Use `radio999k_read` for a received body or `radio999k_read_thread` with the listed `conversation_id` for the conversation.
Keep browsing read-only. Use `radio999k_wait` for pending work. Reserve `radio999k_reply` and `radio999k_confirm_terminal` for actions that should acknowledge a message.
See [browse and search mail](../../docs/setup-skills.md#browse-and-search-mail) for paging.
`403` means the policy does not allow the read.

## Docs

Deeper detail: `docs/configuration.md#dot-through-a-private-secure-mcp-tunnel`, `docs/agents.md`.

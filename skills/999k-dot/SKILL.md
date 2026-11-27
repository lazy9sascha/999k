---
name: 999k-dot
description: Connect Dot (ChatGPT) to its 999K mailbox through protected HTTPS OAuth or a private Secure MCP Tunnel. Verify the mailbox and check optional MCP Events. Use when setting up Dot, diagnosing its connection, or checking its 999K inbox.
---

# 999K for Dot (ChatGPT)

Use the transport the owner selected. A direct connection uses a protected
HTTPS `/dot/mcp` endpoint. A private connection uses an OpenAI Secure MCP
Tunnel. If a setup step fails, record its error in step 5.

## 1. Inputs

| Input | Where it comes from |
| --- | --- |
| Direct OAuth | Exact protected HTTPS URL from the operator. Approve it in ChatGPT. |
| Private tunnel | Tunnel ID and runtime key from the owner. The client forwards the hub's private Dot Bearer. |
| Dot Bearer | The owner keeps it in a private file. Keep it out of chat, queue messages, logs, and Git. |
| Mailbox | `dot`. The token fixes it. |
| Test message ID | The owner or a peer sends one synthetic request to `dot`. |

## 2. Configure the private app

Choose the direct or tunnel procedure in [Dot owner setup](references/owner-setup.md).

## 3. Check the connection

Follow [Check the Dot connection](references/check.md). Report discovery,
message delivery, and automatic wake separately.

## 4. Read and reply

Read the synthetic request by ID and send one linked reply. If the app has
an authorized subscription, test [MCP Events](references/intake.md) separately.

## 5. Report

Report the result in one fenced code block.

```text
agent: dot
transport: direct OAuth | private tunnel
initialize_ok: yes | no (HTTP code)
tools_missing: none | <names>
chatgpt_call: passed | failed | not checked
intake: MCP Events | prompted check | unverified
test_message_id: <id> | none (read: yes/no, reply: yes/no)
blockers: none | <list>
unverified: <anything you could not confirm>
```

## Check the inbox (read-only)

Use `p-999k_inbox` with `{}` for the latest received mail. To search sent and received mail, pass `{"scope":"all","query":"build log"}`.
Use `p-999k_read` for a received body or `p-999k_read_thread` with the listed `conversation_id` for the conversation.
Keep browsing read-only. Use `p-999k_wait` for pending work. Reserve `p-999k_reply` and `p-999k_confirm_terminal` for actions that should acknowledge a message.
See [browse and search mail](../README.md#browse-and-search-mail) for paging.
`403` means the policy does not allow the read.

## Docs

[Dot connection options](../../docs/orca.md#connect-a-peer) and
[connection evidence](../../docs/agents.md#deployment-matrix).

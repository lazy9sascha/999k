---
name: 999k-grok
description: Set up Grok Bot on a 999K hub from start to finish. Connects the /grok/mcp mailbox, checks it, sets up the webhook wake routine, and reports in one code block. Use when connecting Grok Bot to a hub, or when its mailbox or wake stops working. Also use when the owner asks to check its 999K inbox.
---

# 999K for Grok Bot

Do the steps in order. If a step fails, stop and go to step 5.

## 1. Inputs

| Input | Where it comes from |
| --- | --- |
| `PIGEONMQ_URL` | The owner gives the hub URL ending in `/grok/mcp`. Prefer a stable hostname on the owner's domain. A short-lived tunnel URL can change. |
| `PIGEONMQ_TOKEN` | The owner puts the Bearer token in Grok Bot's secret store. Never in chat, 999K messages, logs, or Git. |
| Mailbox | `grok-bot`. The token fixes it. |
| Test message id | The owner or a peer sends one test message to `grok-bot` and gives you its id. |

If an input is missing, stop and report it as a blocker.

## 2. Owner does

Only steps that need the owner's UI or a secret value wait for the owner:

- Prepares the GrokBot mailbox. Uses [pairing](../../docs/pairing.md) or puts
  the matching Bearer in GrokBot’s secure credential store.
- For webhook wake, configures the routine and [stores its credentials on
  the hub](../../docs/quickstart.md#5-optional-grok-bot-wake).

After configuring webhook wake, restart the existing hub on its Mac.

## 3. Check the connection

Read [connection checks](references/check.md) when you reach this step. It has the copy-paste curl calls.
Success: `initialize` returns HTTP 200 and `tools/list` includes the tools listed in [connection checks](references/check.md). Do not check a count.

## 4. Set up message intake (webhook wake)

The hub wakes you with a webhook. You then read and reply with the mailbox tools.
Read [message intake](references/intake.md) when you reach this step. It has the routine text, tool arguments, and wake status checks.
Then read the test message by id. Then reply to it.

## 5. Report

Report the result in one fenced code block.

```text
agent: grok
endpoint_host: <host only, or "redacted">
initialize_ok: yes | no (HTTP code)
tools_missing: none | <names>
loopback_vs_public: both ok | loopback only | both fail | not checked
intake: webhook routine | not set up (why)
test_message_id: <id> | none (read: yes/no, reply: yes/no)
blockers: none | <list>
unverified: <anything you could not confirm>
```

## Check the inbox (read-only)

Use `p-999k_inbox` with `{}` for the latest received mail. To search sent and received mail, pass `{"scope":"all","query":"build log"}`.
Use `p-999k_read` for a received body or `p-999k_read_thread` with the listed `conversation_id` for the conversation.
Keep browsing read-only. Use `p-999k_wait` for pending work. Reserve `p-999k_reply` and `p-999k_confirm_terminal` for actions that should acknowledge a message.
See [browse and search mail](../../docs/setup-skills.md#browse-and-search-mail) for paging.
`403` means the policy does not allow the read.

## Docs

Deeper detail: `docs/agents.md`, `docs/quickstart.md`, `docs/troubleshooting.md`, `docs/tunnels.md`.

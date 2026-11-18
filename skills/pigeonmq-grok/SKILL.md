---
name: pigeonmq-grok
description: Set up Grok Bot on a PigeonMQ hub from start to finish. Connects the /grok/mcp mailbox, checks it, sets up the webhook wake routine, and reports in one code block. Use when connecting Grok Bot to a hub, or when its mailbox or wake stops working. Also use when the owner asks to check its PigeonMQ inbox.
---

# PigeonMQ for Grok Bot

Do the steps in order. If a step fails, stop and go to step 5.

## 1. Inputs

| Input | Where it comes from |
| --- | --- |
| `PIGEONMQ_URL` | The owner gives the hub URL ending in `/grok/mcp`. Prefer a stable hostname on the owner's domain. A short-lived tunnel URL can change. |
| `PIGEONMQ_TOKEN` | The owner puts the Bearer token in Grok Bot's secret store. Never in chat, Pigeon, logs, or Git. |
| Mailbox | `grok-bot`. The token fixes it. |
| Test message id | The owner or a peer sends one test message to `grok-bot` and gives you its id. |

If an input is missing, stop and report it as a blocker.

## 2. Owner does

Only steps that need the owner's UI or a secret value wait for the owner:

- Creates the Grok Bot policy and token on the hub. Hands over the token with `handoff --app grok-bot --copy-token` into Grok Bot's secret store.
- After step 4, puts the routine's webhook URL and auth header into the hub files.

Restarting `run` after that is a local step. If you have shell access on the hub machine and the owner asked for this setup, run it yourself. Otherwise ask the owner.

## 3. Check the connection

Read references/check.md when you reach this step. It has the copy-paste curl calls.
Success: `initialize` returns HTTP 200 and `tools/list` includes the tools listed in references/check.md. Do not check a count.

## 4. Set up message intake (webhook wake)

The hub wakes you with a webhook. You then read and reply with the mailbox tools.
Read references/intake.md when you reach this step. It has the routine text, tool arguments, and wake status checks.
Then read the test message by id. Then reply to it.

## 5. Report

Reply with ONE fenced code block and nothing else:

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

If the owner only asks to check the inbox, call `pigeonmq_inbox` for metadata only.
If you need a body, call `pigeonmq_read` with that `message_id`.
Do not call `pigeonmq_reply` or `pigeonmq_confirm_terminal`. Those tools acknowledge the message.
`403` means the policy does not allow the read.

## Docs

Deeper detail: `docs/agents.md`, `docs/quickstart.md`, `docs/troubleshooting.md`, `docs/tunnels.md`.

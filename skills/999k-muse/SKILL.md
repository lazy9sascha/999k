---
name: 999k-muse
description: Set up Muse on a 999K hub from start to finish. Connects the /muse/mcp mailbox, checks it, sets up a 2-minute self-poll from the home directory, and reports in one code block. Use when connecting Muse to a hub, or when its mailbox or polling stops working, or when the owner asks to check its 999K inbox.
---

# 999K for Muse

Do the steps in order. If a step fails, stop and go to step 5.

## 1. Inputs

| Input | Where it comes from |
| --- | --- |
| `PIGEONMQ_URL` | The owner gives the hub URL ending in `/muse/mcp`. Prefer a stable hostname on the owner's domain. A short-lived tunnel URL can change. |
| `PIGEONMQ_TOKEN` | The owner puts the Bearer token in Muse's secure credential store, attached only to the hub host. Never in chat, 999K messages, logs, or Git. |
| Mailbox | `muse`. The token fixes it. |
| Test message id | The owner or a peer sends one test message to `muse` and gives you its id. |

If an input is missing, stop and report it as a blocker.

## 2. Owner does

- Creates the Muse policy and token on the hub. Hands over the token with `handoff --app muse --copy-token` into Muse's secure credential store.
- Adds the hub as a custom connector in Muse's workspace.
- Approves each outbound `radio999k_send` and `radio999k_reply`. Muse needs per-action approval for these.

These need the owner's UI, approval, or a secret value. Ask the owner. Then wait. Steps 3 and 4 run in your own shell. They do not wait for the owner.

## 3. Check the connection

Read references/check.md when you reach this step. It has the copy-paste curl calls.
Success: `initialize` returns HTTP 200 and `tools/list` includes the tools listed in references/check.md. Do not check a count.

## 4. Set up message intake (self-poll every 2 minutes)

The hub cannot wake Muse. Muse polls itself every 2 minutes from files under the home directory, never `/etc` or `/usr/bin` (VM replace wipes those).
Read references/intake.md when you reach this step. It has the poll script, schedule, and tool arguments.
Then read the test message by id. Then reply to it.

## 5. Report

Reply with ONE fenced code block and nothing else:

```text
agent: muse
endpoint_host: <host only, or "redacted">
initialize_ok: yes | no (HTTP code)
tools_missing: none | <names>
loopback_vs_public: both ok | loopback only | both fail | not checked
intake: self-poll every 2 min (script path) | not set up (why)
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

Deeper detail: `docs/agents.md`, `docs/setup-skills.md`, `docs/troubleshooting.md`, `docs/tunnels.md`.

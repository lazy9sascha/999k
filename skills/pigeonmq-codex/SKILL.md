---
name: pigeonmq-codex
description: Set up Codex on a PigeonMQ hub from start to finish, in one of two modes. Local mode D uses a native worker for mailbox codex-local. Cloud mode uses /codex/mcp with MCP Events for mailbox codex-cloud. Checks the connection and reports in one code block. Use when connecting Codex to a hub, or when either mode stops working, or when the owner asks to check its PigeonMQ inbox.
---

# PigeonMQ for Codex

Pick one mode. Keep the two mailboxes separate. If a step fails, stop and go to step 5.

## 1. Inputs

| Input | Local (`codex-local`) | Cloud (`codex-cloud`) |
| --- | --- | --- |
| Hub address | `http://127.0.0.1:54843/receiver` | `http://127.0.0.1:54843/codex/mcp`, reached through an official Secure MCP Tunnel |
| Token | Owner's mode-600 token file | Owner's mode-600 header file |
| Other | Binding JSON: `thread_id`, `endpoint`, `cwd`, `registration` | Tunnel ID, runtime key, Events callback host |
| Test message id | From the owner or a peer | From the owner or a peer |

Never put a token in chat, Pigeon, logs, or Git.

## 2. Owner does

- Local: enables the receiver route. Writes the receiver policy, registration, token file, and binding. Starts `pigeonmq-codex-receiver`.
- Cloud: runs `setup-codex`. Uses the hub that serves the Codex route. Connects the tunnel client to that route. Binds a platform Events subscription to the Cloud conversation.

Read references/owner-setup.md when the owner needs the exact commands.

## 3. Check the connection

Read references/check.md when you reach this step.
Cloud success: `tools/list` includes the tools listed in references/check.md. Local: the worker reports `configuration_valid`, which checks the binding only.

## 4. Set up message intake

- Local (mode D): the worker claims work and runs it on the bound native thread. No MCP polling.
- Cloud: an MCP Event nudges the conversation. The conversation then drains the inbox.

Read references/intake.md when you reach this step. Then handle the test message.

## 5. Report

Reply with ONE fenced code block and nothing else:

```text
agent: codex-local | codex-cloud
endpoint_host: <host only, or "redacted">
initialize_ok: yes | no (HTTP code)
tools_missing: none | <names>
loopback_vs_public: both ok | loopback only | both fail | not checked
intake: mode D worker | MCP Events | not set up (why)
test_message_id: <id> | none (read: yes/no, reply: yes/no)
blockers: none | <list>
unverified: <anything you could not confirm>
```

## Check the inbox (read-only)

Cloud mode only. Use `pigeonmq_inbox` with `{}` for the latest received mail. To search sent and received mail, pass `{"scope":"all","query":"build log"}`.
Use `pigeonmq_read` for a received body or `pigeonmq_read_thread` with the listed `conversation_id` for the conversation.
Keep browsing read-only. Use `pigeonmq_wait` for pending work. Reserve `pigeonmq_reply` and `pigeonmq_confirm_terminal` for actions that should acknowledge a message.
See [browse and search mail](../../docs/setup-skills.md#browse-and-search-mail) for paging.
`403` means the policy does not allow the read.

## Docs

Deeper detail: `docs/configuration.md` (Codex Cloud operation, Local Codex receiver), `docs/agents.md`.

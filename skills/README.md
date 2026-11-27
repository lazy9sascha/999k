# Set up agent inbox handling

Use the tool names returned by `tools/list`. These examples use the
`p-999k_` prefix. Release 0.2.0 advertises `radio999k_` names.

Triage ends when you choose a connection mode (A–D). The modes are defined in [architecture](../docs/architecture.md#connection-modes). The next step is to give the agent a setup skill for that type.

After triage, copy the template for the selected mode into the agent’s
instructions. Use the fenced block as the template. Ask the agent for its
setup result in one code block.

Webhook and Events connections can receive a wake. A pull-only agent needs
its own poller or a request from the user.

## Which template to use

| Agent | Template |
| --- | --- |
| Grok Bot | [A](#type-a-wake--pull) |
| Muse | [B](#type-b-pull-only) |
| Pickle | [B scripted](#type-b-scripted-variant) |
| Vooy | [B](#type-b-pull-only) (provisional) |
| Instinct | [Private runtime pairing](../docs/tunnels.md#check-the-existing-conversation) from its existing Messages conversation |

The table above is the inbox-handling template to paste after triage. Known agents also have a connection skill: [Grok](999k-grok/SKILL.md), [Muse](999k-muse/SKILL.md), [Dot](999k-dot/SKILL.md), [Codex](999k-codex/SKILL.md), [Pickle](999k-pickle/SKILL.md), [Vooy](999k-vooy/SKILL.md). For an unfamiliar agent, start with [agent triage](999k-cloud-triage/SKILL.md).

## Browse and search mail

Use `p-999k_inbox` with `{}` for the latest received mail. Use `{"query":"build log"}` to search received mail or `{"scope":"all","query":"build log"}` to also search sent mail. Follow `next_continuation` with the same filters until it is null. See the [mailbox tool contract](../docs/architecture.md#mcp-tools) for body reads and page options.

Browsing is read-only. Use `p-999k_wait` to look for pending work. `p-999k_reply` stores the reply and acknowledges the original together. `p-999k_confirm_terminal` acknowledges a terminal answer.

These skills are repository Markdown for a separate agent-side handoff. The hub advertises MCP tools and has no MCP Resources catalog. Update the copied or installed skill through the agent's own mechanism. The [app upgrade checklist](../docs/troubleshooting.md#refresh-a-saved-mcp-app) covers refreshing the MCP tool catalog.

## Type B (pull only)

Use this when the agent can call the hub but has no webhook. The hub cannot start a run. The agent keeps its own scheduler in storage that survives a machine replace. Use a home or workspace path. Never use wiped system paths such as `/etc` or `/usr/bin`.
On each tick, drain the inbox with `p-999k_wait`. Reply once to each message. If a connection drops, retry it once. If the inbox is empty, stay quiet.

Copy the block below. For a scheduler that checks for work without starting a model run, use the [scripted variant](#type-b-scripted-variant) instead.

```text
---
name: 999k-inbox
description: Poll the 999K inbox, handle pending mail, and reply once per message.
---

# 999K inbox (pull only)

The hub cannot wake you. You must poll.

1. Keep a scheduler in persistent storage, not in paths wiped on replace; restore it after a replace if needed.
2. Each run, call `p-999k_wait` (timeout_ms <= 10000) until nothing is pending. For each message, read it, act within your normal rules, then `p-999k_reply` once on that message_id. Call the mailbox tools on demand; do not copy tool docs into this skill.
3. If the connection drops, retry that call once; if it still fails, stop and report. If nothing is pending, end quietly. Put no secrets in a message body or in this skill. Outbound send/reply still need the user's normal approval.

## Report setup in one code block
- skill path saved
- scheduler line or watch script path, and the interval
- one test run: number of messages handled, or "none pending"
```

### Type B scripted variant

Some agents (Pickle) can run a condition script on a schedule without waking the model, and start a full run only when work exists. The script belongs in persistent storage and must not hold secrets. When the model does run, it drains the inbox the same way as type B.

```text
---
name: 999k-inbox
description: When a scheduled check finds 999K work, drain the inbox and reply once per message.
---

# 999K inbox (scripted pull)

The hub cannot wake you. A scheduler runs a condition check that must not start a model run by itself.

1. Keep the scheduler and condition script in persistent storage. Exit success from the script only when work is pending.
2. When a full run starts, call `p-999k_wait` (timeout_ms <= 10000) until nothing is pending. For each message, read it, act within your normal rules, then `p-999k_reply` once on that message_id. Call the mailbox tools on demand; do not copy tool docs into this skill.
3. If the connection drops, retry that call once; if it still fails, stop and report. If nothing is pending, end quietly. Put no secrets in a message body, script, or this skill.

## Report setup in one code block
- skill path and condition script path saved
- scheduler line and the interval
- one test run: number of messages handled, or "none pending"
```

## Type A (wake + pull)

Use this template for a webhook or another trigger that can start a run.
The hub keeps the request queued if a wake is lost. Use a fallback poll
if the platform supports one. For GrokBot, configure the
[webhook wake](../docs/quickstart.md#5-optional-grok-bot-wake) first.

```text
---
name: 999k-inbox
description: On a 999K wake, drain the inbox, handle pending mail, and reply once per message.
---

# 999K inbox (wake + pull)

A webhook or trigger is only a nudge. It may carry a message id, never secrets. Then drain the inbox.

1. Call `p-999k_wait` (timeout_ms <= 10000) until nothing is pending. For each message, read it, act within your normal rules, then `p-999k_reply` once on that message_id. Call the mailbox tools on demand; do not copy tool docs into this skill.
2. If the connection drops, retry that call once; if it still fails, stop and report. If nothing is pending, end quietly.
3. If the platform allows it, keep a fallback poll in persistent storage. Put no secrets in a message body or in this skill. Outbound send/reply still need the user's normal approval.

## Report setup in one code block
- skill path saved
- webhook or trigger setup (no secrets), and any fallback poll
- one test run: number of messages handled, or "none pending"
```

## Type C (Events)

Use this template for Codex Cloud or Dot with a verified MCP Events
subscription. When an event arrives, read the message from the hub.
[Connection evidence for each agent](../docs/agents.md#deployment-matrix)

```text
---
name: 999k-inbox
description: On a 999K event, drain the inbox, handle pending mail, and reply once per message.
---

# 999K inbox (Events)

A platform event is only a nudge, not the message body. Then drain the inbox.

1. Call `p-999k_wait` (timeout_ms <= 10000) until nothing is pending. For each message, read it, act within your normal rules, then `p-999k_reply` once on that message_id. Call the mailbox tools on demand; do not copy tool docs into this skill.
2. If the connection drops, retry that call once; if it still fails, stop and report. If nothing is pending, end quietly.
3. Put no secrets in a message body or in this skill. Outbound send/reply still need the user's normal approval.

## Report setup in one code block
- skill path saved
- event subscription (no secrets)
- one test run: number of messages handled, or "none pending"
```

## Type D (Mac desktop thread)

This repo implements the hub-side path for Codex Local: a native worker claims through the common receiver API and a bound desktop thread. An agent-facing setup skill for that thread is not defined yet.

> **Planned design.** No copy-paste setup skill for type D in this release.

## Instinct

Use the existing Messages conversation and its secure setup form. In the verified flow the private runtime redeemed a single-use code over HTTPS, then used MCP to read the original and post a linked reply. [Peer setup](../docs/tunnels.md#check-the-existing-conversation)

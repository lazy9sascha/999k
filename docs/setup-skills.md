# Setup skills per adapter type

Triage ends when you choose a connection mode (A–D). The modes are defined in [architecture](architecture.md#connection-modes). The next step is to give the agent a setup skill for that type.

**Flow:** triage prompt → capability row → adapter type → paste that type's setup skill into the agent → the agent reports back in ONE fenced code block.

Keep agent instruction files, skills, and MCP tool descriptions small. Load details on demand. The templates tell the agent to call pigeonmq tools on demand. Do not paste tool docs into a skill. The notes below are for you. The fenced blocks are what you copy to the agent.

Only agents with a webhook can be woken. The hub never wakes a polling agent. Type B must poll itself.

> **Planned design.** This handoff is a manual checklist, like [agent triage](architecture.md#agent-triage). No code pastes or installs these skills yet.

## Which template to use

| Agent | Template |
| --- | --- |
| Grok Bot | [A](#type-a-wake-pull) |
| Muse | [B](#type-b-pull-only) |
| Pickle | [B scripted](#type-b-scripted-variant) |
| Vooy | [B](#type-b-pull-only) (provisional) |
| Instinct | none ([relay, planned](#no-matching-type)) |

The table above is the inbox-handling template to paste after triage. Known agents also have a connect skill under `skills/`: [Grok](../skills/pigeonmq-grok/SKILL.md), [Muse](../skills/pigeonmq-muse/SKILL.md), [Dot](../skills/pigeonmq-dot/SKILL.md), [Codex](../skills/pigeonmq-codex/SKILL.md), [Pickle](../skills/pigeonmq-pickle/SKILL.md), [Vooy](../skills/pigeonmq-vooy/SKILL.md). For an unfamiliar agent, start with [agent triage](../skills/pigeonmq-cloud-triage/SKILL.md).

## Type B (pull only)

Use this when the agent can call the hub but has no webhook. The hub cannot start a run. The agent keeps its own scheduler in storage that survives a machine replace. Use a home or workspace path. Never use wiped system paths such as `/etc` or `/usr/bin`.
On each tick, drain the inbox with `pigeonmq_wait`. Reply once to each message. If a connection drops, retry it once. If the inbox is empty, stay quiet.

Copy the block below. For a scheduler that checks for work without starting a model run, use the [scripted variant](#type-b-scripted-variant) instead.

```text
---
name: pigeonmq-inbox
description: Poll the PigeonMQ inbox, handle pending mail, and reply once per message.
---

# PigeonMQ inbox (pull only)

The hub cannot wake you. You must poll.

1. Keep a scheduler in persistent storage, not in paths wiped on replace; restore it after a replace if needed.
2. Each run, call `pigeonmq_wait` (timeout_ms <= 10000) until nothing is pending. For each message, read it, act within your normal rules, then `pigeonmq_reply` once on that message_id. Call pigeonmq tools on demand; do not copy tool docs into this skill.
3. If the connection drops, retry that call once; if it still fails, stop and report. If nothing is pending, end quietly. Put no secrets in a message body or in this skill. Outbound send/reply still need the user's normal approval.

## Report after setup (ONE message, ONE code block)
- skill path saved
- scheduler line or watch script path, and the interval
- one test run: number of messages handled, or "none pending"
```

### Type B scripted variant

Some agents (Pickle) can run a condition script on a schedule without waking the model, and start a full run only when work exists. The script belongs in persistent storage and must not hold secrets. When the model does run, it drains the inbox the same way as type B.

```text
---
name: pigeonmq-inbox
description: When a scheduled check finds PigeonMQ work, drain the inbox and reply once per message.
---

# PigeonMQ inbox (scripted pull)

The hub cannot wake you. A scheduler runs a condition check that must not start a model run by itself.

1. Keep the scheduler and condition script in persistent storage. Exit success from the script only when work is pending.
2. When a full run starts, call `pigeonmq_wait` (timeout_ms <= 10000) until nothing is pending. For each message, read it, act within your normal rules, then `pigeonmq_reply` once on that message_id. Call pigeonmq tools on demand; do not copy tool docs into this skill.
3. If the connection drops, retry that call once; if it still fails, stop and report. If nothing is pending, end quietly. Put no secrets in a message body, script, or this skill.

## Report after setup (ONE message, ONE code block)
- skill path and condition script path saved
- scheduler line and the interval
- one test run: number of messages handled, or "none pending"
```

## Type A (wake + pull)

Use this when an inbound webhook or trigger can start a run. The wake may carry a message id, never secrets or message text. The agent must still drain the inbox itself. A lost wake loses nothing if a fallback poll exists. This repo implements Grok Bot wake on the hub side. The block below is the agent-side skill.

```text
---
name: pigeonmq-inbox
description: On a PigeonMQ wake, drain the inbox, handle pending mail, and reply once per message.
---

# PigeonMQ inbox (wake + pull)

A webhook or trigger is only a nudge. It may carry a message id, never secrets. Then drain the inbox.

1. Call `pigeonmq_wait` (timeout_ms <= 10000) until nothing is pending. For each message, read it, act within your normal rules, then `pigeonmq_reply` once on that message_id. Call pigeonmq tools on demand; do not copy tool docs into this skill.
2. If the connection drops, retry that call once; if it still fails, stop and report. If nothing is pending, end quietly.
3. If the platform allows it, keep a fallback poll in persistent storage. Put no secrets in a message body or in this skill. Outbound send/reply still need the user's normal approval.

## Report after setup (ONE message, ONE code block)
- skill path saved
- webhook or trigger setup (no secrets), and any fallback poll
- one test run: number of messages handled, or "none pending"
```

## Type C (Events)

This repo implements MCP Events delivery for Codex Cloud. When the platform supports event subscriptions, the hub pushes an event and the agent then calls the hub. Treat the event as a nudge, not as the message body, and drain the inbox the same way as type A.

```text
---
name: pigeonmq-inbox
description: On a PigeonMQ event, drain the inbox, handle pending mail, and reply once per message.
---

# PigeonMQ inbox (Events)

A platform event is only a nudge, not the message body. Then drain the inbox.

1. Call `pigeonmq_wait` (timeout_ms <= 10000) until nothing is pending. For each message, read it, act within your normal rules, then `pigeonmq_reply` once on that message_id. Call pigeonmq tools on demand; do not copy tool docs into this skill.
2. If the connection drops, retry that call once; if it still fails, stop and report. If nothing is pending, end quietly.
3. Put no secrets in a message body or in this skill. Outbound send/reply still need the user's normal approval.

## Report after setup (ONE message, ONE code block)
- skill path saved
- event subscription (no secrets)
- one test run: number of messages handled, or "none pending"
```

## Type D (Mac desktop thread)

This repo implements the hub-side path for Codex Local: a native worker claims through the common receiver API and a bound desktop thread. An agent-facing setup skill for that thread is not defined yet.

> **Planned design.** No copy-paste setup skill for type D in this release.

## No matching type

Instinct has no MCP client and no webhook, so there is no setup skill.

> **Planned design.** Relay over email or Slack.
